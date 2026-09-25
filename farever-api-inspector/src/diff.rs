use crate::model::{
    ApiSnapshot, BindingDefinition, CallableDefinition, ChangeKind, DiffChange, DiffReport,
    DiffSummary, EnumVariantDefinition, FieldDefinition, MethodDefinition, TypeDefinition,
    SNAPSHOT_SCHEMA_VERSION,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

#[derive(Debug, thiserror::Error)]
pub enum DiffError {
    #[error("unsupported old snapshot schema {0}; expected {SNAPSHOT_SCHEMA_VERSION}")]
    UnsupportedOldSchema(u32),
    #[error("unsupported new snapshot schema {0}; expected {SNAPSHOT_SCHEMA_VERSION}")]
    UnsupportedNewSchema(u32),
    #[error(
        "snapshots use different extraction selections; compare snapshots created with matching focused filters or --all"
    )]
    IncompatibleSelection,
}

pub fn diff_snapshots(old: &ApiSnapshot, new: &ApiSnapshot) -> Result<DiffReport, DiffError> {
    if old.schema_version != SNAPSHOT_SCHEMA_VERSION {
        return Err(DiffError::UnsupportedOldSchema(old.schema_version));
    }
    if new.schema_version != SNAPSHOT_SCHEMA_VERSION {
        return Err(DiffError::UnsupportedNewSchema(new.schema_version));
    }
    if old.scope.selection != new.scope.selection {
        return Err(DiffError::IncompatibleSelection);
    }

    let mut changes = Vec::new();
    diff_types(&mut changes, &old.types, &new.types);
    diff_globals(&mut changes, old, new);
    diff_callables(&mut changes, &old.callables, &new.callables);
    changes.sort_by(|left, right| {
        left.entity
            .cmp(&right.entity)
            .then(left.id.cmp(&right.id))
            .then(change_order(left.change).cmp(&change_order(right.change)))
    });

    let mut summary = DiffSummary::default();
    for change in &changes {
        match change.change {
            ChangeKind::Added => summary.added += 1,
            ChangeKind::Removed => summary.removed += 1,
            ChangeKind::Modified => summary.modified += 1,
        }
    }

    Ok(DiffReport {
        schema_version: SNAPSHOT_SCHEMA_VERSION,
        old_source_sha256: old.source.hlboot.sha256.clone(),
        new_source_sha256: new.source.hlboot.sha256.clone(),
        old_build_id: old
            .source
            .steam
            .as_ref()
            .and_then(|steam| steam.build_id.clone()),
        new_build_id: new
            .source
            .steam
            .as_ref()
            .and_then(|steam| steam.build_id.clone()),
        summary,
        changes,
    })
}

pub fn render_text_diff(report: &DiffReport) -> String {
    let mut output = String::new();
    writeln!(output, "Farever internal API diff").expect("writing to String cannot fail");
    writeln!(
        output,
        "old: {}{}",
        report.old_source_sha256,
        build_suffix(report.old_build_id.as_deref())
    )
    .expect("writing to String cannot fail");
    writeln!(
        output,
        "new: {}{}",
        report.new_source_sha256,
        build_suffix(report.new_build_id.as_deref())
    )
    .expect("writing to String cannot fail");
    writeln!(
        output,
        "changes: {} added, {} removed, {} modified",
        report.summary.added, report.summary.removed, report.summary.modified
    )
    .expect("writing to String cannot fail");

    if report.changes.is_empty() {
        output.push_str("No semantic API changes.\n");
        return output;
    }

    for change in &report.changes {
        let marker = match change.change {
            ChangeKind::Added => '+',
            ChangeKind::Removed => '-',
            ChangeKind::Modified => '~',
        };
        writeln!(output, "{marker} {} {}", change.entity, change.id)
            .expect("writing to String cannot fail");
        for detail in &change.details {
            writeln!(output, "    {detail}").expect("writing to String cannot fail");
        }
    }
    output
}

fn build_suffix(build_id: Option<&str>) -> String {
    build_id
        .map(|build_id| format!(" (Steam build {build_id})"))
        .unwrap_or_default()
}

fn diff_types(changes: &mut Vec<DiffChange>, old: &[TypeDefinition], new: &[TypeDefinition]) {
    let old_by_id = old
        .iter()
        .map(|ty| (ty.id.as_str(), ty))
        .collect::<BTreeMap<_, _>>();
    let new_by_id = new
        .iter()
        .map(|ty| (ty.id.as_str(), ty))
        .collect::<BTreeMap<_, _>>();
    for id in union_keys(&old_by_id, &new_by_id) {
        match (old_by_id.get(id), new_by_id.get(id)) {
            (None, Some(_)) => push(changes, ChangeKind::Added, "type", id, Vec::new()),
            (Some(_), None) => push(changes, ChangeKind::Removed, "type", id, Vec::new()),
            (Some(old), Some(new)) => diff_type(changes, id, old, new),
            (None, None) => unreachable!(),
        }
    }
}

fn diff_type(changes: &mut Vec<DiffChange>, id: &str, old: &TypeDefinition, new: &TypeDefinition) {
    let mut details = Vec::new();
    if old.kind != new.kind {
        details.push(format!("kind: {:?} -> {:?}", old.kind, new.kind));
    }
    if old.name != new.name {
        details.push(format!("name: {} -> {}", old.name, new.name));
    }
    if old.super_type != new.super_type {
        details.push(format!(
            "super: {} -> {}",
            option_text(old.super_type.as_deref()),
            option_text(new.super_type.as_deref())
        ));
    }
    if !details.is_empty() {
        push(changes, ChangeKind::Modified, "type", id, details);
    }

    diff_fields(changes, id, &old.fields, &new.fields);
    diff_methods(changes, id, &old.methods, &new.methods);
    diff_bindings(changes, id, &old.bindings, &new.bindings);
    diff_variants(changes, id, &old.variants, &new.variants);
}

fn diff_fields(
    changes: &mut Vec<DiffChange>,
    owner: &str,
    old: &[FieldDefinition],
    new: &[FieldDefinition],
) {
    let old_by_key = keyed(old, |field| field.name.clone());
    let new_by_key = keyed(new, |field| field.name.clone());
    for key in union_keys(&old_by_key, &new_by_key) {
        let id = format!("{owner}::{key}");
        match (old_by_key.get(key), new_by_key.get(key)) {
            (None, Some(_)) => push(changes, ChangeKind::Added, "field", &id, Vec::new()),
            (Some(_), None) => push(changes, ChangeKind::Removed, "field", &id, Vec::new()),
            (Some(old), Some(new)) => {
                let mut details = Vec::new();
                if old.r#type != new.r#type {
                    details.push(format!("type: {} -> {}", old.r#type, new.r#type));
                }
                if !details.is_empty() {
                    push(changes, ChangeKind::Modified, "field", &id, details);
                }
            }
            (None, None) => unreachable!(),
        }
    }
}

fn diff_methods(
    changes: &mut Vec<DiffChange>,
    owner: &str,
    old: &[MethodDefinition],
    new: &[MethodDefinition],
) {
    let old_by_key = keyed(old, |method| method.name.clone());
    let new_by_key = keyed(new, |method| method.name.clone());
    for key in union_keys(&old_by_key, &new_by_key) {
        let id = format!("{owner}::{key}");
        match (old_by_key.get(key), new_by_key.get(key)) {
            (None, Some(_)) => push(changes, ChangeKind::Added, "method", &id, Vec::new()),
            (Some(_), None) => push(changes, ChangeKind::Removed, "method", &id, Vec::new()),
            (Some(old), Some(new)) => {
                let details = signature_details(
                    &old.arguments,
                    &old.return_type,
                    &new.arguments,
                    &new.return_type,
                );
                if !details.is_empty() {
                    push(changes, ChangeKind::Modified, "method", &id, details);
                }
            }
            (None, None) => unreachable!(),
        }
    }
}

fn diff_bindings(
    changes: &mut Vec<DiffChange>,
    owner: &str,
    old: &[BindingDefinition],
    new: &[BindingDefinition],
) {
    let old_by_key = keyed(old, |binding| binding.field_name.clone());
    let new_by_key = keyed(new, |binding| binding.field_name.clone());
    for key in union_keys(&old_by_key, &new_by_key) {
        let id = format!("{owner}::{key}");
        match (old_by_key.get(key), new_by_key.get(key)) {
            (None, Some(_)) => push(changes, ChangeKind::Added, "binding", &id, Vec::new()),
            (Some(_), None) => push(changes, ChangeKind::Removed, "binding", &id, Vec::new()),
            (Some(old), Some(new)) => {
                let details = signature_details(
                    &old.arguments,
                    &old.return_type,
                    &new.arguments,
                    &new.return_type,
                );
                if !details.is_empty() {
                    push(changes, ChangeKind::Modified, "binding", &id, details);
                }
            }
            (None, None) => unreachable!(),
        }
    }
}

fn diff_variants(
    changes: &mut Vec<DiffChange>,
    owner: &str,
    old: &[EnumVariantDefinition],
    new: &[EnumVariantDefinition],
) {
    let old_by_key = keyed(old, |variant| variant.name.clone());
    let new_by_key = keyed(new, |variant| variant.name.clone());
    for key in union_keys(&old_by_key, &new_by_key) {
        let id = format!("{owner}::{key}");
        match (old_by_key.get(key), new_by_key.get(key)) {
            (None, Some(_)) => push(changes, ChangeKind::Added, "enum-variant", &id, Vec::new()),
            (Some(_), None) => push(
                changes,
                ChangeKind::Removed,
                "enum-variant",
                &id,
                Vec::new(),
            ),
            (Some(old), Some(new)) if old.parameters != new.parameters => push(
                changes,
                ChangeKind::Modified,
                "enum-variant",
                &id,
                vec![format!(
                    "parameters: ({}) -> ({})",
                    old.parameters.join(", "),
                    new.parameters.join(", ")
                )],
            ),
            _ => {}
        }
    }
}

fn diff_globals(changes: &mut Vec<DiffChange>, old: &ApiSnapshot, new: &ApiSnapshot) {
    let counts = |snapshot: &ApiSnapshot| {
        let mut result = BTreeMap::<String, usize>::new();
        for global in &snapshot.globals {
            *result.entry(global.r#type.clone()).or_default() += 1;
        }
        result
    };
    let old_counts = counts(old);
    let new_counts = counts(new);
    for ty in union_keys(&old_counts, &new_counts) {
        let old_count = old_counts.get(ty).copied().unwrap_or_default();
        let new_count = new_counts.get(ty).copied().unwrap_or_default();
        if old_count != new_count {
            let change = if old_count == 0 {
                ChangeKind::Added
            } else if new_count == 0 {
                ChangeKind::Removed
            } else {
                ChangeKind::Modified
            };
            push(
                changes,
                change,
                "global",
                ty,
                vec![format!("count: {old_count} -> {new_count}")],
            );
        }
    }
}

fn diff_callables(
    changes: &mut Vec<DiffChange>,
    old: &[CallableDefinition],
    new: &[CallableDefinition],
) {
    // Object-owned functions are already represented as methods or field bindings.
    let old_by_id = old
        .iter()
        .filter(|callable| callable.owner.is_none())
        .map(|callable| (callable.id.as_str(), callable))
        .collect::<BTreeMap<_, _>>();
    let new_by_id = new
        .iter()
        .filter(|callable| callable.owner.is_none())
        .map(|callable| (callable.id.as_str(), callable))
        .collect::<BTreeMap<_, _>>();
    for id in union_keys(&old_by_id, &new_by_id) {
        match (old_by_id.get(id), new_by_id.get(id)) {
            (None, Some(_)) => push(changes, ChangeKind::Added, "callable", id, Vec::new()),
            (Some(_), None) => push(changes, ChangeKind::Removed, "callable", id, Vec::new()),
            (Some(old), Some(new)) => {
                let details = signature_details(
                    &old.arguments,
                    &old.return_type,
                    &new.arguments,
                    &new.return_type,
                );
                if !details.is_empty() {
                    push(changes, ChangeKind::Modified, "callable", id, details);
                }
            }
            (None, None) => unreachable!(),
        }
    }
}

fn signature_details(
    old_arguments: &[String],
    old_return: &str,
    new_arguments: &[String],
    new_return: &str,
) -> Vec<String> {
    let mut details = Vec::new();
    if old_arguments != new_arguments {
        details.push(format!(
            "arguments: ({}) -> ({})",
            old_arguments.join(", "),
            new_arguments.join(", ")
        ));
    }
    if old_return != new_return {
        details.push(format!("return: {old_return} -> {new_return}"));
    }
    details
}

fn keyed<T>(items: &[T], base: impl Fn(&T) -> String) -> BTreeMap<String, &T> {
    let mut totals = BTreeMap::<String, usize>::new();
    for item in items {
        *totals.entry(base(item)).or_default() += 1;
    }
    let mut seen = BTreeMap::<String, usize>::new();
    let mut result = BTreeMap::new();
    for item in items {
        let key = base(item);
        let key = if totals[&key] > 1 {
            let ordinal = seen.entry(key.clone()).or_default();
            *ordinal += 1;
            format!("{key}#{}", ordinal)
        } else {
            key
        };
        result.insert(key, item);
    }
    result
}

fn union_keys<'a, V>(
    old: &'a BTreeMap<impl AsRef<str> + Ord, V>,
    new: &'a BTreeMap<impl AsRef<str> + Ord, V>,
) -> Vec<&'a str> {
    old.keys()
        .map(AsRef::as_ref)
        .chain(new.keys().map(AsRef::as_ref))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn push(
    changes: &mut Vec<DiffChange>,
    change: ChangeKind,
    entity: &str,
    id: &str,
    details: Vec<String>,
) {
    changes.push(DiffChange {
        change,
        entity: entity.to_owned(),
        id: id.to_owned(),
        details,
    });
}

fn option_text(value: Option<&str>) -> &str {
    value.unwrap_or("<none>")
}

fn change_order(change: ChangeKind) -> u8 {
    match change {
        ChangeKind::Removed => 0,
        ChangeKind::Added => 1,
        ChangeKind::Modified => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        FileFingerprint, SnapshotScope, SnapshotSelection, SnapshotSummary, SourceMetadata,
        TypeKind,
    };

    fn snapshot(hash: &str, fields: Vec<FieldDefinition>) -> ApiSnapshot {
        ApiSnapshot {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            tool_version: "test".to_owned(),
            source: SourceMetadata {
                bytecode_version: 4,
                hlboot: FileFingerprint {
                    file_name: "hlboot.dat".to_owned(),
                    size: 1,
                    sha256: hash.to_owned(),
                },
                farever_exe: None,
                libhl: None,
                steam: None,
                extracted_at_unix_seconds: 0,
            },
            scope: SnapshotScope::new(SnapshotSelection {
                mode: "focused".to_owned(),
                namespace_prefixes: vec!["ent.".to_owned()],
                root_types: Vec::new(),
                includes_direct_references: true,
            }),
            summary: SnapshotSummary::default(),
            types: vec![TypeDefinition {
                id: "object:ent.Hero".to_owned(),
                kind: TypeKind::Object,
                name: "ent.Hero".to_owned(),
                type_index: 100,
                super_type: Some("ent.Unit".to_owned()),
                global_index: Some(2),
                inherited_field_count: 0,
                fields,
                methods: Vec::new(),
                bindings: Vec::new(),
                variants: Vec::new(),
            }],
            globals: Vec::new(),
            callables: Vec::new(),
        }
    }

    #[test]
    fn ignores_raw_type_and_global_index_churn() {
        let old = snapshot("old", Vec::new());
        let mut new = snapshot("new", Vec::new());
        new.types[0].type_index = 900;
        new.types[0].global_index = Some(42);
        let report = diff_snapshots(&old, &new).unwrap();
        assert_eq!(report.summary.total(), 0);
    }

    #[test]
    fn reports_field_type_changes_but_ignores_slot_churn() {
        let field = |slot, ty: &str| FieldDefinition {
            slot,
            name: "health".to_owned(),
            r#type: ty.to_owned(),
        };
        let old = snapshot("old", vec![field(3, "i32")]);
        let new = snapshot("new", vec![field(4, "f64")]);
        let report = diff_snapshots(&old, &new).unwrap();
        assert_eq!(report.summary.modified, 1);
        assert_eq!(report.changes[0].entity, "field");
        assert!(report.changes[0]
            .details
            .iter()
            .any(|line| line.contains("i32 -> f64")));
        assert_eq!(report.changes[0].details.len(), 1);
    }

    #[test]
    fn rejects_snapshots_with_different_selections() {
        let old = snapshot("old", Vec::new());
        let mut new = snapshot("new", Vec::new());
        new.scope.selection.includes_direct_references = false;
        assert!(matches!(
            diff_snapshots(&old, &new),
            Err(DiffError::IncompatibleSelection)
        ));
    }
}

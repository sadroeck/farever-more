//! Host compatibility checks share the offline inspector's bytecode parser.

use crate::extract::{compatibility_types, validate_hlboot_header};
use crate::model::{EnumVariantDefinition, TypeDefinition};
use hlbc::opcodes::Opcode;
use hlbc::Bytecode;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CompatibilityContract {
    pub schema_version: u32,
    pub baseline_sha256: String,
    pub types: Vec<RequiredType>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RequiredType {
    pub name: String,
    pub kind: String,
    pub super_type: Option<String>,
    pub fields: BTreeMap<String, String>,
    pub methods: BTreeMap<String, RequiredSignature>,
    pub bindings: BTreeMap<String, RequiredSignature>,
    pub variants: Vec<EnumVariantDefinition>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RequiredSignature {
    pub arguments: Vec<String>,
    pub return_type: String,
}

/// Validate before installing even the allocator hook. Indexes and unrelated
/// members may change; required members and ordered enum payloads may not.
pub fn verify_bytecode_contract(
    path: &Path,
    contract: &CompatibilityContract,
    playable_state: i32,
) -> Result<(), String> {
    if contract.schema_version != 1 || contract.types.is_empty() {
        return Err("invalid embedded game compatibility contract".to_owned());
    }
    let size = std::fs::metadata(path)
        .map_err(|error| format!("read {}: {error}", path.display()))?
        .len();
    if size > 128 * 1024 * 1024 {
        return Err("game bytecode exceeds the 128 MiB inspection limit".to_owned());
    }
    validate_hlboot_header(path).map_err(|error| error.to_string())?;
    // The vendored parser can panic on corrupt cross references. Treat that as
    // a refused load, rather than losing the worker and leaving startup waiting.
    std::panic::catch_unwind(|| {
        let code = Bytecode::from_file(path)
            .map_err(|error| format!("parse {}: {error}", path.display()))?;
        let names = contract.types.iter().map(|ty| ty.name.as_str()).collect();
        let types = compatibility_types(&code, &names);
        verify_types(&types, contract)?;
        verify_playable_state(&code, &types, playable_state)
    })
    .map_err(|_| "invalid game bytecode metadata references".to_owned())?
}

fn verify_types(types: &[TypeDefinition], contract: &CompatibilityContract) -> Result<(), String> {
    let mut by_name = HashMap::new();
    for actual in types {
        by_name
            .entry(actual.name.as_str())
            .or_insert_with(Vec::new)
            .push(actual);
    }
    for required in &contract.types {
        let candidates = by_name
            .get(required.name.as_str())
            .ok_or_else(|| format!("missing game type {}", required.name))?;
        if candidates.len() != 1 {
            return Err(format!("ambiguous game type {}", required.name));
        }
        let actual = candidates[0];
        let kind = serde_json::to_value(actual.kind).map_err(|error| error.to_string())?;
        if kind.as_str() != Some(required.kind.as_str()) || actual.super_type != required.super_type
        {
            return Err(format!(
                "game type {} changed kind or inheritance",
                required.name
            ));
        }
        for (name, expected) in &required.fields {
            let fields = actual
                .fields
                .iter()
                .filter(|field| &field.name == name)
                .collect::<Vec<_>>();
            if fields.len() != 1 || &fields[0].r#type != expected {
                return Err(format!(
                    "game field {}.{name} must have type {expected}",
                    required.name
                ));
            }
        }
        for (name, expected) in &required.methods {
            let methods = actual
                .methods
                .iter()
                .filter(|method| &method.name == name)
                .collect::<Vec<_>>();
            if methods.len() != 1
                || methods[0].arguments != expected.arguments
                || methods[0].return_type != expected.return_type
            {
                return Err(format!(
                    "game method {}.{name} changed signature or is missing",
                    required.name
                ));
            }
        }
        for (name, expected) in &required.bindings {
            let bindings = actual
                .bindings
                .iter()
                .filter(|binding| &binding.field_name == name)
                .collect::<Vec<_>>();
            if bindings.len() != 1
                || bindings[0].arguments != expected.arguments
                || bindings[0].return_type != expected.return_type
            {
                return Err(format!(
                    "game binding {}.{name} changed signature or is missing",
                    required.name
                ));
            }
        }
        if actual.variants != required.variants {
            return Err(format!(
                "game enum {} changed constructors or payloads",
                required.name
            ));
        }
    }
    Ok(())
}

fn verify_playable_state(
    code: &Bytecode,
    types: &[TypeDefinition],
    state: i32,
) -> Result<(), String> {
    let app = types
        .iter()
        .find(|ty| ty.name == "GameApp")
        .ok_or("missing GameApp loading-state contract")?;
    let setter = app
        .methods
        .iter()
        .find(|method| method.name == "set_loadingState")
        .ok_or("missing GameApp.set_loadingState")?;
    let finished = app
        .methods
        .iter()
        .find(|method| method.name == "finishedLoading")
        .ok_or("missing GameApp.finishedLoading")?;
    let function = code
        .functions
        .iter()
        .find(|function| function.findex.0 == finished.findex)
        .ok_or("missing GameApp.finishedLoading bytecode")?;
    verify_loading_call(&function.ops, &code.ints, setter.findex, state)
}

fn verify_loading_call(
    ops: &[Opcode],
    ints: &[i32],
    setter: usize,
    state: i32,
) -> Result<(), String> {
    let calls = ops
        .iter()
        .filter(|op| matches!(op, Opcode::Call2 { fun, .. } if fun.0 == setter))
        .count();
    let expected = ops.windows(2).any(|pair| match (&pair[0], &pair[1]) {
        (
            Opcode::Int { dst, ptr },
            Opcode::Call2 {
                fun, arg0, arg1, ..
            },
        ) => fun.0 == setter && arg0.0 == 0 && arg1 == dst && ints.get(ptr.0) == Some(&state),
        _ => false,
    });
    if calls != 1 || !expected {
        return Err(format!(
            "GameApp.finishedLoading no longer sets playable loading state {state}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FieldDefinition, MethodDefinition, TypeKind};
    use hlbc::types::{RefFun, RefInt, Reg};

    fn fixture() -> (Vec<TypeDefinition>, CompatibilityContract) {
        let actual = TypeDefinition {
            id: "object:GameApp".into(),
            kind: TypeKind::Object,
            name: "GameApp".into(),
            type_index: 123,
            super_type: Some("App".into()),
            global_index: Some(19),
            inherited_field_count: 4,
            fields: vec![FieldDefinition {
                slot: 4,
                name: "loadingState".into(),
                r#type: "i32".into(),
            }],
            methods: vec![MethodDefinition {
                name: "finishedLoading".into(),
                findex: 25,
                prototype_index: 5,
                arguments: vec!["GameApp".into()],
                return_type: "void".into(),
            }],
            bindings: vec![],
            variants: vec![],
        };
        let contract = CompatibilityContract {
            schema_version: 1,
            baseline_sha256: "fixture".into(),
            types: vec![RequiredType {
                name: "GameApp".into(),
                kind: "object".into(),
                super_type: Some("App".into()),
                fields: BTreeMap::from([("loadingState".into(), "i32".into())]),
                methods: BTreeMap::from([(
                    "finishedLoading".into(),
                    RequiredSignature {
                        arguments: vec!["GameApp".into()],
                        return_type: "void".into(),
                    },
                )]),
                bindings: BTreeMap::new(),
                variants: vec![],
            }],
        };
        (vec![actual], contract)
    }

    #[test]
    fn unrelated_additions_and_index_churn_are_compatible() {
        let (mut types, contract) = fixture();
        types[0].type_index += 30;
        types[0].fields[0].slot += 9;
        types[0].methods[0].findex += 500;
        types[0].fields.push(FieldDefinition {
            slot: 5,
            name: "newField".into(),
            r#type: "bool".into(),
        });
        verify_types(&types, &contract).unwrap();
    }

    #[test]
    fn changed_host_fields_receivers_returns_and_inheritance_are_refused() {
        let (types, contract) = fixture();
        for change in 0..5 {
            let mut changed = types.clone();
            match change {
                0 => changed[0].fields[0].r#type = "f64".into(),
                1 => changed[0].methods[0].arguments[0] = "App".into(),
                2 => changed[0].methods[0].return_type = "bool".into(),
                3 => changed[0].super_type = None,
                _ => changed[0].methods.clear(),
            }
            assert!(verify_types(&changed, &contract).is_err());
        }
    }

    #[test]
    fn enum_order_payloads_and_bound_callbacks_are_checked() {
        let (mut types, mut contract) = fixture();
        let variants = vec![
            EnumVariantDefinition {
                name: "Exit".into(),
                parameters: vec![],
            },
            EnumVariantDefinition {
                name: "Switch".into(),
                parameters: vec!["String".into()],
            },
        ];
        types[0].variants = variants.clone();
        contract.types[0].variants = variants;
        verify_types(&types, &contract).unwrap();
        types[0].variants.reverse();
        assert!(verify_types(&types, &contract).is_err());
        types[0].variants.reverse();
        types[0].variants[1].parameters.clear();
        assert!(verify_types(&types, &contract).is_err());
        types[0].variants = contract.types[0].variants.clone();
        contract.types[0].bindings.insert(
            "onClickWorld".into(),
            RequiredSignature {
                arguments: vec!["GameApp".into()],
                return_type: "void".into(),
            },
        );
        assert!(verify_types(&types, &contract).is_err());
    }

    #[test]
    fn loading_state_constant_and_receiver_must_match() {
        let mut ops = vec![
            Opcode::Int {
                dst: Reg(2),
                ptr: RefInt(0),
            },
            Opcode::Call2 {
                dst: Reg(2),
                fun: RefFun(43),
                arg0: Reg(0),
                arg1: Reg(2),
            },
        ];
        verify_loading_call(&ops, &[10], 43, 10).unwrap();
        assert!(verify_loading_call(&ops, &[11], 43, 10).is_err());
        ops.push(ops[1].clone());
        assert!(verify_loading_call(&ops, &[10], 43, 10).is_err());
    }
}

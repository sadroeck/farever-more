//! Fills the inventory's placement table, and projects it into the POI add-on.
//!
//! Two verbs, because the two directions have different inputs:
//!
//! ```text
//! placements import <census.json> <assets-dir> [--check]   # release -> farever-db
//! placements project <table.rs> [--check]                  # farever-db -> add-on
//! ```
//!
//! `import` needs the released map census and is run when that release changes
//! the placements; `project` needs only the crate, and is run whenever the
//! inventory changes. Neither needs the game installation. See
//! [`farever_db::placements`] for what each direction guarantees.

use farever_db::placements;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "usage: placements import <census.json> <assets-dir> [--check]\n       \
                     placements project <table.rs> [--check]";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("placements: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut arguments = std::env::args().skip(1);
    let verb = arguments.next().ok_or(USAGE)?;
    let mut positional = Vec::new();
    let mut check = false;
    for argument in arguments {
        match argument.as_str() {
            "--check" => check = true,
            _ if argument.starts_with("--") => return Err(format!("unknown option {argument}")),
            _ => positional.push(argument),
        }
    }
    match verb.as_str() {
        "import" => import(&positional, check),
        "project" => project(&positional, check),
        _ => Err(USAGE.to_owned()),
    }
}

/// Imports the released census into the inventory's placement artifact.
fn import(positional: &[String], check: bool) -> Result<(), String> {
    let [census_path, assets_directory] = positional else {
        return Err(USAGE.to_owned());
    };
    let records = placements::parse_census(
        &std::fs::read(census_path).map_err(|error| format!("read {census_path}: {error}"))?,
    )?;
    let (artifact, coverage) =
        placements::render_placements(&records).map_err(|error| error.problems().join("\n  "))?;
    println!(
        "census of {} records against {} gatherables",
        records.len(),
        farever_db::Inventory::gatherables().len()
    );
    println!("{}", coverage.summary());

    let path = PathBuf::from(assets_directory).join(artifact.file_name);
    write_or_check(&path, &artifact.contents, check)?;
    Ok(())
}

/// Projects the inventory's placements into the add-on's table.
fn project(positional: &[String], check: bool) -> Result<(), String> {
    let [table_path] = positional else {
        return Err(USAGE.to_owned());
    };
    let records = placements::build_inventory_table()?;
    let generated = placements::render_table(&records);

    if check {
        let committed = std::fs::read_to_string(table_path)
            .map_err(|error| format!("read {table_path}: {error}"))?;
        if !placements::table_is_current(&committed, &generated) {
            return Err(format!(
                "{table_path} is stale; run the projection from the inventory"
            ));
        }
        println!("{table_path} is current");
        return Ok(());
    }
    write_or_check(Path::new(table_path), &generated, false)?;
    println!("projected {} records into {table_path}", records.len());
    Ok(())
}

fn write_or_check(path: &Path, contents: &str, check: bool) -> Result<(), String> {
    if check {
        let committed = std::fs::read_to_string(path)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        if committed.replace("\r\n", "\n") != contents {
            return Err(format!("{} is stale", path.display()));
        }
        println!("{} is current", path.display());
        return Ok(());
    }
    std::fs::write(path, contents).map_err(|error| format!("write {}: {error}", path.display()))?;
    println!("wrote {}", path.display());
    Ok(())
}

//! Writes the game inventory's artifacts from a Farever installation.
//!
//! Reads `res.light.pak` → `data.cdb` through [`farever_db::cdb`], validates it
//! against the crate's whitelist, and writes the artifacts that
//! [`farever_db::Inventory`] serves. Anything the whitelist does not describe
//! fails the run instead of ending up in an artifact.
//!
//! ```text
//! extract-db <assets-dir>            # write the artifacts
//! extract-db <assets-dir> --check    # fail if any artifact is stale
//! extract-db --list-sheets           # show every sheet the game declares
//! ```
//!
//! The installation is found through `FAREVER_GAME_DIR`, the usual Steam
//! library locations, or `--game <dir>`.

use farever_db::cdb::{discover_game, GameInstall};
use farever_db::inventory;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("extract-db: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut positional = Vec::new();
    let mut check = false;
    let mut list_sheets = false;
    let mut game_directory = None;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--check" => check = true,
            "--list-sheets" => list_sheets = true,
            "--game" => {
                game_directory = Some(PathBuf::from(
                    arguments.next().ok_or("--game needs a directory")?,
                ));
            }
            _ if argument.starts_with("--") => return Err(format!("unknown option {argument}")),
            _ => positional.push(argument),
        }
    }
    let assets_directory = match (list_sheets, positional.as_slice()) {
        (true, []) => None,
        (true, _) => return Err("--list-sheets takes no arguments".to_owned()),
        (false, [directory]) => Some(directory.clone()),
        (false, _) => {
            return Err("usage: extract-db <assets-dir> [--check] [--game <dir>]".to_owned())
        }
    };

    let game = match game_directory {
        Some(directory) => GameInstall::open(directory),
        None => discover_game(),
    }
    .map_err(|error| format!("locate the Farever installation: {error}"))?;
    let document = game
        .load_cdb()
        .map_err(|error| format!("read data.cdb: {error}"))?;

    if list_sheets {
        for sheet in
            inventory::declarations(&document).map_err(|error| format!("read data.cdb: {error}"))?
        {
            println!(
                "{:<40} {:>5}  {}",
                sheet.path,
                sheet.rows,
                sheet.columns.join(", ")
            );
            if !sheet.undeclared_keys.is_empty() {
                println!(
                    "  {:<38}        data-only: {}",
                    "",
                    sheet.undeclared_keys.join(", ")
                );
            }
        }
        return Ok(());
    }
    let assets_directory = assets_directory.expect("checked above");
    let summary = game
        .summarize_cdb()
        .map_err(|error| format!("summarize data.cdb: {error}"))?;
    // The artifacts store the build id as a literal, so the one string read
    // here has to outlive the render; extraction is a one-shot process.
    let build_id: Option<&'static str> = summary
        .fingerprint
        .steam_build_id
        .as_deref()
        .map(|id| &*Box::leak(id.to_owned().into_boxed_str()));
    let build = inventory::BuildSource::new(build_id, &summary.fingerprint, summary.sheet_count);
    let artifacts =
        inventory::render(&document, &build).map_err(|error| format!("extract: {error}"))?;

    for artifact in &artifacts {
        let path = PathBuf::from(&assets_directory).join(artifact.file_name);
        if check {
            let committed = std::fs::read_to_string(&path)
                .map_err(|error| format!("read {}: {error}", path.display()))?;
            if committed.replace("\r\n", "\n") != artifact.contents {
                return Err(format!(
                    "{} is stale; run the extractor against the installed game",
                    path.display()
                ));
            }
            continue;
        }
        std::fs::write(&path, &artifact.contents)
            .map_err(|error| format!("write {}: {error}", path.display()))?;
    }

    if check {
        println!(
            "{} artifacts match the installed game data",
            artifacts.len()
        );
        return Ok(());
    }
    println!(
        "wrote {} artifacts: {} gatherables, {} zones, data.cdb checksum {:#010x}",
        artifacts.len(),
        inventory::row_count(&document, "gatherable"),
        inventory::row_count(&document, "zone"),
        build.cdb_checksum
    );
    Ok(())
}

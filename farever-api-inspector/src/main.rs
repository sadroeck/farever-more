use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use farever_api_inspector::{
    diff_snapshots, extract_game_directory, extract_hlboot, render_text_diff, ApiSnapshot,
    ExtractOptions, ExtractionSelection,
};
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Parser)]
#[command(
    name = "farever-api-inspector",
    version,
    about = "Export and diff Farever's offline HashLink interface metadata",
    after_help = "The extractor never starts, attaches to, or reads memory from Farever.",
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Export a JSON snapshot from an installed or archived hlboot.dat.
    Extract(ExtractArgs),
    /// Compare two previously exported JSON snapshots.
    Diff(DiffArgs),
}

#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("source")
        .required(true)
        .multiple(false)
        .args(["game_dir", "hlboot"])
))]
struct ExtractArgs {
    /// Read hlboot.dat and release fingerprints from a Farever install.
    #[arg(long, value_name = "DIR")]
    game_dir: Option<PathBuf>,

    /// Read one hlboot.dat directly.
    #[arg(long, value_name = "FILE")]
    hlboot: Option<PathBuf>,

    /// Optional Farever.exe fingerprint when using --hlboot.
    #[arg(long, value_name = "FILE", requires = "hlboot")]
    farever_exe: Option<PathBuf>,

    /// Optional libhl.dll fingerprint when using --hlboot.
    #[arg(long, value_name = "FILE", requires = "hlboot")]
    libhl: Option<PathBuf>,

    /// Optional Steam appmanifest_3672400.acf metadata when using --hlboot.
    #[arg(long, value_name = "FILE", requires = "hlboot")]
    steam_manifest: Option<PathBuf>,

    /// Export every bytecode definition instead of the focused add-on surface.
    #[arg(
        long,
        conflicts_with_all = ["namespaces", "root_types", "expand_fields"]
    )]
    all: bool,

    /// Add a namespace prefix to the focused export, for example `script.`.
    #[arg(long = "namespace", value_name = "PREFIX")]
    namespaces: Vec<String>,

    /// Add an exact root type to the focused export.
    #[arg(long = "root-type", value_name = "TYPE")]
    root_types: Vec<String>,

    /// Also export definitions referenced by selected fields and enum payloads.
    #[arg(long)]
    expand_fields: bool,

    /// JSON snapshot destination.
    #[arg(long, value_name = "FILE")]
    output: PathBuf,

    /// Write compact JSON rather than human-readable pretty JSON.
    #[arg(long)]
    compact: bool,
}

#[derive(Debug, Args)]
struct DiffArgs {
    /// Old JSON snapshot.
    #[arg(long, value_name = "FILE")]
    old: PathBuf,

    /// New JSON snapshot.
    #[arg(long, value_name = "FILE")]
    new: PathBuf,

    /// Diff output format.
    #[arg(long, value_enum, default_value_t = DiffFormat::Text)]
    format: DiffFormat,

    /// Write the report to a file instead of stdout.
    #[arg(long, value_name = "FILE")]
    output: Option<PathBuf>,

    /// Exit with code 2 when semantic changes exist.
    #[arg(long)]
    check: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum DiffFormat {
    Text,
    Json,
}

fn main() {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    }
}

fn run(cli: Cli) -> Result<i32, Box<dyn Error>> {
    match cli.command {
        Command::Extract(arguments) => run_extract(arguments),
        Command::Diff(arguments) => run_diff(arguments),
    }
}

fn run_extract(arguments: ExtractArgs) -> Result<i32, Box<dyn Error>> {
    let selection = if arguments.all {
        ExtractionSelection::All
    } else {
        ExtractionSelection::Focused {
            additional_namespaces: arguments.namespaces,
            additional_root_types: arguments.root_types,
            include_direct_references: arguments.expand_fields,
        }
    };
    let snapshot = if let Some(game_directory) = arguments.game_dir {
        extract_game_directory(game_directory, selection)?
    } else {
        extract_hlboot(ExtractOptions {
            hlboot_path: arguments
                .hlboot
                .expect("clap requires exactly one extraction source"),
            farever_exe_path: arguments.farever_exe,
            libhl_path: arguments.libhl,
            steam_manifest_path: arguments.steam_manifest,
            selection,
        })?
    };

    let bytes = if arguments.compact {
        serde_json::to_vec(&snapshot)?
    } else {
        serde_json::to_vec_pretty(&snapshot)?
    };
    write_output(&arguments.output, &bytes)?;
    println!("Wrote {}", arguments.output.display());
    println!("Selection: {}", snapshot.scope.selection.mode);
    println!(
        "Steam build: {}",
        snapshot
            .source
            .steam
            .as_ref()
            .and_then(|steam| steam.build_id.as_deref())
            .unwrap_or("unknown")
    );
    println!("hlboot.dat SHA-256: {}", snapshot.source.hlboot.sha256);
    println!(
        "Inventory: {} types, {} fields, {} methods, {} bindings, {} globals, {} callables",
        snapshot.summary.exported_type_definitions,
        snapshot.summary.fields,
        snapshot.summary.methods,
        snapshot.summary.bindings,
        snapshot.summary.globals,
        snapshot.summary.exported_callables
    );
    Ok(0)
}

fn run_diff(arguments: DiffArgs) -> Result<i32, Box<dyn Error>> {
    let old = read_snapshot(&arguments.old)?;
    let new = read_snapshot(&arguments.new)?;
    let report = diff_snapshots(&old, &new)?;
    let bytes = match arguments.format {
        DiffFormat::Text => render_text_diff(&report).into_bytes(),
        DiffFormat::Json => serde_json::to_vec_pretty(&report)?,
    };

    if let Some(output_path) = arguments.output {
        write_output(&output_path, &bytes)?;
        println!("Wrote {}", output_path.display());
    } else {
        print!("{}", String::from_utf8_lossy(&bytes));
        if !bytes.ends_with(b"\n") {
            println!();
        }
    }

    if arguments.check && report.summary.total() > 0 {
        Ok(2)
    } else {
        Ok(0)
    }
}

fn read_snapshot(path: &Path) -> Result<ApiSnapshot, Box<dyn Error>> {
    let bytes = fs::read(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to read {}: {error}", path.display()),
        )
    })?;
    serde_json::from_slice(&bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("failed to parse snapshot {}: {error}", path.display()),
        )
        .into()
    })
}

fn write_output(path: &Path, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, bytes).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to write {}: {error}", path.display()),
        )
        .into()
    })
}

#[cfg(test)]
mod tests {
    use super::{Cli, Command};
    use clap::Parser;

    #[test]
    fn clap_parses_focused_extract_by_default() {
        let cli = Cli::try_parse_from([
            "farever-api-inspector",
            "extract",
            "--game-dir",
            "Farever",
            "--output",
            "snapshot.json",
        ])
        .unwrap();
        let Command::Extract(arguments) = cli.command else {
            panic!("expected extract command");
        };
        assert!(!arguments.all);
        assert!(arguments.namespaces.is_empty());
        assert!(!arguments.expand_fields);
    }

    #[test]
    fn clap_rejects_all_with_a_focused_filter() {
        let result = Cli::try_parse_from([
            "farever-api-inspector",
            "extract",
            "--game-dir",
            "Farever",
            "--output",
            "snapshot.json",
            "--all",
            "--namespace",
            "script.",
        ]);
        assert!(result.is_err());
    }
}

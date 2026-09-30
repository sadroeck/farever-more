//! Offline Farever HashLink metadata extraction and semantic snapshot diffs.

mod compatibility;
mod diff;
mod extract;
mod model;

pub use compatibility::{
    verify_bytecode_contract, CompatibilityContract, RequiredSignature, RequiredType,
};
pub use diff::{diff_snapshots, render_text_diff, DiffError};
pub use extract::{extract_game_directory, extract_hlboot, ExtractError};
pub use model::{
    ApiSnapshot, ChangeKind, DiffChange, DiffReport, DiffSummary, EnumVariantDefinition,
    ExtractOptions, ExtractionSelection, SNAPSHOT_SCHEMA_VERSION,
};

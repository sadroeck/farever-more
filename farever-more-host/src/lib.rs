//! In-process DLL entry point for the Farever More runtime.

use farever_more_runtime::{run, RuntimeConfig, TargetProcess};
use std::sync::Once;

static START: Once = Once::new();

#[no_mangle]
/// Starts the runtime thread exactly once for the lifetime of the loaded DLL.
///
/// Repeated bootstrap calls are successful no-ops. Initialization runs on a
/// dedicated thread so the proxy never blocks Windows loader work.
pub extern "C" fn fas_host_start_v0() -> u32 {
    START.call_once(|| {
        std::thread::spawn(|| {
            let game_directory = std::env::current_exe()
                .ok()
                .and_then(|path| path.parent().map(ToOwned::to_owned))
                .unwrap_or_default();
            run(RuntimeConfig {
                addon_root: game_directory.join("farever-addons"),
                // Use an ordinary read handle to the exact current PID rather
                // than relying on executable-name lookup.
                target: TargetProcess::ProcessId(std::process::id()),
            });
        });
    });
    0
}

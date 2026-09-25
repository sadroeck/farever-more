use crate::backend;
use crate::config::ManagerConfig;
use crate::model::{AddonEntry, LifecycleState, ManagerSnapshot};
use crate::{AddonItem, AddonManagerWindow, AddonState};
use slint::{ComponentHandle, SharedString, Timer, TimerMode, VecModel};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

pub(crate) fn run() -> Result<(), String> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_help();
        return Ok(());
    }
    if has_headless_action(&args) {
        return run_headless(&args);
    }
    run_gui()
}

fn print_help() {
    println!(
        "Farever Add-on Manager\n\
         \n\
         GUI (default): farever-more-manager\n\
         \n\
         Headless:\n  \
         --list [--addons-dir <dir>]\n  \
         --enable <id> | --disable <id> | --remove <id> [--addons-dir <dir>]\n  \
         --install-source <git-url|folder> [--addons-dir <dir>]\n  \
         --update-runtime [--game-dir <dir>] [--proxy-dll <path>] [--host-dll <path>]\n  \
         --set-game-dir <dir>\n\
         \n\
         Add-on roots resolve from --addons-dir, FAREVER_ADDONS_DIR,\n\
         the configured game dir, Steam auto-detection,\n\
         or <manager-exe-dir>/farever-addons.\n\
         Disabling moves the add-on to .disabled-addons; removing deletes its\n\
         folder but keeps settings in config/."
    );
}

fn has_headless_action(args: &[String]) -> bool {
    args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--list"
                | "--enable"
                | "--disable"
                | "--remove"
                | "--install-archive"
                | "--update-runtime"
                | "--set-game-dir"
        )
    })
}

fn flag_value(args: &[String], flag: &str) -> Option<PathBuf> {
    args.windows(2).find_map(|window| {
        if window[0] == flag {
            Some(PathBuf::from(&window[1]))
        } else {
            None
        }
    })
}

fn flag_str<'args>(args: &'args [String], flag: &str) -> Option<&'args str> {
    args.windows(2).find_map(|window| {
        if window[0] == flag {
            Some(window[1].as_str())
        } else {
            None
        }
    })
}

fn headless_addon_root(args: &[String]) -> PathBuf {
    if let Some(dir) = flag_value(args, "--addons-dir") {
        return dir;
    }
    ManagerConfig::load().addon_root()
}

fn run_headless(args: &[String]) -> Result<(), String> {
    let addon_root = headless_addon_root(args);
    let mut acted = false;
    if let Some(query) = flag_str(args, "--enable").or(flag_str(args, "--disable")) {
        let enabled = flag_str(args, "--enable").is_some();
        let unit = backend::find_unit(&addon_root, query)
            .ok_or_else(|| format!("add-on not found: {query}"))?;
        let moved = backend::set_enabled(&addon_root, &unit, enabled)?;
        println!("{} -> {}", query, moved.display());
        acted = true;
    }
    if let Some(query) = flag_str(args, "--remove") {
        let unit = backend::find_unit(&addon_root, query)
            .ok_or_else(|| format!("add-on not found: {query}"))?;
        backend::remove_unit(&addon_root, &unit)?;
        println!("{query} removed; settings kept");
        acted = true;
    }
    if let Some(archive) = flag_value(args, "--install-archive") {
        let installed = backend::install_archive(&addon_root, &archive)?;
        for path in &installed {
            println!("installed {}", path.display());
        }
        acted = true;
    }
    if let Some(dir) = flag_str(args, "--set-game-dir") {
        let mut config = ManagerConfig::load();
        let path = PathBuf::from(dir.trim());
        if !path.is_dir() {
            return Err(format!("game folder not found: {}", path.display()));
        }
        config.game_dir = Some(path);
        let saved = config.save()?;
        println!("game location saved to {}", saved.display());
        acted = true;
    }
    if args.iter().any(|arg| arg == "--update-runtime") {
        let game_dir = flag_value(args, "--game-dir").map_or_else(
            || {
                addon_root
                    .parent()
                    .map(Path::to_path_buf)
                    .ok_or_else(|| "cannot locate the game directory".to_owned())
            },
            Ok,
        )?;
        let proxy = flag_value(args, "--proxy-dll")
            .ok_or_else(|| "missing --proxy-dll <path>".to_owned())?;
        let host =
            flag_value(args, "--host-dll").ok_or_else(|| "missing --host-dll <path>".to_owned())?;
        backend::update_runtime(&game_dir, &proxy, &host)?;
        println!("runtime updated");
        acted = true;
    }
    if args.iter().any(|arg| arg == "--list") {
        let runtime = crate::model::runtime_status(&addon_root);
        if !runtime.installed {
            println!("runtime missing");
        } else if let Some(version) = runtime.version {
            println!("runtime v{version}");
        } else {
            println!("runtime installed (unknown version)");
        }
        for addon in backend::scan(&addon_root) {
            println!(
                "{} {} v{} {} {}",
                if addon.enabled {
                    "enabled "
                } else {
                    "disabled"
                },
                addon.id,
                addon.version.as_deref().unwrap_or("unknown"),
                addon.artifact,
                addon.wasm_path.display()
            );
        }
        acted = true;
    }
    if acted {
        Ok(())
    } else {
        Err("no headless action given".to_owned())
    }
}

fn run_gui() -> Result<(), String> {
    let window = AddonManagerWindow::new().map_err(|error| error.to_string())?;
    let snapshot = Rc::new(RefCell::new(ManagerSnapshot::load()));
    // One persistent row model for the window's lifetime: syncs push new
    // contents into it instead of swapping models, so scroll position,
    // focus, and row identity survive refreshes.
    let rows: Rc<VecModel<AddonItem>> = Rc::new(VecModel::from(vec![]));
    window.set_addons(rows.clone().into());
    sync_window(&window, &rows, &snapshot.borrow());
    bind_callbacks(&window, &snapshot, &rows);
    fit_scale(&window);

    let timer_snapshot = Rc::clone(&snapshot);
    let timer_rows = Rc::clone(&rows);
    let weak_window = window.as_weak();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, REFRESH_INTERVAL, move || {
        if let Some(window) = weak_window.upgrade() {
            if timer_snapshot.borrow_mut().refresh() {
                sync_window(&window, &timer_rows, &timer_snapshot.borrow());
            }
        }
    });

    // Tracks window resizes far ahead of the disk refresh above so scaling
    // follows the drag instead of lagging behind it. The callback only reads
    // the window size and pushes a float when it actually changed.
    let weak_window = window.as_weak();
    let scale_timer = Timer::default();
    scale_timer.start(TimerMode::Repeated, Duration::from_millis(50), move || {
        if let Some(window) = weak_window.upgrade() {
            fit_scale(&window);
        }
    });

    window.run().map_err(|error| error.to_string())
}

/// Pushes a resolution-independent scale factor into the UI from the live
/// logical window width. Driven from Rust (startup + fast timer) so Slint
/// sees a plain property instead of a width-derived binding loop.
fn fit_scale(window: &AddonManagerWindow) {
    let api = window.window();
    let factor = api.scale_factor();
    if factor <= 0.0 {
        return;
    }
    let logical_width = api.size().width as f32 / factor;
    let scale = discrete_scale(logical_width);
    if (scale - window.get_ui_scale()).abs() > 0.001 {
        window.set_ui_scale(scale);
    }
}

/// A few discrete sizes instead of a continuum: crossing a breakpoint swaps
/// the whole layout exactly once, cheaply enough to paint mid-drag, where a
/// continuous scale would relayout every tick and only the final frame would
/// ever reach the screen.
fn discrete_scale(logical_width: f32) -> f32 {
    if logical_width < 800.0 {
        0.8
    } else if logical_width < 1050.0 {
        1.0
    } else if logical_width < 1300.0 {
        1.25
    } else {
        1.5
    }
}

#[cfg(test)]
mod tests {
    use super::discrete_scale;

    #[test]
    fn scale_steps_are_discrete_and_cover_defaults() {
        assert_eq!(discrete_scale(760.0), 0.8);
        assert_eq!(discrete_scale(799.9), 0.8);
        assert_eq!(discrete_scale(800.0), 1.0);
        // The 980px design width lands exactly on the default step.
        assert_eq!(discrete_scale(980.0), 1.0);
        assert_eq!(discrete_scale(1049.9), 1.0);
        assert_eq!(discrete_scale(1050.0), 1.25);
        assert_eq!(discrete_scale(1299.9), 1.25);
        assert_eq!(discrete_scale(1300.0), 1.5);
        assert_eq!(discrete_scale(4000.0), 1.5);
    }
}

fn bind_callbacks(
    window: &AddonManagerWindow,
    snapshot: &Rc<RefCell<ManagerSnapshot>>,
    rows: &Rc<VecModel<AddonItem>>,
) {
    // Each row-indexed action keeps its own explicit binding so the index
    // path stays visible; all of them funnel through update_snapshot.
    let weak_window = window.as_weak();
    let callback_snapshot = Rc::clone(snapshot);
    let callback_rows = Rc::clone(rows);
    window.on_toggle_enabled(move |index| {
        update_snapshot(
            &weak_window,
            &callback_snapshot,
            &callback_rows,
            index,
            |snapshot, index| {
                snapshot.toggle_enabled(index);
            },
        );
    });

    let weak_window = window.as_weak();
    let callback_snapshot = Rc::clone(snapshot);
    let callback_rows = Rc::clone(rows);
    window.on_toggle_expanded(move |index| {
        update_snapshot(
            &weak_window,
            &callback_snapshot,
            &callback_rows,
            index,
            |snapshot, index| {
                snapshot.toggle_expanded(index);
            },
        );
    });

    let weak_window = window.as_weak();
    let callback_snapshot = Rc::clone(snapshot);
    let callback_rows = Rc::clone(rows);
    window.on_retry_activation(move |index| {
        update_snapshot(
            &weak_window,
            &callback_snapshot,
            &callback_rows,
            index,
            |snapshot, index| {
                snapshot.retry_activation(index);
            },
        );
    });

    let weak_window = window.as_weak();
    let callback_snapshot = Rc::clone(snapshot);
    let callback_rows = Rc::clone(rows);
    window.on_remove_addon(move |index| {
        update_snapshot(
            &weak_window,
            &callback_snapshot,
            &callback_rows,
            index,
            |snapshot, index| {
                snapshot.remove(index);
            },
        );
    });

    let weak_window = window.as_weak();
    let callback_snapshot = Rc::clone(snapshot);
    let callback_rows = Rc::clone(rows);
    window.on_select_addon(move |index| {
        update_snapshot(
            &weak_window,
            &callback_snapshot,
            &callback_rows,
            index,
            |snapshot, index| {
                snapshot.expand_error(index);
            },
        );
    });

    let weak_window = window.as_weak();
    let callback_snapshot = Rc::clone(snapshot);
    let callback_rows = Rc::clone(rows);
    window.on_refresh(move || {
        callback_snapshot.borrow_mut().refresh();
        if let Some(window) = weak_window.upgrade() {
            sync_window(&window, &callback_rows, &callback_snapshot.borrow());
        }
    });

    let weak_window = window.as_weak();
    let callback_snapshot = Rc::clone(snapshot);
    let callback_rows = Rc::clone(rows);
    window.on_install_from_file(move || {
        let Some(path) = crate::file_dialog::pick_archive() else {
            return;
        };
        callback_snapshot
            .borrow_mut()
            .install_archive(path.to_str().unwrap_or_default());
        if let Some(window) = weak_window.upgrade() {
            sync_window(&window, &callback_rows, &callback_snapshot.borrow());
        }
    });

    let weak_window = window.as_weak();
    let callback_snapshot = Rc::clone(snapshot);
    let callback_rows = Rc::clone(rows);
    window.on_update_runtime(move || {
        callback_snapshot.borrow_mut().update_runtime(None, None);
        if let Some(window) = weak_window.upgrade() {
            sync_window(&window, &callback_rows, &callback_snapshot.borrow());
        }
    });

    let weak_window = window.as_weak();
    let callback_snapshot = Rc::clone(snapshot);
    let callback_rows = Rc::clone(rows);
    window.on_save_config(move |input| {
        callback_snapshot.borrow_mut().save_game_dir(input.as_str());
        if let Some(window) = weak_window.upgrade() {
            sync_window(&window, &callback_rows, &callback_snapshot.borrow());
        }
    });
}

fn update_snapshot(
    weak_window: &slint::Weak<AddonManagerWindow>,
    snapshot: &Rc<RefCell<ManagerSnapshot>>,
    rows: &Rc<VecModel<AddonItem>>,
    index: i32,
    update: impl FnOnce(&mut ManagerSnapshot, usize),
) {
    let Ok(index) = usize::try_from(index) else {
        return;
    };
    update(&mut snapshot.borrow_mut(), index);
    if let Some(window) = weak_window.upgrade() {
        sync_window(&window, rows, &snapshot.borrow());
    }
}

fn sync_window(
    window: &AddonManagerWindow,
    rows: &VecModel<AddonItem>,
    snapshot: &ManagerSnapshot,
) {
    let summary = snapshot.summary();
    let addons = snapshot.addons.iter().map(view_item).collect::<Vec<_>>();
    rows.set_vec(addons);
    window.set_active_count(summary.active as i32);
    window.set_compiling_count(summary.compiling as i32);
    window.set_disabled_count(summary.disabled as i32);
    window.set_incompatible_count(summary.incompatible as i32);
    window.set_addon_directory(snapshot.addon_directory.display().to_string().into());
    window.set_status_message(snapshot.status_message().into());
    window.set_game_dir(snapshot.game_dir.display().to_string().into());
    window.set_config_source(snapshot.game_dir_source().into());
    window.set_runtime_version(snapshot.runtime_version.clone().into());
    window.set_runtime_installed(snapshot.runtime_installed);
}

fn view_item(addon: &AddonEntry) -> AddonItem {
    AddonItem {
        id: SharedString::from(&addon.id),
        initials: SharedString::from(&addon.initials),
        name: SharedString::from(&addon.name),
        short_name: SharedString::from(addon.name.strip_prefix("Farever ").unwrap_or(&addon.name)),
        metadata: addon.metadata().into(),
        state: match addon.state {
            LifecycleState::Active => AddonState::Active,
            LifecycleState::Compiling => AddonState::Compiling,
            LifecycleState::Disabled => AddonState::Disabled,
            LifecycleState::Incompatible => AddonState::Incompatible,
        },
        enabled: addon.enabled,
        locked: addon.locked,
        error_message: addon
            .error
            .as_ref()
            .map(|error| SharedString::from(&error.message))
            .unwrap_or_default(),
        expanded: addon.expanded,
    }
}

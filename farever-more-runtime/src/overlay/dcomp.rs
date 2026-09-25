use super::{
    find_main_window, game_bounds, host_config_menu_key, is_host_config_menu, painter::Painter,
    render_interactive_frame, CanvasAction, ClickAction, ConfigControlKey, ConfigHostState,
    ConfigMenuKey, GameBounds, HostUiEvent, InteractiveRegion, SharedState, SliderAction,
    StampedUiEvent,
};
use crate::diagnostics::Level;
use std::collections::{HashMap, HashSet, VecDeque};
use std::mem::zeroed;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};
use windows::core::Interface;
use windows::Win32::Foundation::{BOOL as WinBool, HMODULE as WinHmodule, HWND as WinHwnd};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11DepthStencilView, ID3D11Device, ID3D11DeviceContext,
    ID3D11RenderTargetView, ID3D11Texture2D, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
    D3D11_VIEWPORT,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIFactory2, IDXGISwapChain1, DXGI_PRESENT, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows_sys::Win32::Foundation::{GetLastError, SetLastError, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CombineRgn, CreateRectRgn, DeleteObject, SetWindowRgn, RGN_OR,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
#[cfg(test)]
use windows_sys::Win32::UI::WindowsAndMessaging::WS_EX_TOPMOST;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetForegroundWindow,
    GetWindowLongPtrW, GetWindowThreadProcessId, PeekMessageW, RegisterClassW,
    SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage,
    UnregisterClassW, CS_HREDRAW, CS_VREDRAW, GWLP_HWNDPARENT, GWLP_USERDATA, HTTRANSPARENT,
    HWND_TOP, LWA_ALPHA, MA_NOACTIVATE, MSG, PM_REMOVE, SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE,
    SW_SHOWNOACTIVATE, WM_CANCELMODE, WM_CAPTURECHANGED, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_NCHITTEST, WM_QUIT, WNDCLASSW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
};

const INITIAL_WIDTH: u32 = 640;
const INITIAL_HEIGHT: u32 = 360;
const UI_REFERENCE_WIDTH: f32 = 1920.0;
const UI_REFERENCE_HEIGHT: f32 = 1080.0;
const MIN_UI_SCALE: f32 = 1.0;
const MAX_UI_SCALE: f32 = 3.0;
const MAX_PENDING_UI_CLICKS: usize = 64;
// A constant alpha of zero makes layered windows transparent to hit testing.
// One is visually negligible while the explicit window region remains clickable.
const INTERACTION_WINDOW_ALPHA: u8 = 1;
/// How long a client size has to hold still before it is reported as the
/// resting size. Window drags and monitor transitions resize on every frame.
const RESIZE_SETTLE: Duration = Duration::from_millis(400);

/// Reports each settled client size once, so a resize burst produces one line
/// instead of one line per frame.
struct ResizeReporter {
    settle: Duration,
    pending: Option<((i32, i32), Instant)>,
    reported: Option<(i32, i32)>,
}

impl ResizeReporter {
    fn new(settle: Duration) -> Self {
        Self {
            settle,
            pending: None,
            reported: None,
        }
    }

    fn observe(&mut self, size: (i32, i32)) -> Option<(i32, i32)> {
        if self.reported == Some(size) {
            self.pending = None;
            return None;
        }
        match self.pending {
            Some((pending, since)) if pending == size => {
                if since.elapsed() < self.settle {
                    return None;
                }
                self.pending = None;
                self.reported = Some(size);
                Some(size)
            }
            _ => {
                self.pending = Some((size, Instant::now()));
                None
            }
        }
    }

    fn reset(&mut self) {
        self.pending = None;
    }
}

#[derive(Default)]
struct UiQueueDrops {
    addon_events: u64,
    host_events: u64,
}

pub(super) fn run(
    title: &str,
    shared: Arc<SharedState>,
    diagnostics: mpsc::Sender<(Level, String)>,
    ui_events: mpsc::SyncSender<StampedUiEvent>,
    host_ui_events: mpsc::SyncSender<HostUiEvent>,
) -> Result<(), String> {
    let _apartment = ComApartment::initialize()?;
    let window = OverlayWindow::create(title, INITIAL_WIDTH as i32, INITIAL_HEIGHT as i32)?;
    let interaction_window =
        InteractionWindow::create(title, INITIAL_WIDTH as i32, INITIAL_HEIGHT as i32)?;
    let mut renderer = DcompRenderer::new(window.handle, INITIAL_WIDTH, INITIAL_HEIGHT)?;
    let log = |level: Level, message: String| {
        let _ = diagnostics.send((level, message));
    };
    log(
        Level::Info,
        "egui renderer initialized backend=d3d11-directcomposition alpha=premultiplied click_through=layered+transparent+hit_test"
            .to_owned(),
    );

    let mut shown = false;
    let mut owner = None;
    let mut last_bounds = None;
    let mut last_revision = u64::MAX;
    let mut last_placement = None;
    let mut placement_owner = None;
    let mut last_addon_surface_count = 0;
    let mut resized = ResizeReporter::new(RESIZE_SETTLE);
    let mut installed_fonts = Arc::new(Vec::new());
    let mut installed_images = Arc::new(Vec::new());
    let mut config_host = ConfigHostState::default();
    let mut last_game_menu_open = false;
    let mut last_game_menu_geometry_revision = 0_u64;
    let mut game_menu_geometry_by_viewport = HashMap::new();
    let mut last_slider_preview_revision = 0_u64;
    let mut total_click_drops = 0_u64;
    let mut total_addon_event_drops = 0_u64;
    let mut total_host_event_drops = 0_u64;

    while !shared.stop.load(Ordering::Acquire) && window.pump_messages() {
        let packet = match shared.packet.read() {
            Ok(packet) => packet.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        let search = packet
            .target_pid
            .map(|pid| find_main_window(pid, window.handle))
            .unwrap_or_default();
        let target_is_foreground = packet.target_pid.is_some_and(process_is_foreground);
        let selected = search.window.unwrap_or(null_mut()) as usize;
        let placement = format!(
            "requested={} foreground={} target_pid={:?} selected=0x{selected:x} pid_windows={} eligible_windows={} selected_area={}",
            packet.visible,
            target_is_foreground,
            packet.target_pid,
            search.pid_windows,
            search.eligible_windows,
            search.area,
        );
        // Which window the overlay is allowed to follow is the fact worth
        // keeping; the window census around it moves constantly and stays at
        // debug so a scan of the log is not a wall of candidate counts.
        let placement_owner_pointer = (packet.visible, target_is_foreground, selected);
        if placement_owner != Some(placement_owner_pointer) {
            log(Level::Info, format!("placement {placement}"));
            placement_owner = Some(placement_owner_pointer);
            last_placement = Some(placement);
        } else if last_placement.as_deref() != Some(placement.as_str()) {
            log(Level::Debug, format!("placement {placement}"));
            last_placement = Some(placement);
        }

        if search.window != owner {
            if let Some(target) = search.window {
                window.set_owner(target)?;
                interaction_window.set_owner(target)?;
                owner = Some(target);
                log(Level::Info, format!("owner target=0x{:x}", target as usize));
            }
        }

        let bounds = search
            .window
            .filter(|_| packet.visible && target_is_foreground)
            .and_then(game_bounds);
        let Some(bounds) = bounds else {
            if shown {
                window.hide();
                interaction_window.hide();
                shown = false;
                last_bounds = None;
                resized.reset();
            }
            thread::sleep(Duration::from_millis(50));
            continue;
        };

        let bounds_changed = last_bounds != Some(bounds);
        if bounds_changed {
            window.place(bounds)?;
            interaction_window.place(bounds)?;
            renderer.resize(bounds.width as u32, bounds.height as u32)?;
            last_bounds = Some(bounds);
            last_revision = u64::MAX;
        }
        // Dragging the game window or crossing a monitor changes the client
        // area on every frame. Report the size that settled instead of the
        // frames in between, which used to be the largest single source of
        // burst lines in the log.
        if let Some(size) = resized.observe((bounds.width, bounds.height)) {
            log(
                Level::Info,
                format!(
                    "resized width={} height={} ui_scale={:.3}",
                    size.0,
                    size.1,
                    renderer.ui_scale()
                ),
            );
        }
        let viewport_key = (bounds.width, bounds.height);
        let mut game_menu_geometry_changed = false;
        if packet.game_menu_geometry_revision != last_game_menu_geometry_revision {
            if let Some(geometry) = packet
                .game_menu_geometry
                .filter(|geometry| game_menu_geometry_is_plausible(bounds, *geometry))
            {
                game_menu_geometry_by_viewport.insert(viewport_key, geometry);
                log(
                    Level::Info,
                    format!(
                        "game_menu geometry cached client={}x{} physical_rect={:.1},{:.1},{:.1},{:.1} revision={}",
                        bounds.width,
                        bounds.height,
                        geometry.x,
                        geometry.y,
                        geometry.width,
                    geometry.height,
                    packet.game_menu_geometry_revision,
                ));
                last_revision = u64::MAX;
                game_menu_geometry_changed = true;
            } else if let Some(geometry) = packet.game_menu_geometry {
                log(
                    Level::Warn,
                    format!(
                        "game_menu geometry rejected client={}x{} runtime_rect={:.1},{:.1},{:.1},{:.1} revision={}",
                        bounds.width,
                        bounds.height,
                        geometry.x,
                        geometry.y,
                        geometry.width,
                    geometry.height,
                    packet.game_menu_geometry_revision,
                ));
            }
            last_game_menu_geometry_revision = packet.game_menu_geometry_revision;
        }
        let game_menu_geometry = game_menu_geometry_by_viewport.get(&viewport_key).copied();
        if packet.game_menu_open
            && (!last_game_menu_open || bounds_changed || game_menu_geometry_changed)
        {
            if let Some(game_menu_geometry) = game_menu_geometry {
                let ui_scale = renderer.ui_scale();
                let logical_viewport = egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(
                        bounds.width as f32 / ui_scale,
                        bounds.height as f32 / ui_scale,
                    ),
                );
                let gear =
                    super::game_menu_gear_rect(logical_viewport, ui_scale, game_menu_geometry);
                let layout = if logical_viewport.width() >= logical_viewport.height() {
                    "wide"
                } else {
                    "centered"
                };
                log(
                    Level::Info,
                    format!(
                        "game_menu config_entrypoint layout={layout} client={}x{} ui_scale={ui_scale:.3} logical_viewport={:.1}x{:.1} logical_rect={:.1},{:.1},{:.1},{:.1} physical_rect={:.1},{:.1},{:.1},{:.1}",
                        bounds.width,
                        bounds.height,
                        logical_viewport.width(),
                        logical_viewport.height(),
                        gear.min.x,
                        gear.min.y,
                        gear.width(),
                        gear.height(),
                        gear.min.x * ui_scale,
                        gear.min.y * ui_scale,
                        gear.width() * ui_scale,
                        gear.height() * ui_scale,
                    ),
                );
            } else {
                log(
                    Level::Debug,
                    format!(
                        "game_menu config_entrypoint waiting_for_geometry client={}x{}",
                        bounds.width, bounds.height
                    ),
                );
            }
        }
        last_game_menu_open = packet.game_menu_open;
        if !Arc::ptr_eq(&installed_fonts, &packet.fonts) {
            renderer.install_fonts(&packet.fonts);
            let byte_count = packet
                .fonts
                .iter()
                .map(|font| font.bytes.len())
                .sum::<usize>();
            log(
                Level::Info,
                format!(
                    "fonts installed faces={} bytes={byte_count}",
                    packet.fonts.len()
                ),
            );
            installed_fonts = Arc::clone(&packet.fonts);
            last_revision = u64::MAX;
        }
        if !Arc::ptr_eq(&installed_images, &packet.images) {
            renderer.install_images(&packet.images);
            let byte_count = packet
                .images
                .iter()
                .map(|image| image.rgba.len())
                .sum::<usize>();
            log(
                Level::Info,
                format!(
                    "images installed resources={} bytes={byte_count}",
                    packet.images.len()
                ),
            );
            installed_images = Arc::clone(&packet.images);
            last_revision = u64::MAX;
        }
        let was_shown = shown;
        if !shown {
            window.show();
            interaction_window.show();
            shown = true;
        }
        let first_surface_after_empty =
            last_addon_surface_count == 0 && packet.addon_surface_count != 0;
        if !was_shown || first_surface_after_empty {
            // Bounds can stay fixed across a game scene or focus transition.
            // Place both owned popups again when add-on content first appears
            // or the overlay returns, keeping the click target above its
            // visual window without activating either one.
            window.place(bounds)?;
            interaction_window.place(bounds)?;
            if packet.addon_surface_count != 0 {
                let reason = if first_surface_after_empty {
                    "first-addon-surface"
                } else {
                    "foreground-return"
                };
                log(Level::Info, format!("z_order refreshed reason={reason}"));
            }
        }
        last_addon_surface_count = packet.addon_surface_count;

        let mut interaction_changed = false;
        let mut queue_drops = UiQueueDrops::default();
        if !packet.game_menu_open && config_host.open {
            if let Some(previous) = config_host.active_menu.take() {
                send_config_visibility(
                    &ui_events,
                    packet.addon_revision,
                    previous,
                    false,
                    &mut queue_drops,
                );
            }
            config_host.open = false;
            config_host.open_dropdown = None;
            config_host.slider_preview = None;
            interaction_changed = true;
        }
        if config_host
            .active_menu
            .as_ref()
            .is_some_and(|active| !menu_exists(&packet.frame, active))
        {
            if let Some(previous) = config_host.active_menu.take() {
                send_config_visibility(
                    &ui_events,
                    packet.addon_revision,
                    previous,
                    false,
                    &mut queue_drops,
                );
            }
            config_host.open_dropdown = None;
            config_host.slider_preview = None;
            interaction_changed = true;
        }
        if config_host.open && config_host.active_menu.is_none() {
            config_host.active_menu = Some(host_config_menu_key());
            interaction_changed = true;
        }
        let slider_preview_revision = interaction_window.slider_preview_revision();
        if slider_preview_revision != last_slider_preview_revision {
            config_host.slider_preview = interaction_window.slider_preview();
            last_slider_preview_revision = slider_preview_revision;
            interaction_changed = true;
        }
        for action in interaction_window.take_clicks() {
            interaction_changed |= apply_click_action(
                action,
                &packet.frame,
                packet.game_menu_open,
                &mut config_host,
                &ui_events,
                &host_ui_events,
                &mut queue_drops,
            );
        }
        let click_drops = interaction_window.take_dropped_clicks();
        if click_drops != 0 || queue_drops.addon_events != 0 || queue_drops.host_events != 0 {
            total_click_drops = total_click_drops.saturating_add(click_drops);
            total_addon_event_drops =
                total_addon_event_drops.saturating_add(queue_drops.addon_events);
            total_host_event_drops = total_host_event_drops.saturating_add(queue_drops.host_events);
            log(
                Level::Warn,
                format!(
                    "ui input dropped click_queue_total={total_click_drops} addon_event_queue_total={total_addon_event_drops} host_event_queue_total={total_host_event_drops}"
                ),
            );
        }

        let revision = shared.revision.load(Ordering::Acquire);
        if revision != last_revision || bounds_changed || interaction_changed {
            let interactions = renderer.render(
                &packet.frame,
                packet.game_menu_open,
                game_menu_geometry,
                packet.slash_commands_enabled,
                &config_host,
            )?;
            interaction_window.update_regions(
                &interactions,
                renderer.ui_scale(),
                packet.addon_revision,
            )?;
            last_revision = revision;
        }
        thread::sleep(Duration::from_millis(16));
    }

    Ok(())
}

fn apply_click_action(
    stamped: impl Into<StampedClick>,
    frame: &farever_more_api::UiFrame,
    game_menu_open: bool,
    config_host: &mut ConfigHostState,
    ui_events: &mpsc::SyncSender<StampedUiEvent>,
    host_ui_events: &mpsc::SyncSender<HostUiEvent>,
    queue_drops: &mut UiQueueDrops,
) -> bool {
    let StampedClick {
        addon_revision,
        action,
        position,
    } = stamped.into();
    match action {
        ClickAction::OpenConfigMenu if game_menu_open && !config_host.open => {
            config_host.open = true;
            config_host.active_menu = Some(host_config_menu_key());
            config_host.open_dropdown = None;
            true
        }
        ClickAction::SelectConfigMenu(selected)
            if config_host.open
                && game_menu_open
                && menu_exists(frame, &selected)
                && config_host.active_menu.as_ref() != Some(&selected) =>
        {
            if let Some(previous) = config_host.active_menu.replace(selected.clone()) {
                send_config_visibility(ui_events, addon_revision, previous, false, queue_drops);
            }
            config_host.open_dropdown = None;
            config_host.slider_preview = None;
            send_config_visibility(ui_events, addon_revision, selected, true, queue_drops);
            true
        }
        ClickAction::CloseConfigMenu => {
            if !config_host.open {
                return false;
            }
            if let Some(previous) = config_host.active_menu.take() {
                send_config_visibility(ui_events, addon_revision, previous, false, queue_drops);
            }
            config_host.open = false;
            config_host.open_dropdown = None;
            config_host.slider_preview = None;
            true
        }
        ClickAction::SetSlashCommandsEnabled(enabled)
            if config_host.open
                && game_menu_open
                && config_host
                    .active_menu
                    .as_ref()
                    .is_some_and(is_host_config_menu) =>
        {
            if matches!(
                host_ui_events.try_send(HostUiEvent::SetSlashCommandsEnabled(enabled)),
                Err(mpsc::TrySendError::Full(_))
            ) {
                queue_drops.host_events = queue_drops.host_events.saturating_add(1);
            }
            false
        }
        ClickAction::AddonButton(event)
            if routed_button_exists(frame, &event)
                && routed_view_is_active(
                    &event,
                    game_menu_open,
                    config_host.active_menu.as_ref(),
                ) =>
        {
            send_addon_ui_event(ui_events, addon_revision, event, queue_drops);
            false
        }
        ClickAction::AddonCheckbox(event)
            if routed_checkbox_exists(
                frame,
                &event,
                game_menu_open,
                config_host.active_menu.as_ref(),
            ) =>
        {
            send_addon_ui_event(ui_events, addon_revision, event, queue_drops);
            config_host.open_dropdown = None;
            false
        }
        ClickAction::ToggleAddonDropdown(control)
            if routed_dropdown_control_exists(
                frame,
                &control,
                game_menu_open,
                config_host.active_menu.as_ref(),
            ) =>
        {
            if config_host.open_dropdown.as_ref() == Some(&control) {
                config_host.open_dropdown = None;
            } else {
                config_host.open_dropdown = Some(control);
            }
            true
        }
        ClickAction::AddonDropdown(event)
            if routed_dropdown_exists(
                frame,
                &event,
                game_menu_open,
                config_host.active_menu.as_ref(),
            ) =>
        {
            send_addon_ui_event(ui_events, addon_revision, event, queue_drops);
            config_host.open_dropdown = None;
            true
        }
        ClickAction::AddonCanvas(action)
            if routed_canvas_exists(
                frame,
                &action,
                game_menu_open,
                config_host.active_menu.as_ref(),
            ) =>
        {
            // Report the press in canvas-local logical points: physical click,
            // divided by the render scale, minus the canvas origin.
            if let Some(position) = position {
                let local = [
                    f64::from(position[0] / action.pixels_per_point - action.origin[0]),
                    f64::from(position[1] / action.pixels_per_point - action.origin[1]),
                ];
                send_addon_ui_event(
                    ui_events,
                    addon_revision,
                    farever_more_api::RoutedUiEvent {
                        owner: action.owner.clone(),
                        event: farever_more_api::UiEvent::CanvasPressed {
                            view: action.view.clone(),
                            node_id: action.node_id.clone(),
                            x: local[0],
                            y: local[1],
                        },
                    },
                    queue_drops,
                );
            }
            false
        }
        ClickAction::AddonSlider(slider)
            if routed_slider_exists(
                frame,
                &slider,
                game_menu_open,
                config_host.active_menu.as_ref(),
            ) =>
        {
            send_addon_ui_event(
                ui_events,
                addon_revision,
                farever_more_api::RoutedUiEvent {
                    owner: slider.control.owner,
                    event: farever_more_api::UiEvent::SliderChanged {
                        node_id: slider.control.node_id,
                        value: slider.value,
                    },
                },
                queue_drops,
            );
            config_host.open_dropdown = None;
            false
        }
        _ => false,
    }
}

fn game_menu_geometry_is_plausible(bounds: GameBounds, geometry: super::GameMenuGeometry) -> bool {
    let right = geometry.x + geometry.width;
    let bottom = geometry.y + geometry.height;
    [geometry.x, geometry.y, geometry.width, geometry.height]
        .iter()
        .all(|value| value.is_finite())
        && geometry.width >= 64.0
        && geometry.height >= 64.0
        && geometry.x >= 0.0
        && geometry.y >= 0.0
        && right <= bounds.width as f32 + 1.0
        && bottom <= bounds.height as f32 + 1.0
}

fn routed_view_is_active(
    routed: &farever_more_api::RoutedUiEvent,
    game_menu_open: bool,
    active_config_menu: Option<&ConfigMenuKey>,
) -> bool {
    let farever_more_api::UiEvent::ButtonPressed { view, .. } = &routed.event else {
        return false;
    };
    match view {
        farever_more_api::UiView::Surface(_) => true,
        farever_more_api::UiView::ConfigMenu(menu_id) => {
            game_menu_open
                && active_config_menu
                    .is_some_and(|active| active.owner == routed.owner && active.id == *menu_id)
        }
    }
}

fn send_config_visibility(
    ui_events: &mpsc::SyncSender<StampedUiEvent>,
    addon_revision: u64,
    menu: ConfigMenuKey,
    visible: bool,
    queue_drops: &mut UiQueueDrops,
) {
    if is_host_config_menu(&menu) {
        return;
    }
    let event = if visible {
        farever_more_api::UiEvent::ConfigMenuShown(menu.id)
    } else {
        farever_more_api::UiEvent::ConfigMenuHidden(menu.id)
    };
    send_addon_ui_event(
        ui_events,
        addon_revision,
        farever_more_api::RoutedUiEvent {
            owner: menu.owner,
            event,
        },
        queue_drops,
    );
}

fn send_addon_ui_event(
    ui_events: &mpsc::SyncSender<StampedUiEvent>,
    addon_revision: u64,
    event: farever_more_api::RoutedUiEvent,
    queue_drops: &mut UiQueueDrops,
) {
    if matches!(
        ui_events.try_send(StampedUiEvent {
            addon_revision,
            routed: event,
        }),
        Err(mpsc::TrySendError::Full(_))
    ) {
        queue_drops.addon_events = queue_drops.addon_events.saturating_add(1);
    }
}

fn menu_exists(frame: &farever_more_api::UiFrame, key: &ConfigMenuKey) -> bool {
    is_host_config_menu(key)
        || frame
            .config_menus
            .iter()
            .any(|menu| menu.owner == key.owner && menu.id == key.id)
}

fn routed_button_exists(
    frame: &farever_more_api::UiFrame,
    routed: &farever_more_api::RoutedUiEvent,
) -> bool {
    let farever_more_api::UiEvent::ButtonPressed { view, node_id } = &routed.event else {
        return false;
    };
    let nodes = match view {
        farever_more_api::UiView::Surface(surface_id) => frame
            .surfaces
            .iter()
            .find(|surface| surface.owner == routed.owner && surface.id == *surface_id)
            .map(|surface| surface.nodes.as_slice()),
        farever_more_api::UiView::ConfigMenu(menu_id) => frame
            .config_menus
            .iter()
            .find(|menu| menu.owner == routed.owner && menu.id == *menu_id)
            .map(|menu| menu.nodes.as_slice()),
    };
    nodes.is_some_and(|nodes| {
        nodes.iter().any(|node| {
            node.id == *node_id
                && matches!(
                    node.widget,
                    farever_more_api::Widget::Button(farever_more_api::ButtonWidget {
                        enabled: true,
                        ..
                    })
                )
        })
    })
}

fn routed_canvas_exists(
    frame: &farever_more_api::UiFrame,
    action: &CanvasAction,
    game_menu_open: bool,
    active_config_menu: Option<&ConfigMenuKey>,
) -> bool {
    let nodes = match &action.view {
        farever_more_api::UiView::Surface(surface_id) => frame
            .surfaces
            .iter()
            .find(|surface| surface.owner == action.owner && surface.id == *surface_id)
            .map(|surface| surface.nodes.as_slice()),
        farever_more_api::UiView::ConfigMenu(menu_id) => {
            if !game_menu_open
                || active_config_menu
                    .is_none_or(|active| active.owner != action.owner || active.id != *menu_id)
            {
                return false;
            }
            frame
                .config_menus
                .iter()
                .find(|menu| menu.owner == action.owner && menu.id == *menu_id)
                .map(|menu| menu.nodes.as_slice())
        }
    };
    nodes.is_some_and(|nodes| {
        nodes.iter().any(|node| {
            node.id == action.node_id && matches!(node.widget, farever_more_api::Widget::Canvas(_))
        })
    })
}

fn routed_checkbox_exists(
    frame: &farever_more_api::UiFrame,
    routed: &farever_more_api::RoutedUiEvent,
    game_menu_open: bool,
    active_config_menu: Option<&ConfigMenuKey>,
) -> bool {
    let farever_more_api::UiEvent::CheckboxChanged { node_id, checked } = &routed.event else {
        return false;
    };
    let Some(active) = active_config_menu else {
        return false;
    };
    if !game_menu_open || active.owner != routed.owner || is_host_config_menu(active) {
        return false;
    }
    frame
        .config_menus
        .iter()
        .find(|menu| menu.owner == active.owner && menu.id == active.id)
        .is_some_and(|menu| {
            menu.nodes.iter().any(|node| {
                node.id == *node_id
                    && matches!(
                        node.widget,
                        farever_more_api::Widget::Checkbox(
                            farever_more_api::CheckboxWidget {
                                checked: current,
                                enabled: true,
                                ..
                            }
                        ) if current != *checked
                    )
            })
        })
}

fn routed_dropdown_control_exists(
    frame: &farever_more_api::UiFrame,
    control: &ConfigControlKey,
    game_menu_open: bool,
    active_config_menu: Option<&ConfigMenuKey>,
) -> bool {
    let Some(active) = active_config_menu else {
        return false;
    };
    if !game_menu_open
        || active.owner != control.owner
        || active.id != control.menu_id
        || is_host_config_menu(active)
    {
        return false;
    }
    frame
        .config_menus
        .iter()
        .find(|menu| menu.owner == control.owner && menu.id == control.menu_id)
        .is_some_and(|menu| {
            menu.nodes.iter().any(|node| {
                node.id == control.node_id
                    && matches!(
                        node.widget,
                        farever_more_api::Widget::Dropdown(farever_more_api::DropdownWidget {
                            enabled: true,
                            ..
                        })
                    )
            })
        })
}

fn routed_dropdown_exists(
    frame: &farever_more_api::UiFrame,
    routed: &farever_more_api::RoutedUiEvent,
    game_menu_open: bool,
    active_config_menu: Option<&ConfigMenuKey>,
) -> bool {
    let farever_more_api::UiEvent::DropdownChanged {
        node_id,
        selected_id,
    } = &routed.event
    else {
        return false;
    };
    let Some(active) = active_config_menu else {
        return false;
    };
    if !game_menu_open || active.owner != routed.owner || is_host_config_menu(active) {
        return false;
    }
    frame
        .config_menus
        .iter()
        .find(|menu| menu.owner == active.owner && menu.id == active.id)
        .is_some_and(|menu| {
            menu.nodes.iter().any(|node| {
                node.id == *node_id
                    && matches!(
                        &node.widget,
                        farever_more_api::Widget::Dropdown(dropdown)
                            if dropdown.enabled
                                && dropdown.selected_id != *selected_id
                                && dropdown.options.iter().any(|option| option.id == *selected_id)
                    )
            })
        })
}

fn routed_slider_exists(
    frame: &farever_more_api::UiFrame,
    slider: &SliderAction,
    game_menu_open: bool,
    active_config_menu: Option<&ConfigMenuKey>,
) -> bool {
    let Some(active) = active_config_menu else {
        return false;
    };
    if !game_menu_open
        || active.owner != slider.control.owner
        || active.id != slider.control.menu_id
        || is_host_config_menu(active)
        || !slider.value.is_finite()
    {
        return false;
    }
    frame
        .config_menus
        .iter()
        .find(|menu| menu.owner == active.owner && menu.id == active.id)
        .is_some_and(|menu| {
            menu.nodes.iter().any(|node| {
                node.id == slider.control.node_id
                    && matches!(
                        node.widget,
                        farever_more_api::Widget::Slider(farever_more_api::SliderWidget {
                            value: current,
                            minimum,
                            maximum,
                            step,
                            enabled: true,
                            ..
                        }) if slider.value != current
                            && slider.minimum == minimum
                            && slider.maximum == maximum
                            && slider.step == step
                            && slider.value >= minimum
                            && slider.value <= maximum
                    )
            })
        })
}

fn process_is_foreground(target_pid: u32) -> bool {
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.is_null() {
        return false;
    }
    let mut foreground_pid = 0;
    unsafe { GetWindowThreadProcessId(foreground, &mut foreground_pid) };
    foreground_pid == target_pid
}

struct ComApartment;

impl ComApartment {
    fn initialize() -> Result<Self, String> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }
            .map_err(|error| format!("COM initialization failed: {error}"))?;
        Ok(Self)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

struct OverlayWindow {
    handle: HWND,
    instance: *mut core::ffi::c_void,
    class_name: Vec<u16>,
}

impl OverlayWindow {
    fn create(title: &str, width: i32, height: i32) -> Result<Self, String> {
        let instance = unsafe { GetModuleHandleW(null()) };
        if instance.is_null() {
            return Err("GetModuleHandleW failed for overlay window".to_owned());
        }
        let class_name = format!("FareverAddonOverlay_{}", std::process::id())
            .encode_utf16()
            .chain([0])
            .collect::<Vec<_>>();
        let title = title.encode_utf16().chain([0]).collect::<Vec<_>>();
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class_name.as_ptr(),
            ..unsafe { zeroed() }
        };
        if unsafe { RegisterClassW(&class) } == 0 {
            return Err("RegisterClassW failed for overlay window".to_owned());
        }

        let extended_style = overlay_extended_style();
        let handle = unsafe {
            CreateWindowExW(
                extended_style,
                class_name.as_ptr(),
                title.as_ptr(),
                WS_POPUP,
                0,
                0,
                width,
                height,
                null_mut(),
                null_mut(),
                instance,
                null(),
            )
        };
        if handle.is_null() {
            unsafe { UnregisterClassW(class_name.as_ptr(), instance) };
            return Err("CreateWindowExW failed for overlay window".to_owned());
        }
        // WS_EX_TRANSPARENT has documented mouse pass-through semantics for a
        // layered window. DirectComposition supports layered composition
        // targets, and the explicit hit-test result below is a second guard.
        if unsafe { SetLayeredWindowAttributes(handle, 0, 255, LWA_ALPHA) } == 0 {
            unsafe {
                DestroyWindow(handle);
                UnregisterClassW(class_name.as_ptr(), instance);
            }
            return Err("SetLayeredWindowAttributes failed for overlay window".to_owned());
        }
        Ok(Self {
            handle,
            instance,
            class_name,
        })
    }

    fn pump_messages(&self) -> bool {
        let mut message: MSG = unsafe { zeroed() };
        while unsafe { PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) } != 0 {
            if message.message == WM_QUIT {
                return false;
            }
            unsafe {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        true
    }

    fn place(&self, bounds: GameBounds) -> Result<(), String> {
        let placed = unsafe {
            SetWindowPos(
                self.handle,
                HWND_TOP,
                bounds.x,
                bounds.y,
                bounds.width,
                bounds.height,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )
        };
        (placed != 0)
            .then_some(())
            .ok_or_else(|| "SetWindowPos failed for overlay window".to_owned())
    }

    fn set_owner(&self, owner: HWND) -> Result<(), String> {
        // An owned non-topmost popup stays immediately above Farever when the
        // game is activated, without covering unrelated applications. The
        // overlay is created before the target HWND is known, so ownership is
        // assigned as soon as window selection succeeds.
        unsafe { SetLastError(0) };
        let previous = unsafe { SetWindowLongPtrW(self.handle, GWLP_HWNDPARENT, owner as isize) };
        let error = unsafe { GetLastError() };
        if previous == 0 && error != 0 {
            return Err(format!(
                "SetWindowLongPtrW failed for overlay owner error={error}"
            ));
        }
        Ok(())
    }

    fn show(&self) {
        unsafe { ShowWindow(self.handle, SW_SHOWNOACTIVATE) };
    }

    fn hide(&self) {
        unsafe { ShowWindow(self.handle, SW_HIDE) };
    }
}

impl Drop for OverlayWindow {
    fn drop(&mut self) {
        unsafe {
            DestroyWindow(self.handle);
            UnregisterClassW(self.class_name.as_ptr(), self.instance);
        }
    }
}

#[derive(Clone)]
struct PhysicalInteraction {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
    addon_revision: u64,
    action: ClickAction,
}

#[derive(Clone, Debug, PartialEq)]
struct StampedClick {
    addon_revision: u64,
    action: ClickAction,
    /// Physical click position, when the click came from the interaction
    /// window. Actions that need it (canvases) derive their payload from it.
    position: Option<[f32; 2]>,
}

impl From<ClickAction> for StampedClick {
    fn from(action: ClickAction) -> Self {
        Self {
            addon_revision: 0,
            action,
            position: None,
        }
    }
}

#[derive(Clone)]
struct SliderDrag {
    left: i32,
    right: i32,
    addon_revision: u64,
    action: SliderAction,
}

#[derive(Default)]
struct InteractionState {
    regions: RwLock<Vec<PhysicalInteraction>>,
    clicks: Mutex<VecDeque<StampedClick>>,
    slider_drag: Mutex<Option<SliderDrag>>,
    slider_preview: RwLock<Option<SliderAction>>,
    slider_preview_revision: AtomicU64,
    dropped_clicks: AtomicU64,
}

impl InteractionState {
    fn queue_action(&self, action: StampedClick) {
        let mut clicks = self
            .clicks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if clicks.len() < MAX_PENDING_UI_CLICKS {
            clicks.push_back(action);
        } else {
            self.dropped_clicks.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn click_at(&self, x: i32, y: i32) {
        let action = self
            .regions
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .rev()
            .find(|region| {
                x >= region.left && x < region.right && y >= region.top && y < region.bottom
            })
            .map(|region| StampedClick {
                addon_revision: region.addon_revision,
                action: region.action.clone(),
                position: Some([x as f32, y as f32]),
            });
        if let Some(action) = action {
            self.queue_action(action);
        }
    }

    fn begin_slider_drag(&self, x: i32, y: i32) -> bool {
        let drag = self
            .regions
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .rev()
            .find_map(|region| {
                if x < region.left || x >= region.right || y < region.top || y >= region.bottom {
                    return None;
                }
                let ClickAction::AddonSlider(action) = &region.action else {
                    return None;
                };
                Some(SliderDrag {
                    left: region.left,
                    right: region.right,
                    addon_revision: region.addon_revision,
                    action: positioned_slider_action(action, region.left, region.right, x),
                })
            });
        let Some(drag) = drag else {
            return false;
        };
        self.set_slider_preview(Some(drag.action.clone()));
        *self
            .slider_drag
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(drag);
        true
    }

    fn update_slider_drag(&self, x: i32) -> bool {
        let preview = {
            let mut drag = self
                .slider_drag
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(drag) = drag.as_mut() else {
                return false;
            };
            drag.action = positioned_slider_action(&drag.action, drag.left, drag.right, x);
            drag.action.clone()
        };
        self.set_slider_preview(Some(preview));
        true
    }

    fn finish_slider_drag(&self, x: i32) -> bool {
        let drag = self
            .slider_drag
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        let Some(drag) = drag else {
            return false;
        };
        let action = positioned_slider_action(&drag.action, drag.left, drag.right, x);
        self.set_slider_preview(None);
        self.queue_action(StampedClick {
            addon_revision: drag.addon_revision,
            action: ClickAction::AddonSlider(action),
            position: None,
        });
        true
    }

    fn cancel_slider_drag(&self) {
        let had_drag = self
            .slider_drag
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .is_some();
        if had_drag {
            self.set_slider_preview(None);
        }
    }

    fn set_slider_preview(&self, preview: Option<SliderAction>) {
        let mut current = self
            .slider_preview
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *current != preview {
            *current = preview;
            self.slider_preview_revision.fetch_add(1, Ordering::Release);
        }
    }

    fn take_dropped_clicks(&self) -> u64 {
        self.dropped_clicks.swap(0, Ordering::Relaxed)
    }
}

fn positioned_slider_action(action: &SliderAction, left: i32, right: i32, x: i32) -> SliderAction {
    let width = f64::from((right - left).max(1));
    let fraction = f64::from(x - left) / width;
    let mut positioned = action.clone();
    positioned.value = action.value_at_fraction(fraction);
    positioned
}

struct InteractionWindow {
    handle: HWND,
    instance: *mut core::ffi::c_void,
    class_name: Vec<u16>,
    state: Arc<InteractionState>,
}

impl InteractionWindow {
    fn create(title: &str, width: i32, height: i32) -> Result<Self, String> {
        let instance = unsafe { GetModuleHandleW(null()) };
        if instance.is_null() {
            return Err("GetModuleHandleW failed for interaction window".to_owned());
        }
        let class_name = format!("FareverAddonInteraction_{}", std::process::id())
            .encode_utf16()
            .chain([0])
            .collect::<Vec<_>>();
        let title = format!("{title} input")
            .encode_utf16()
            .chain([0])
            .collect::<Vec<_>>();
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(interaction_window_proc),
            hInstance: instance,
            lpszClassName: class_name.as_ptr(),
            ..unsafe { zeroed() }
        };
        if unsafe { RegisterClassW(&class) } == 0 {
            return Err("RegisterClassW failed for interaction window".to_owned());
        }

        let handle = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_NOREDIRECTIONBITMAP,
                class_name.as_ptr(),
                title.as_ptr(),
                WS_POPUP,
                0,
                0,
                width,
                height,
                null_mut(),
                null_mut(),
                instance,
                null(),
            )
        };
        if handle.is_null() {
            unsafe { UnregisterClassW(class_name.as_ptr(), instance) };
            return Err("CreateWindowExW failed for interaction window".to_owned());
        }
        if unsafe { SetLayeredWindowAttributes(handle, 0, INTERACTION_WINDOW_ALPHA, LWA_ALPHA) }
            == 0
        {
            unsafe {
                DestroyWindow(handle);
                UnregisterClassW(class_name.as_ptr(), instance);
            }
            return Err("SetLayeredWindowAttributes failed for interaction window".to_owned());
        }

        let state = Arc::new(InteractionState::default());
        let raw_state = Arc::into_raw(Arc::clone(&state));
        unsafe { SetWindowLongPtrW(handle, GWLP_USERDATA, raw_state as isize) };
        Ok(Self {
            handle,
            instance,
            class_name,
            state,
        })
    }

    fn set_owner(&self, owner: HWND) -> Result<(), String> {
        unsafe { SetLastError(0) };
        let previous = unsafe { SetWindowLongPtrW(self.handle, GWLP_HWNDPARENT, owner as isize) };
        let error = unsafe { GetLastError() };
        if previous == 0 && error != 0 {
            return Err(format!(
                "SetWindowLongPtrW failed for interaction owner error={error}"
            ));
        }
        Ok(())
    }

    fn place(&self, bounds: GameBounds) -> Result<(), String> {
        let placed = unsafe {
            SetWindowPos(
                self.handle,
                HWND_TOP,
                bounds.x,
                bounds.y,
                bounds.width,
                bounds.height,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )
        };
        (placed != 0)
            .then_some(())
            .ok_or_else(|| "SetWindowPos failed for interaction window".to_owned())
    }

    fn update_regions(
        &self,
        regions: &[InteractiveRegion],
        scale: f32,
        addon_revision: u64,
    ) -> Result<(), String> {
        let physical = regions
            .iter()
            .filter_map(|region| {
                let left = (region.rect.left() * scale).floor() as i32;
                let top = (region.rect.top() * scale).floor() as i32;
                let right = (region.rect.right() * scale).ceil() as i32;
                let bottom = (region.rect.bottom() * scale).ceil() as i32;
                (right > left && bottom > top).then_some(PhysicalInteraction {
                    left,
                    top,
                    right,
                    bottom,
                    addon_revision,
                    action: region.action.clone(),
                })
            })
            .collect::<Vec<_>>();
        *self
            .state
            .regions
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = physical.clone();

        let combined = unsafe { CreateRectRgn(0, 0, 0, 0) };
        if combined.is_null() {
            return Err("CreateRectRgn failed for interaction region".to_owned());
        }
        for region in physical {
            let part =
                unsafe { CreateRectRgn(region.left, region.top, region.right, region.bottom) };
            if part.is_null() {
                unsafe { DeleteObject(combined) };
                return Err("CreateRectRgn failed for interaction target".to_owned());
            }
            unsafe {
                CombineRgn(combined, combined, part, RGN_OR);
                DeleteObject(part);
            }
        }
        if unsafe { SetWindowRgn(self.handle, combined, 1) } == 0 {
            unsafe { DeleteObject(combined) };
            return Err("SetWindowRgn failed for interaction window".to_owned());
        }
        Ok(())
    }

    fn take_clicks(&self) -> Vec<StampedClick> {
        self.state
            .clicks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain(..)
            .collect()
    }

    fn take_dropped_clicks(&self) -> u64 {
        self.state.take_dropped_clicks()
    }

    fn slider_preview(&self) -> Option<SliderAction> {
        self.state
            .slider_preview
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn slider_preview_revision(&self) -> u64 {
        self.state.slider_preview_revision.load(Ordering::Acquire)
    }

    fn show(&self) {
        unsafe { ShowWindow(self.handle, SW_SHOWNOACTIVATE) };
    }

    fn hide(&self) {
        unsafe { ShowWindow(self.handle, SW_HIDE) };
    }
}

impl Drop for InteractionWindow {
    fn drop(&mut self) {
        let raw_state = unsafe { SetWindowLongPtrW(self.handle, GWLP_USERDATA, 0) };
        unsafe {
            DestroyWindow(self.handle);
            UnregisterClassW(self.class_name.as_ptr(), self.instance);
        }
        if raw_state != 0 {
            unsafe { drop(Arc::from_raw(raw_state as *const InteractionState)) };
        }
    }
}

unsafe extern "system" fn interaction_window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_MOUSEACTIVATE => return MA_NOACTIVATE as LRESULT,
        WM_LBUTTONDOWN => {
            let x = (lparam as u16 as i16) as i32;
            let y = ((lparam >> 16) as u16 as i16) as i32;
            let state =
                unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *const InteractionState;
            if !state.is_null() && unsafe { &*state }.begin_slider_drag(x, y) {
                unsafe { SetCapture(window) };
            }
            return 0;
        }
        WM_MOUSEMOVE => {
            let x = (lparam as u16 as i16) as i32;
            let state =
                unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *const InteractionState;
            if !state.is_null() {
                unsafe { &*state }.update_slider_drag(x);
            }
            return 0;
        }
        WM_LBUTTONUP => {
            let x = (lparam as u16 as i16) as i32;
            let y = ((lparam >> 16) as u16 as i16) as i32;
            let state =
                unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *const InteractionState;
            if !state.is_null() {
                if unsafe { &*state }.finish_slider_drag(x) {
                    unsafe { ReleaseCapture() };
                } else {
                    unsafe { &*state }.click_at(x, y);
                }
            }
            return 0;
        }
        WM_CANCELMODE | WM_CAPTURECHANGED => {
            let state =
                unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *const InteractionState;
            if !state.is_null() {
                unsafe { &*state }.cancel_slider_drag();
            }
            return 0;
        }
        _ => {}
    }
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if let Some(result) = noninteractive_message_result(message) {
        return result;
    }
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

fn overlay_extended_style() -> u32 {
    WS_EX_LAYERED
        | WS_EX_TRANSPARENT
        | WS_EX_TOOLWINDOW
        | WS_EX_NOACTIVATE
        | WS_EX_NOREDIRECTIONBITMAP
}

fn noninteractive_message_result(message: u32) -> Option<LRESULT> {
    match message {
        WM_NCHITTEST => Some(HTTRANSPARENT as LRESULT),
        WM_MOUSEACTIVATE => Some(MA_NOACTIVATE as LRESULT),
        _ => None,
    }
}

struct DcompRenderer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    swap_chain: IDXGISwapChain1,
    _composition_device: IDCompositionDevice,
    _composition_target: IDCompositionTarget,
    _composition_visual: IDCompositionVisual,
    egui: egui::Context,
    images: HashMap<String, egui::TextureHandle>,
    painter: Painter,
    width: u32,
    height: u32,
    started: Instant,
}

impl DcompRenderer {
    fn new(window: HWND, width: u32, height: u32) -> Result<Self, String> {
        let (device, context) = create_device()?;
        let dxgi_device: IDXGIDevice = device
            .cast()
            .map_err(|error| format!("D3D11 device does not expose IDXGIDevice: {error}"))?;
        let adapter = unsafe { dxgi_device.GetAdapter() }
            .map_err(|error| format!("IDXGIDevice::GetAdapter failed: {error}"))?;
        let factory: IDXGIFactory2 = unsafe { adapter.GetParent() }
            .map_err(|error| format!("IDXGIAdapter::GetParent failed: {error}"))?;
        let description = swap_chain_description(width, height);
        let swap_chain =
            unsafe { factory.CreateSwapChainForComposition(&device, &description, None) }
                .map_err(|error| format!("CreateSwapChainForComposition failed: {error}"))?;

        let composition_device: IDCompositionDevice =
            unsafe { DCompositionCreateDevice(&dxgi_device) }
                .map_err(|error| format!("DCompositionCreateDevice failed: {error}"))?;
        let composition_target =
            unsafe { composition_device.CreateTargetForHwnd(WinHwnd(window), WinBool(1)) }
                .map_err(|error| format!("CreateTargetForHwnd failed: {error}"))?;
        let composition_visual = unsafe { composition_device.CreateVisual() }
            .map_err(|error| format!("CreateVisual failed: {error}"))?;
        unsafe {
            composition_visual
                .SetContent(&swap_chain)
                .map_err(|error| format!("DirectComposition SetContent failed: {error}"))?;
            composition_target
                .SetRoot(&composition_visual)
                .map_err(|error| format!("DirectComposition SetRoot failed: {error}"))?;
            composition_device
                .Commit()
                .map_err(|error| format!("DirectComposition Commit failed: {error}"))?;
        }

        let egui = egui::Context::default();
        egui.set_visuals(super::farever_visuals());
        let painter = Painter::new(&device)?;

        Ok(Self {
            device,
            context,
            swap_chain,
            _composition_device: composition_device,
            _composition_target: composition_target,
            _composition_visual: composition_visual,
            egui,
            images: HashMap::new(),
            painter,
            width,
            height,
            started: Instant::now(),
        })
    }

    fn resize(&mut self, width: u32, height: u32) -> Result<(), String> {
        if self.width == width && self.height == height {
            return Ok(());
        }
        unsafe {
            self.context.ClearState();
            self.context.Flush();
            self.swap_chain.ResizeBuffers(
                0,
                width,
                height,
                DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG(0),
            )
        }
        .map_err(|error| format!("DirectComposition swap-chain resize failed: {error}"))?;
        self.width = width;
        self.height = height;
        Ok(())
    }

    fn render(
        &mut self,
        frame: &farever_more_api::UiFrame,
        game_menu_open: bool,
        game_menu_geometry: Option<super::GameMenuGeometry>,
        slash_commands_enabled: bool,
        config_host: &ConfigHostState,
    ) -> Result<Vec<InteractiveRegion>, String> {
        let raw_input = egui_input(
            self.width,
            self.height,
            self.started.elapsed().as_secs_f64(),
        );
        let mut interactions = Vec::new();
        let output = self.egui.run(raw_input, |context| {
            interactions = render_interactive_frame(
                context,
                frame,
                &self.images,
                game_menu_open,
                game_menu_geometry,
                slash_commands_enabled,
                config_host,
            );
        });

        let back_buffer: ID3D11Texture2D = unsafe { self.swap_chain.GetBuffer(0) }
            .map_err(|error| format!("DirectComposition GetBuffer failed: {error}"))?;
        let mut target: Option<ID3D11RenderTargetView> = None;
        unsafe {
            self.device
                .CreateRenderTargetView(&back_buffer, None, Some(&mut target))
        }
        .map_err(|error| format!("CreateRenderTargetView failed: {error}"))?;
        let target = target.ok_or_else(|| "CreateRenderTargetView returned null".to_owned())?;
        let viewport = D3D11_VIEWPORT {
            Width: self.width as f32,
            Height: self.height as f32,
            MaxDepth: 1.0,
            ..Default::default()
        };
        unsafe {
            self.context.OMSetRenderTargets(
                Some(&[Some(target.clone())]),
                None::<&ID3D11DepthStencilView>,
            );
            self.context.RSSetViewports(Some(&[viewport]));
            self.context
                .ClearRenderTargetView(&target, &[0.0, 0.0, 0.0, 0.0]);
        }
        self.painter.paint(
            &self.device,
            &self.context,
            &self.egui,
            output,
            [self.width, self.height],
        )?;
        unsafe {
            self.context
                .OMSetRenderTargets(None, None::<&ID3D11DepthStencilView>);
            self.context.Flush();
            self.swap_chain.Present(1, DXGI_PRESENT(0)).ok()
        }
        .map_err(|error| format!("DirectComposition Present failed: {error}"))?;
        Ok(interactions)
    }

    fn ui_scale(&self) -> f32 {
        resolution_ui_scale(self.width, self.height)
    }

    fn install_fonts(&mut self, assets: &[farever_more_api::FontAsset]) {
        let mut definitions = egui::FontDefinitions::default();
        let proportional_fallbacks = definitions
            .families
            .get(&egui::FontFamily::Proportional)
            .cloned()
            .unwrap_or_default();
        for asset in assets {
            definitions.font_data.insert(
                asset.family.clone(),
                Arc::new(egui::FontData::from_owned(asset.bytes.to_vec())),
            );
            let mut family = vec![asset.family.clone()];
            family.extend(proportional_fallbacks.iter().cloned());
            definitions
                .families
                .insert(egui::FontFamily::Name(asset.family.clone().into()), family);
        }
        self.egui.set_fonts(definitions);
    }

    fn install_images(&mut self, assets: &[farever_more_api::ImageAsset]) {
        let active = assets
            .iter()
            .map(|asset| asset.id.as_str())
            .collect::<HashSet<_>>();
        self.images.retain(|id, _| active.contains(id.as_str()));
        for asset in assets {
            let image = egui::ColorImage::from_rgba_unmultiplied(
                [asset.width as usize, asset.height as usize],
                &asset.rgba,
            );
            if let Some(texture) = self.images.get_mut(&asset.id) {
                texture.set(image, egui::TextureOptions::LINEAR);
            } else {
                let texture =
                    self.egui
                        .load_texture(asset.id.clone(), image, egui::TextureOptions::LINEAR);
                self.images.insert(asset.id.clone(), texture);
            }
        }
    }
}

fn resolution_ui_scale(width: u32, height: u32) -> f32 {
    // The smaller axis controls scale, so ultrawide and non-16:9 windows gain
    // extra usable space without making the overlay larger than intended.
    let width_scale = width as f32 / UI_REFERENCE_WIDTH;
    let height_scale = height as f32 / UI_REFERENCE_HEIGHT;
    width_scale
        .min(height_scale)
        .clamp(MIN_UI_SCALE, MAX_UI_SCALE)
}

fn egui_input(width: u32, height: u32, time: f64) -> egui::RawInput {
    let ui_scale = resolution_ui_scale(width, height);
    // egui's input and mesh coordinates are logical points. Pixels-per-point
    // controls font rasterization and tells the painter how those points map
    // back to the physical Direct3D target.
    let logical_size = egui::vec2(width as f32 / ui_scale, height as f32 / ui_scale);
    let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, logical_size);
    let mut input = egui::RawInput {
        screen_rect: Some(screen_rect),
        time: Some(time),
        predicted_dt: 0.1,
        focused: false,
        ..Default::default()
    };
    let viewport = input
        .viewports
        .get_mut(&input.viewport_id)
        .expect("RawInput always contains its root viewport");
    viewport.native_pixels_per_point = Some(ui_scale);
    viewport.inner_rect = Some(screen_rect);
    input
}

fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext), String> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            WinHmodule::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .map_err(|error| format!("D3D11CreateDevice failed: {error}"))?;
    Ok((
        device.ok_or_else(|| "D3D11CreateDevice returned no device".to_owned())?,
        context.ok_or_else(|| "D3D11CreateDevice returned no context".to_owned())?,
    ))
}

fn swap_chain_description(width: u32, height: u32) -> DXGI_SWAP_CHAIN_DESC1 {
    DXGI_SWAP_CHAIN_DESC1 {
        Width: width,
        Height: height,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        Stereo: WinBool(0),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
        AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
        Flags: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_is_layered_transparent_nonactivating_and_non_topmost() {
        let style = overlay_extended_style();
        assert_ne!(style & WS_EX_LAYERED, 0);
        assert_ne!(style & WS_EX_TRANSPARENT, 0);
        assert_ne!(style & WS_EX_NOACTIVATE, 0);
        assert_eq!(style & WS_EX_TOPMOST, 0);
    }

    #[test]
    fn overlay_refuses_mouse_hit_testing_and_activation() {
        assert_eq!(
            noninteractive_message_result(WM_NCHITTEST),
            Some(HTTRANSPARENT as LRESULT)
        );
        assert_eq!(
            noninteractive_message_result(WM_MOUSEACTIVATE),
            Some(MA_NOACTIVATE as LRESULT)
        );
    }

    #[test]
    fn resolution_scale_uses_1080p_as_the_reference() {
        assert_eq!(resolution_ui_scale(1280, 720), 1.0);
        assert_eq!(resolution_ui_scale(1920, 1080), 1.0);
        assert!((resolution_ui_scale(2560, 1440) - 4.0 / 3.0).abs() < f32::EPSILON);
        assert_eq!(resolution_ui_scale(3840, 2160), 2.0);
    }

    #[test]
    fn game_menu_geometry_accepts_scaled_physical_client_rectangles() {
        let bounds = GameBounds {
            x: 0,
            y: 0,
            width: 2507,
            height: 1376,
        };
        assert!(game_menu_geometry_is_plausible(
            bounds,
            super::super::GameMenuGeometry {
                x: 187.0,
                y: 275.0,
                width: 493.0,
                height: 714.0,
            }
        ));

        assert!(game_menu_geometry_is_plausible(
            bounds,
            super::super::GameMenuGeometry {
                x: 147.0,
                y: 216.0,
                width: 387.0,
                height: 560.0,
            }
        ));
    }

    #[test]
    fn game_menu_geometry_must_fit_inside_the_current_client() {
        let bounds = GameBounds {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        assert!(!game_menu_geometry_is_plausible(
            bounds,
            super::super::GameMenuGeometry {
                x: 1700.0,
                y: 700.0,
                width: 493.0,
                height: 714.0,
            }
        ));
    }

    #[test]
    fn ultrawide_scale_is_limited_by_the_shorter_axis() {
        assert!((resolution_ui_scale(3440, 1440) - 4.0 / 3.0).abs() < f32::EPSILON);
    }

    #[test]
    fn resized_input_updates_scale_while_preserving_reference_layout() {
        let initial = egui_input(1920, 1080, 0.0);
        let resized = egui_input(3840, 2160, 1.0);

        assert_eq!(initial.viewport().native_pixels_per_point, Some(1.0));
        assert_eq!(resized.viewport().native_pixels_per_point, Some(2.0));
        assert_eq!(initial.screen_rect, resized.screen_rect);
    }

    #[test]
    fn resize_reports_the_settled_size_once() {
        let mut reporter = ResizeReporter::new(Duration::from_millis(20));
        assert_eq!(reporter.observe((1280, 720)), None);
        assert_eq!(reporter.observe((1300, 720)), None);
        assert_eq!(reporter.observe((1300, 720)), None, "burst is not reported");

        thread::sleep(Duration::from_millis(25));
        assert_eq!(reporter.observe((1300, 720)), Some((1300, 720)));
        assert_eq!(reporter.observe((1300, 720)), None, "reported once");

        // A later, different size is reported after it settles again.
        assert_eq!(reporter.observe((1600, 900)), None);
        thread::sleep(Duration::from_millis(25));
        assert_eq!(reporter.observe((1600, 900)), Some((1600, 900)));

        // Hiding the overlay abandons whatever was pending.
        reporter.reset();
        assert_eq!(reporter.observe((1600, 900)), None);
    }

    #[test]
    fn config_actions_emit_visibility_edges_and_validated_control_events() {
        let menu = |owner: &str, id: &str| farever_more_api::ConfigMenu {
            owner: owner.to_owned(),
            id: id.to_owned(),
            title: id.to_owned(),
            nodes: vec![
                farever_more_api::UiNode {
                    id: "run".to_owned(),
                    parent: None,
                    widget: farever_more_api::Widget::Button(farever_more_api::ButtonWidget {
                        label: "Run".to_owned(),
                        enabled: true,
                    }),
                },
                farever_more_api::UiNode {
                    id: "enabled".to_owned(),
                    parent: None,
                    widget: farever_more_api::Widget::Checkbox(farever_more_api::CheckboxWidget {
                        label: "Enabled".to_owned(),
                        checked: true,
                        enabled: true,
                    }),
                },
                farever_more_api::UiNode {
                    id: "theme".to_owned(),
                    parent: None,
                    widget: farever_more_api::Widget::Dropdown(farever_more_api::DropdownWidget {
                        label: "Theme".to_owned(),
                        selected_id: "warm".to_owned(),
                        options: vec![
                            farever_more_api::DropdownOption {
                                id: "warm".to_owned(),
                                label: "Warm".to_owned(),
                            },
                            farever_more_api::DropdownOption {
                                id: "cool".to_owned(),
                                label: "Cool".to_owned(),
                            },
                        ],
                        enabled: true,
                    }),
                },
                farever_more_api::UiNode {
                    id: "scale".to_owned(),
                    parent: None,
                    widget: farever_more_api::Widget::Slider(farever_more_api::SliderWidget {
                        label: "Scale".to_owned(),
                        value: 1.0,
                        minimum: 0.5,
                        maximum: 2.0,
                        step: Some(0.25),
                        enabled: true,
                    }),
                },
            ],
            canvas: Vec::new(),
        };
        let frame = farever_more_api::UiFrame {
            surfaces: Vec::new(),
            config_menus: vec![menu("addon-a", "main"), menu("addon-b", "settings")],
        };
        let (sender, receiver) = mpsc::sync_channel(8);
        let (host_sender, host_receiver) = mpsc::sync_channel(8);
        let mut config_host = ConfigHostState::default();
        let mut queue_drops = UiQueueDrops::default();

        let button = farever_more_api::RoutedUiEvent {
            owner: "addon-b".to_owned(),
            event: farever_more_api::UiEvent::ButtonPressed {
                view: farever_more_api::UiView::ConfigMenu("settings".to_owned()),
                node_id: "run".to_owned(),
            },
        };
        let checkbox = farever_more_api::RoutedUiEvent {
            owner: "addon-b".to_owned(),
            event: farever_more_api::UiEvent::CheckboxChanged {
                node_id: "enabled".to_owned(),
                checked: false,
            },
        };
        assert!(!apply_click_action(
            ClickAction::AddonButton(button.clone()),
            &frame,
            true,
            &mut config_host,
            &sender,
            &host_sender,
            &mut queue_drops,
        ));
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        assert!(!apply_click_action(
            ClickAction::AddonCheckbox(checkbox.clone()),
            &frame,
            true,
            &mut config_host,
            &sender,
            &host_sender,
            &mut queue_drops,
        ));
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        assert!(apply_click_action(
            ClickAction::OpenConfigMenu,
            &frame,
            true,
            &mut config_host,
            &sender,
            &host_sender,
            &mut queue_drops,
        ));
        assert_eq!(config_host.active_menu, Some(host_config_menu_key()));
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        assert!(!apply_click_action(
            ClickAction::SetSlashCommandsEnabled(false),
            &frame,
            true,
            &mut config_host,
            &sender,
            &host_sender,
            &mut queue_drops,
        ));
        assert_eq!(
            host_receiver.try_recv().expect("host setting event"),
            HostUiEvent::SetSlashCommandsEnabled(false)
        );

        let first = ConfigMenuKey {
            owner: "addon-a".to_owned(),
            id: "main".to_owned(),
        };
        assert!(apply_click_action(
            ClickAction::SelectConfigMenu(first),
            &frame,
            true,
            &mut config_host,
            &sender,
            &host_sender,
            &mut queue_drops,
        ));
        assert_eq!(
            receiver.try_recv().expect("shown event"),
            farever_more_api::RoutedUiEvent {
                owner: "addon-a".to_owned(),
                event: farever_more_api::UiEvent::ConfigMenuShown("main".to_owned()),
            }
        );

        let second = ConfigMenuKey {
            owner: "addon-b".to_owned(),
            id: "settings".to_owned(),
        };
        assert!(apply_click_action(
            ClickAction::SelectConfigMenu(second.clone()),
            &frame,
            true,
            &mut config_host,
            &sender,
            &host_sender,
            &mut queue_drops,
        ));
        assert!(matches!(
            receiver.try_recv().expect("hidden event").routed.event,
            farever_more_api::UiEvent::ConfigMenuHidden(id) if id == "main"
        ));
        assert!(matches!(
            receiver.try_recv().expect("shown event").routed.event,
            farever_more_api::UiEvent::ConfigMenuShown(id) if id == "settings"
        ));

        assert!(!apply_click_action(
            ClickAction::AddonButton(button.clone()),
            &frame,
            true,
            &mut config_host,
            &sender,
            &host_sender,
            &mut queue_drops,
        ));
        assert_eq!(receiver.try_recv().expect("button event"), button);
        assert!(!apply_click_action(
            ClickAction::AddonCheckbox(checkbox.clone()),
            &frame,
            true,
            &mut config_host,
            &sender,
            &host_sender,
            &mut queue_drops,
        ));
        assert_eq!(receiver.try_recv().expect("checkbox event"), checkbox);

        let dropdown_control = ConfigControlKey {
            owner: "addon-b".to_owned(),
            menu_id: "settings".to_owned(),
            node_id: "theme".to_owned(),
        };
        assert!(apply_click_action(
            ClickAction::ToggleAddonDropdown(dropdown_control.clone()),
            &frame,
            true,
            &mut config_host,
            &sender,
            &host_sender,
            &mut queue_drops,
        ));
        assert_eq!(config_host.open_dropdown, Some(dropdown_control.clone()));
        let dropdown = farever_more_api::RoutedUiEvent {
            owner: "addon-b".to_owned(),
            event: farever_more_api::UiEvent::DropdownChanged {
                node_id: "theme".to_owned(),
                selected_id: "cool".to_owned(),
            },
        };
        assert!(apply_click_action(
            ClickAction::AddonDropdown(dropdown.clone()),
            &frame,
            true,
            &mut config_host,
            &sender,
            &host_sender,
            &mut queue_drops,
        ));
        assert_eq!(receiver.try_recv().expect("dropdown event"), dropdown);
        assert_eq!(config_host.open_dropdown, None);

        let slider = SliderAction {
            control: ConfigControlKey {
                owner: "addon-b".to_owned(),
                menu_id: "settings".to_owned(),
                node_id: "scale".to_owned(),
            },
            value: 1.5,
            minimum: 0.5,
            maximum: 2.0,
            step: Some(0.25),
        };
        assert!(!apply_click_action(
            ClickAction::AddonSlider(slider),
            &frame,
            true,
            &mut config_host,
            &sender,
            &host_sender,
            &mut queue_drops,
        ));
        assert_eq!(
            receiver.try_recv().expect("slider event"),
            farever_more_api::RoutedUiEvent {
                owner: "addon-b".to_owned(),
                event: farever_more_api::UiEvent::SliderChanged {
                    node_id: "scale".to_owned(),
                    value: 1.5,
                },
            }
        );

        let stale_checkbox = farever_more_api::RoutedUiEvent {
            owner: "addon-b".to_owned(),
            event: farever_more_api::UiEvent::CheckboxChanged {
                node_id: "enabled".to_owned(),
                checked: true,
            },
        };
        assert!(!apply_click_action(
            ClickAction::AddonCheckbox(stale_checkbox),
            &frame,
            true,
            &mut config_host,
            &sender,
            &host_sender,
            &mut queue_drops,
        ));
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        assert!(apply_click_action(
            ClickAction::CloseConfigMenu,
            &frame,
            true,
            &mut config_host,
            &sender,
            &host_sender,
            &mut queue_drops,
        ));
        assert_eq!(config_host, ConfigHostState::default());
        assert!(matches!(
            receiver.try_recv().expect("hidden event").routed.event,
            farever_more_api::UiEvent::ConfigMenuHidden(id) if id == "settings"
        ));
        assert!(matches!(
            host_receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn config_host_opens_and_closes_without_addon_pages() {
        let frame = farever_more_api::UiFrame::default();
        let (sender, receiver) = mpsc::sync_channel(1);
        let (host_sender, _host_receiver) = mpsc::sync_channel(1);
        let mut config_host = ConfigHostState::default();
        let mut queue_drops = UiQueueDrops::default();

        assert!(apply_click_action(
            ClickAction::OpenConfigMenu,
            &frame,
            true,
            &mut config_host,
            &sender,
            &host_sender,
            &mut queue_drops,
        ));
        assert_eq!(
            config_host,
            ConfigHostState {
                open: true,
                active_menu: Some(host_config_menu_key()),
                ..Default::default()
            }
        );
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        assert!(apply_click_action(
            ClickAction::CloseConfigMenu,
            &frame,
            true,
            &mut config_host,
            &sender,
            &host_sender,
            &mut queue_drops,
        ));
        assert_eq!(config_host, ConfigHostState::default());
    }

    #[test]
    fn canvas_presses_are_reported_in_canvas_local_points() {
        let frame = farever_more_api::UiFrame {
            surfaces: vec![farever_more_api::UiSurface {
                owner: "minimap".to_owned(),
                id: "map".to_owned(),
                title: "Minimap".to_owned(),
                anchor: farever_more_api::SurfaceAnchor::TopRight,
                margin_x: 0.0,
                margin_y: 0.0,
                width: Some(236.0),
                style: None,
                nodes: vec![farever_more_api::UiNode {
                    id: "minimap-canvas".to_owned(),
                    parent: None,
                    widget: farever_more_api::Widget::Canvas(farever_more_api::Size {
                        width: 236.0,
                        height: 236.0,
                    }),
                }],
                canvas: Vec::new(),
            }],
            config_menus: Vec::new(),
        };
        let (sender, receiver) = mpsc::sync_channel(4);
        let (host_sender, _host_receiver) = mpsc::sync_channel(1);
        let mut config_host = ConfigHostState::default();
        let mut queue_drops = UiQueueDrops::default();
        let action = |node_id: &str| {
            ClickAction::AddonCanvas(CanvasAction {
                owner: "minimap".to_owned(),
                view: farever_more_api::UiView::Surface("map".to_owned()),
                node_id: node_id.to_owned(),
                origin: [100.0, 50.0],
                pixels_per_point: 2.0,
            })
        };

        // Physical (300, 200) at scale 2 is logical (150, 100); relative to the
        // canvas origin that is (50, 50) inside the canvas.
        apply_click_action(
            StampedClick {
                addon_revision: 3,
                action: action("minimap-canvas"),
                position: Some([300.0, 200.0]),
            },
            &frame,
            false,
            &mut config_host,
            &sender,
            &host_sender,
            &mut queue_drops,
        );
        let routed = receiver.try_recv().expect("canvas event");
        assert_eq!(routed.addon_revision, 3);
        assert_eq!(routed.owner, "minimap");
        assert_eq!(
            routed.event,
            farever_more_api::UiEvent::CanvasPressed {
                view: farever_more_api::UiView::Surface("map".to_owned()),
                node_id: "minimap-canvas".to_owned(),
                x: 50.0,
                y: 50.0,
            }
        );

        // A frame that no longer holds that canvas drops the press instead.
        apply_click_action(
            StampedClick {
                addon_revision: 4,
                action: action("gone"),
                position: Some([300.0, 200.0]),
            },
            &frame,
            false,
            &mut config_host,
            &sender,
            &host_sender,
            &mut queue_drops,
        );
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn interaction_click_queue_is_bounded() {
        let state = InteractionState::default();
        state
            .regions
            .write()
            .expect("regions")
            .push(PhysicalInteraction {
                left: 0,
                top: 0,
                right: 10,
                bottom: 10,
                addon_revision: 7,
                action: ClickAction::OpenConfigMenu,
            });

        for _ in 0..(MAX_PENDING_UI_CLICKS + 10) {
            state.click_at(5, 5);
        }

        assert_eq!(
            state.clicks.lock().expect("clicks").len(),
            MAX_PENDING_UI_CLICKS
        );
        assert_eq!(state.take_dropped_clicks(), 10);
        assert_eq!(state.take_dropped_clicks(), 0);
    }

    #[test]
    fn slider_drag_previews_and_commits_one_quantized_value() {
        let state = InteractionState::default();
        let slider = SliderAction {
            control: ConfigControlKey {
                owner: "addon".to_owned(),
                menu_id: "settings".to_owned(),
                node_id: "scale".to_owned(),
            },
            value: 0.0,
            minimum: 0.0,
            maximum: 10.0,
            step: Some(2.0),
        };
        state
            .regions
            .write()
            .expect("regions")
            .push(PhysicalInteraction {
                left: 10,
                top: 20,
                right: 110,
                bottom: 40,
                addon_revision: 7,
                action: ClickAction::AddonSlider(slider),
            });

        assert!(state.begin_slider_drag(35, 30));
        assert_eq!(
            state.slider_preview.read().unwrap().as_ref().unwrap().value,
            2.0
        );
        assert!(state.update_slider_drag(78));
        assert_eq!(
            state.slider_preview.read().unwrap().as_ref().unwrap().value,
            6.0
        );
        assert!(state.finish_slider_drag(96));
        assert!(state.slider_preview.read().unwrap().is_none());
        let actions = state
            .clicks
            .lock()
            .expect("clicks")
            .drain(..)
            .collect::<Vec<_>>();
        assert!(matches!(
            actions.as_slice(),
            [StampedClick {
                addon_revision: 7,
                action: ClickAction::AddonSlider(SliderAction { value, .. }),
                ..
            }] if *value == 8.0
        ));
    }

    #[test]
    fn addon_ui_event_queue_overflow_is_counted() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        let event = farever_more_api::RoutedUiEvent {
            owner: "addon-a".to_owned(),
            event: farever_more_api::UiEvent::ConfigMenuShown("main".to_owned()),
        };
        let mut queue_drops = UiQueueDrops::default();

        send_addon_ui_event(&sender, 7, event.clone(), &mut queue_drops);
        send_addon_ui_event(&sender, 7, event, &mut queue_drops);

        assert_eq!(queue_drops.addon_events, 1);
    }
}

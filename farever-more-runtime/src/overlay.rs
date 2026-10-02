mod dcomp;
mod painter;

use crate::diagnostics::Level;
use egui_extras::{Column, TableBuilder, TableRow};
use farever_more_api as api;
use std::cell::RefCell;
use std::collections::HashMap;
use std::mem::zeroed;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, RwLock};
use std::thread::{self, JoinHandle};
use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClientRect, GetWindow, GetWindowThreadProcessId, IsWindowVisible, GW_OWNER,
};

const SURFACE_VIEWPORT_GUTTER: f32 = 8.0;
const MIN_RESPONSIVE_SURFACE_WIDTH: f32 = 120.0;
const CONFIG_SHELL_SIZE: egui::Vec2 = egui::vec2(900.0, 600.0);
const CONFIG_NAV_WIDTH: f32 = 240.0;
const CONFIG_BODY_HEIGHT: f32 = 500.0;
const CONFIG_NAV_ROW_HEIGHT: f32 = 48.0;
const GAME_MENU_PANEL_PHYSICAL_SIZE: egui::Vec2 = egui::vec2(493.0, 714.0);
const GAME_MENU_GEAR_PHYSICAL_OFFSET: egui::Vec2 = egui::vec2(339.0, 598.0);
const GAME_MENU_ICON_PHYSICAL_SIZE: egui::Vec2 = egui::vec2(72.0, 72.0);
const MAX_PENDING_UI_EVENTS: usize = 64;
const MAX_PENDING_HOST_UI_EVENTS: usize = 16;
const HOST_CONFIG_OWNER: &str = "farever.host";
const HOST_CONFIG_MENU_ID: &str = "settings";

// Farever's UI uses warm, low-contrast parchment surfaces with restrained
// cocoa accents. Keep this palette host-owned so add-ons that rely on default
// widget chrome feel native without each component reproducing the theme.
const FAREVER_INK: egui::Color32 = egui::Color32::from_rgb(55, 41, 34);
const FAREVER_INK_MUTED: egui::Color32 = egui::Color32::from_rgb(112, 88, 77);
const FAREVER_PARCHMENT: egui::Color32 = egui::Color32::from_rgb(207, 188, 179);
const FAREVER_PARCHMENT_LIGHT: egui::Color32 = egui::Color32::from_rgb(235, 214, 204);
const FAREVER_CONTROL: egui::Color32 = egui::Color32::from_rgb(184, 158, 148);
const FAREVER_CONTROL_HOVERED: egui::Color32 = egui::Color32::from_rgb(198, 171, 161);
const FAREVER_CONTROL_ACTIVE: egui::Color32 = egui::Color32::from_rgb(155, 111, 86);
const FAREVER_CONTROL_DISABLED: egui::Color32 = egui::Color32::from_rgb(202, 181, 172);
const FAREVER_TEXT_DISABLED: egui::Color32 = egui::Color32::from_rgb(239, 225, 217);
const FAREVER_BORDER: egui::Color32 = egui::Color32::from_rgb(156, 121, 104);
const FAREVER_BORDER_SOFT: egui::Color32 = egui::Color32::from_rgb(184, 153, 140);
const FAREVER_CREAM: egui::Color32 = egui::Color32::from_rgb(249, 239, 232);
const FAREVER_GOLD: egui::Color32 = egui::Color32::from_rgb(226, 190, 72);
const FAREVER_TRACK: egui::Color32 = egui::Color32::from_rgb(91, 79, 72);

pub(super) fn farever_visuals() -> egui::Visuals {
    let mut visuals = egui::Visuals::light();
    visuals.dark_mode = false;
    visuals.panel_fill = egui::Color32::TRANSPARENT;
    visuals.window_fill = FAREVER_PARCHMENT;
    visuals.window_stroke = egui::Stroke::new(1.0_f32, FAREVER_BORDER);
    visuals.window_corner_radius = egui::CornerRadius::same(5);
    visuals.window_shadow = egui::epaint::Shadow {
        offset: [0, 4],
        blur: 10,
        spread: 1,
        color: egui::Color32::from_black_alpha(72),
    };
    visuals.menu_corner_radius = egui::CornerRadius::same(5);
    visuals.popup_shadow = visuals.window_shadow;
    visuals.selection = egui::style::Selection {
        bg_fill: FAREVER_CONTROL_ACTIVE,
        stroke: egui::Stroke::new(1.0_f32, FAREVER_CREAM),
    };
    visuals.faint_bg_color = FAREVER_PARCHMENT_LIGHT;
    visuals.extreme_bg_color = FAREVER_TRACK;
    visuals.text_edit_bg_color = Some(egui::Color32::from_rgb(225, 205, 196));
    visuals.code_bg_color = egui::Color32::from_rgb(195, 173, 164);
    visuals.hyperlink_color = egui::Color32::from_rgb(132, 86, 65);
    visuals.warn_fg_color = egui::Color32::from_rgb(151, 83, 56);
    visuals.error_fg_color = egui::Color32::from_rgb(151, 63, 52);
    visuals.widgets.noninteractive = egui::style::WidgetVisuals {
        bg_fill: FAREVER_PARCHMENT,
        weak_bg_fill: FAREVER_PARCHMENT,
        bg_stroke: egui::Stroke::new(1.0_f32, FAREVER_BORDER_SOFT),
        corner_radius: egui::CornerRadius::same(4),
        fg_stroke: egui::Stroke::new(1.0_f32, FAREVER_INK),
        expansion: 0.0,
    };
    visuals.widgets.inactive = egui::style::WidgetVisuals {
        bg_fill: FAREVER_CONTROL,
        weak_bg_fill: FAREVER_CONTROL,
        bg_stroke: egui::Stroke::new(1.0_f32, FAREVER_BORDER_SOFT),
        corner_radius: egui::CornerRadius::same(6),
        fg_stroke: egui::Stroke::new(1.0_f32, FAREVER_CREAM),
        expansion: 0.0,
    };
    visuals.widgets.hovered = egui::style::WidgetVisuals {
        bg_fill: FAREVER_CONTROL_HOVERED,
        weak_bg_fill: FAREVER_CONTROL_HOVERED,
        bg_stroke: egui::Stroke::new(1.5_f32, FAREVER_CREAM),
        corner_radius: egui::CornerRadius::same(6),
        fg_stroke: egui::Stroke::new(1.0_f32, FAREVER_CREAM),
        expansion: 1.0,
    };
    visuals.widgets.active = egui::style::WidgetVisuals {
        bg_fill: FAREVER_CONTROL_ACTIVE,
        weak_bg_fill: FAREVER_CONTROL_ACTIVE,
        bg_stroke: egui::Stroke::new(1.5_f32, FAREVER_GOLD),
        corner_radius: egui::CornerRadius::same(6),
        fg_stroke: egui::Stroke::new(1.0_f32, FAREVER_CREAM),
        expansion: 0.0,
    };
    visuals.widgets.open = visuals.widgets.active;
    visuals.slider_trailing_fill = true;
    visuals
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ConfigMenuKey {
    pub(super) owner: String,
    pub(super) id: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct ConfigHostState {
    pub(super) open: bool,
    pub(super) active_menu: Option<ConfigMenuKey>,
    pub(super) open_dropdown: Option<ConfigControlKey>,
    pub(super) slider_preview: Option<SliderAction>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ConfigControlKey {
    pub(super) owner: String,
    pub(super) menu_id: String,
    pub(super) node_id: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct SliderAction {
    pub(super) control: ConfigControlKey,
    pub(super) value: f64,
    pub(super) minimum: f64,
    pub(super) maximum: f64,
    pub(super) step: Option<f64>,
}

impl SliderAction {
    pub(super) fn value_at_fraction(&self, fraction: f64) -> f64 {
        let value = self.minimum + fraction.clamp(0.0, 1.0) * (self.maximum - self.minimum);
        self.step.map_or(value, |step| {
            let steps = ((value - self.minimum) / step).round();
            (self.minimum + steps * step).clamp(self.minimum, self.maximum)
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum ClickAction {
    OpenConfigMenu,
    SelectConfigMenu(ConfigMenuKey),
    CloseConfigMenu,
    SetSlashCommandsEnabled(bool),
    AddonButton(api::RoutedUiEvent),
    AddonCheckbox(api::RoutedUiEvent),
    ToggleAddonDropdown(ConfigControlKey),
    AddonDropdown(api::RoutedUiEvent),
    AddonSlider(SliderAction),
    AddonCanvas(CanvasAction),
}

/// A canvas that accepts presses. The event is built when a click lands,
/// because its payload carries the position inside the canvas.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct CanvasAction {
    pub(super) owner: String,
    pub(super) view: api::UiView,
    pub(super) node_id: String,
    /// Canvas top-left in logical points, and the scale it was drawn at, so a
    /// physical click can be reported in canvas-local points.
    pub(super) origin: [f32; 2],
    pub(super) pixels_per_point: f32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum HostUiEvent {
    SetSlashCommandsEnabled(bool),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct InteractiveRegion {
    pub(super) rect: egui::Rect,
    pub(super) action: ClickAction,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct StampedUiEvent {
    pub(super) addon_revision: u64,
    pub(super) routed: api::RoutedUiEvent,
}

impl std::ops::Deref for StampedUiEvent {
    type Target = api::RoutedUiEvent;

    fn deref(&self) -> &Self::Target {
        &self.routed
    }
}

impl PartialEq<api::RoutedUiEvent> for StampedUiEvent {
    fn eq(&self, other: &api::RoutedUiEvent) -> bool {
        self.routed == *other
    }
}

#[derive(Clone)]
pub(super) struct OverlayPacket {
    pub(super) target_pid: Option<u32>,
    pub(super) addon_revision: u64,
    pub(super) frame: api::UiFrame,
    pub(super) fonts: Arc<Vec<api::FontAsset>>,
    pub(super) images: Arc<Vec<api::ImageAsset>>,
    pub(super) visible: bool,
    pub(super) addon_surface_count: usize,
    pub(super) game_menu_open: bool,
    pub(super) game_menu_geometry: Option<GameMenuGeometry>,
    pub(super) game_menu_geometry_revision: u64,
    pub(super) slash_commands_enabled: bool,
}

impl Default for OverlayPacket {
    fn default() -> Self {
        Self {
            target_pid: None,
            addon_revision: 0,
            frame: api::UiFrame::default(),
            fonts: Arc::new(Vec::new()),
            images: Arc::new(Vec::new()),
            visible: false,
            addon_surface_count: 0,
            game_menu_open: false,
            game_menu_geometry: None,
            game_menu_geometry_revision: 0,
            slash_commands_enabled: crate::host_settings::DEFAULT_SLASH_COMMANDS_ENABLED,
        }
    }
}

pub(super) struct SharedState {
    pub(super) packet: RwLock<OverlayPacket>,
    pub(super) revision: AtomicU64,
    pub(super) running: AtomicBool,
    pub(super) stop: AtomicBool,
    pub(super) pixels_per_point: AtomicU32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct GameMenuGeometry {
    /// Candidate absolute Farever UI bounds; accepted by the renderer only
    /// when they are consistent with the current physical client space.
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct OverlayHostState {
    pub(crate) visible: bool,
    pub(crate) addon_surface_count: usize,
    pub(crate) game_menu_open: bool,
    pub(crate) game_menu_geometry: Option<GameMenuGeometry>,
    pub(crate) game_menu_geometry_revision: u64,
    pub(crate) slash_commands_enabled: bool,
}

impl Default for OverlayHostState {
    fn default() -> Self {
        Self {
            visible: false,
            addon_surface_count: 0,
            game_menu_open: false,
            game_menu_geometry: None,
            game_menu_geometry_revision: 0,
            slash_commands_enabled: crate::host_settings::DEFAULT_SLASH_COMMANDS_ENABLED,
        }
    }
}

pub struct Overlay {
    shared: Arc<SharedState>,
    diagnostics: mpsc::Receiver<(Level, String)>,
    ui_events: mpsc::Receiver<StampedUiEvent>,
    host_ui_events: mpsc::Receiver<HostUiEvent>,
    thread: Option<JoinHandle<()>>,
}

impl Overlay {
    pub fn create(title: &str) -> Self {
        let shared = Arc::new(SharedState {
            packet: RwLock::new(OverlayPacket::default()),
            revision: AtomicU64::new(0),
            running: AtomicBool::new(true),
            stop: AtomicBool::new(false),
            pixels_per_point: AtomicU32::new(0),
        });
        let (diagnostic_tx, diagnostics) = mpsc::channel();
        let (ui_event_tx, ui_events) = mpsc::sync_channel(MAX_PENDING_UI_EVENTS);
        let (host_ui_event_tx, host_ui_events) = mpsc::sync_channel(MAX_PENDING_HOST_UI_EVENTS);
        let thread_shared = Arc::clone(&shared);
        let title = title.to_owned();
        let thread = thread::spawn(move || {
            let result = dcomp::run(
                &title,
                Arc::clone(&thread_shared),
                diagnostic_tx.clone(),
                ui_event_tx,
                host_ui_event_tx,
            );
            thread_shared.running.store(false, Ordering::Release);
            thread_shared.pixels_per_point.store(0, Ordering::Release);
            if let Err(error) = result {
                let _ = diagnostic_tx
                    .send((Level::Error, format!("egui renderer stopped error={error}")));
            }
        });

        Self {
            shared,
            diagnostics,
            ui_events,
            host_ui_events,
            thread: Some(thread),
        }
    }

    pub fn take_diagnostics(&mut self) -> Vec<(Level, String)> {
        self.diagnostics.try_iter().collect()
    }

    pub(crate) fn pixels_per_point(&self) -> Option<f32> {
        let value = f32::from_bits(self.shared.pixels_per_point.load(Ordering::Acquire));
        (value.is_finite() && value > 0.0).then_some(value)
    }

    pub fn take_ui_events(&mut self, addon_revision: u64) -> Vec<api::RoutedUiEvent> {
        drain_ui_events(&self.ui_events, addon_revision)
    }

    pub fn take_host_ui_events(&mut self) -> Vec<HostUiEvent> {
        self.host_ui_events
            .try_iter()
            .take(MAX_PENDING_HOST_UI_EVENTS)
            .collect()
    }

    pub fn update(
        &mut self,
        target_pid: Option<u32>,
        addon_revision: u64,
        frame: &api::UiFrame,
        fonts: &Arc<Vec<api::FontAsset>>,
        images: &Arc<Vec<api::ImageAsset>>,
        host: OverlayHostState,
    ) -> bool {
        let packet = OverlayPacket {
            target_pid,
            addon_revision,
            frame: frame.clone(),
            fonts: Arc::clone(fonts),
            images: Arc::clone(images),
            visible: host.visible,
            addon_surface_count: host.addon_surface_count,
            game_menu_open: host.game_menu_open,
            game_menu_geometry: host.game_menu_geometry,
            game_menu_geometry_revision: host.game_menu_geometry_revision,
            slash_commands_enabled: host.slash_commands_enabled,
        };
        match self.shared.packet.write() {
            Ok(mut current) => *current = packet,
            Err(poisoned) => *poisoned.into_inner() = packet,
        }
        self.shared.revision.fetch_add(1, Ordering::Release);
        self.shared.running.load(Ordering::Acquire)
    }
}

fn drain_ui_events(
    receiver: &mpsc::Receiver<StampedUiEvent>,
    addon_revision: u64,
) -> Vec<api::RoutedUiEvent> {
    receiver
        .try_iter()
        .take(MAX_PENDING_UI_EVENTS)
        .filter(|stamped| stamped.addon_revision == addon_revision)
        .map(|stamped| stamped.routed)
        .collect()
}

impl Drop for Overlay {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
pub(super) fn render_frame(
    context: &egui::Context,
    frame: &api::UiFrame,
    images: &HashMap<String, egui::TextureHandle>,
) {
    let _ = render_interactive_frame(
        context,
        frame,
        images,
        false,
        None,
        false,
        &ConfigHostState::default(),
    );
}

pub(super) fn render_interactive_frame(
    context: &egui::Context,
    frame: &api::UiFrame,
    images: &HashMap<String, egui::TextureHandle>,
    game_menu_open: bool,
    game_menu_geometry: Option<GameMenuGeometry>,
    slash_commands_enabled: bool,
    config_host: &ConfigHostState,
) -> Vec<InteractiveRegion> {
    let interactions = RefCell::new(Vec::new());
    for surface in &frame.surfaces {
        let (anchor, offset) = surface_placement(surface);
        if let [node] = surface.nodes.as_slice() {
            if let api::Widget::PassiveCanvas(size) = node.widget {
                // Paint directly: a native-map annotation must have neither
                // Window's layout adjustments nor an input hit rectangle.
                let rect = anchor
                    .align_size_within_rect(
                        egui::vec2(size.width, size.height),
                        context.content_rect(),
                    )
                    .translate(offset);
                let painter = context
                    .layer_painter(egui::LayerId::new(
                        egui::Order::Middle,
                        egui::Id::new(("farever-passive-canvas", &surface.owner, &surface.id)),
                    ))
                    .with_clip_rect(rect.intersect(context.content_rect()));
                let primitives: Vec<_> = surface
                    .canvas
                    .iter()
                    .filter(|command| command.canvas == 0)
                    .map(|command| &command.primitive)
                    .collect();
                paint_canvas(&painter, rect.min, &primitives, images);
                continue;
            }
        }
        let mut window = egui::Window::new(&surface.title)
            .id(egui::Id::new((
                "farever-addon-surface",
                &surface.owner,
                &surface.id,
            )))
            .anchor(anchor, offset)
            .collapsible(false)
            .resizable(false)
            .movable(false);
        if let Some(style) = surface.style {
            let frame = egui::Frame::window(&context.style())
                .fill(egui_color(style.fill))
                .stroke(style.stroke.map_or(egui::Stroke::NONE, egui_stroke))
                .corner_radius(style.corner_radius)
                .inner_margin(style.padding)
                .shadow(egui::epaint::Shadow::NONE);
            window = window.title_bar(style.title_bar).frame(frame);
        }
        if let Some(width) = surface.width.map(|width| {
            responsive_surface_width(width, context.content_rect().width(), surface.margin_x)
        }) {
            window = window
                .default_width(width)
                .min_width(width)
                .max_width(width);
        }
        window.show(context, |ui| {
            render_surface(ui, surface, images, &interactions)
        });
    }
    render_config_host(
        context,
        frame,
        images,
        GameMenuRenderState {
            open: game_menu_open,
            geometry: game_menu_geometry,
            slash_commands_enabled,
        },
        config_host,
        &interactions,
    );
    interactions.into_inner()
}

#[derive(Clone, Copy)]
struct GameMenuRenderState {
    open: bool,
    geometry: Option<GameMenuGeometry>,
    slash_commands_enabled: bool,
}

fn render_config_host(
    context: &egui::Context,
    frame: &api::UiFrame,
    images: &HashMap<String, egui::TextureHandle>,
    game_menu: GameMenuRenderState,
    config_host: &ConfigHostState,
    interactions: &RefCell<Vec<InteractiveRegion>>,
) {
    if !game_menu.open {
        return;
    }

    if !config_host.open {
        let Some(geometry) = game_menu.geometry else {
            // The sampled open state can arrive before the hook observer has
            // projected the EscapeMenu bounds. Avoid one frame at the guessed
            // fallback position; the geometry revision triggers a redraw.
            return;
        };
        let pixels_per_point = context.pixels_per_point();
        let gear_rect = game_menu_gear_rect(context.content_rect(), pixels_per_point, geometry);
        egui::Area::new(egui::Id::new("farever-config-cog"))
            .fixed_pos(gear_rect.min)
            .constrain(false)
            .order(egui::Order::Foreground)
            .show(context, |ui| {
                let button = egui::Button::new(
                    egui::RichText::new("⚙")
                        .size(30.0 / pixels_per_point)
                        .color(FAREVER_CREAM),
                )
                .fill(FAREVER_CONTROL_ACTIVE)
                .stroke(egui::Stroke::new(1.5_f32 / pixels_per_point, FAREVER_GOLD))
                .corner_radius((8.0 / pixels_per_point).round() as u8);
                let response = ui.add_sized(gear_rect.size(), button);
                record_interaction(interactions, response.rect, ClickAction::OpenConfigMenu);
            });
        return;
    }

    let host_key = host_config_menu_key();
    let host_selected = config_host.active_menu.as_ref() == Some(&host_key);
    let active_menu = config_host.active_menu.as_ref().and_then(|active_key| {
        frame
            .config_menus
            .iter()
            .find(|menu| menu.owner == active_key.owner && menu.id == active_key.id)
    });
    let menu_groups = config_menu_groups(&frame.config_menus);

    let shell_outer_size = CONFIG_SHELL_SIZE + egui::vec2(24.0, 24.0);
    let shell_position = context.content_rect().center() - shell_outer_size * 0.5;
    egui::Area::new(egui::Id::new("farever-config-shell"))
        .fixed_pos(shell_position)
        .order(egui::Order::Foreground)
        .show(context, |ui| {
            let shell = egui::Frame::new()
                .fill(FAREVER_PARCHMENT)
                .stroke(egui::Stroke::new(1.5_f32, FAREVER_BORDER))
                .corner_radius(5.0)
                .shadow(egui::epaint::Shadow {
                    offset: [0, 5],
                    blur: 12,
                    spread: 1,
                    color: egui::Color32::from_black_alpha(84),
                })
                .inner_margin(12.0);
            let shell_response = shell.show(ui, |ui| {
                ui.set_min_size(CONFIG_SHELL_SIZE);
                ui.set_max_size(CONFIG_SHELL_SIZE);

                egui::Frame::new()
                    .fill(FAREVER_PARCHMENT_LIGHT)
                    .stroke(egui::Stroke::new(1.0_f32, FAREVER_BORDER_SOFT))
                    .corner_radius(4.0)
                    .inner_margin(8.0)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.set_min_height(42.0);
                            ui.add_space(6.0);
                            ui.vertical(|ui| {
                                ui.label(
                                    egui::RichText::new("Farever More")
                                        .size(18.0)
                                        .strong()
                                        .color(FAREVER_INK),
                                );
                                ui.label(
                                    egui::RichText::new("Add-on configuration")
                                        .small()
                                        .color(FAREVER_INK_MUTED),
                                );
                            });
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    let close = egui::Button::new(
                                        egui::RichText::new("×")
                                            .size(22.0)
                                            .strong()
                                            .color(FAREVER_INK_MUTED),
                                    )
                                    .frame(false);
                                    let response = ui.add_sized([34.0, 30.0], close);
                                    record_interaction(
                                        interactions,
                                        response.rect,
                                        ClickAction::CloseConfigMenu,
                                    );
                                },
                            );
                        });
                    });
                ui.separator();

                ui.horizontal_top(|ui| {
                    egui::Frame::new()
                        .fill(FAREVER_PARCHMENT_LIGHT)
                        .inner_margin(10.0)
                        .show(ui, |ui| {
                            ui.set_width(CONFIG_NAV_WIDTH - 20.0);
                            ui.set_height(CONFIG_BODY_HEIGHT);
                            ui.with_layout(
                                egui::Layout::top_down(egui::Align::Min),
                                |ui| {
                                    config_nav_section_label(ui, "Framework");
                                    render_config_nav_entry(
                                        ui,
                                        "FM",
                                        "Farever More",
                                        host_selected,
                                        ClickAction::SelectConfigMenu(host_key.clone()),
                                        interactions,
                                    );
                                    ui.add_space(12.0);
                                    config_nav_section_label(ui, "Add-ons");

                                    egui::ScrollArea::vertical()
                                        .id_salt("farever-config-navigation")
                                        .auto_shrink([false, false])
                                        .max_height(CONFIG_BODY_HEIGHT - 128.0)
                                        .show(ui, |ui| {
                                            ui.set_min_width(CONFIG_NAV_WIDTH - 28.0);
                                            if menu_groups.is_empty() {
                                                ui.label(
                                                    egui::RichText::new("No registered pages")
                                                        .small()
                                                    .color(FAREVER_INK_MUTED),
                                                );
                                            }
                                            for group in &menu_groups {
                                                let selected = config_host
                                                    .active_menu
                                                    .as_ref()
                                                    .is_some_and(|key| key.owner == group.owner);
                                                let target = config_host
                                                    .active_menu
                                                    .as_ref()
                                                    .filter(|key| key.owner == group.owner)
                                                    .cloned()
                                                    .unwrap_or_else(|| ConfigMenuKey {
                                                        owner: group.owner.to_owned(),
                                                        id: group.menus[0].id.clone(),
                                                    });
                                                render_config_nav_entry(
                                                    ui,
                                                    &config_menu_initials(&group.label),
                                                    &group.label,
                                                    selected,
                                                    ClickAction::SelectConfigMenu(target),
                                                    interactions,
                                                );
                                            }
                                        });
                                },
                            );
                        });
                    ui.separator();

                    egui::Frame::new()
                        .fill(FAREVER_CREAM)
                        .inner_margin(22.0)
                        .show(ui, |ui| {
                            ui.set_width(CONFIG_SHELL_SIZE.x - CONFIG_NAV_WIDTH - 72.0);
                            ui.set_height(CONFIG_BODY_HEIGHT - 24.0);
                            ui.with_layout(
                                egui::Layout::top_down(egui::Align::Min),
                                |ui| {
                                    if host_selected {
                                        render_config_page_header(
                                            ui,
                                            "Framework / General",
                                            "Farever More",
                                        );
                                        ui.add_space(18.0);
                                        let response = ui
                                            .allocate_ui_with_layout(
                                                egui::vec2(ui.available_width(), 54.0),
                                                egui::Layout::left_to_right(egui::Align::Center),
                                                |ui| {
                                                    ui.vertical(|ui| {
                                                        ui.label(
                                                            egui::RichText::new(
                                                                "Enable slash commands",
                                                            )
                                                            .strong()
                                                            .color(FAREVER_INK),
                                                        );
                                                        ui.label(
                                                            egui::RichText::new(
                                                                "Allow add-ons to receive commands entered in Farever chat.",
                                                            )
                                                            .small()
                                                            .color(FAREVER_INK_MUTED),
                                                        );
                                                    });
                                                    ui.with_layout(
                                                        egui::Layout::right_to_left(
                                                            egui::Align::Center,
                                                        ),
                                                        |ui| {
                                                            render_toggle_switch(
                                                                ui,
                                                                game_menu.slash_commands_enabled,
                                                                true,
                                                            );
                                                        },
                                                    );
                                                },
                                            )
                                            .response;
                                        record_interaction(
                                            interactions,
                                            response.rect,
                                            ClickAction::SetSlashCommandsEnabled(
                                                !game_menu.slash_commands_enabled,
                                            ),
                                        );
                                    } else if let Some(active_menu) = active_menu {
                                        let active_group = menu_groups
                                            .iter()
                                            .find(|group| group.owner == active_menu.owner);
                                        let page_title = active_group
                                            .map_or(active_menu.title.as_str(), |group| {
                                                group.label.as_str()
                                            });
                                        render_config_page_header(
                                            ui,
                                            &format!("Add-ons / {page_title}"),
                                            page_title,
                                        );
                                        if let Some(group) =
                                            active_group.filter(|group| group.menus.len() > 1)
                                        {
                                            ui.add_space(10.0);
                                            render_config_page_tabs(
                                                ui,
                                                group,
                                                active_menu,
                                                interactions,
                                            );
                                        }
                                        ui.add_space(14.0);
                                        egui::ScrollArea::vertical()
                                            .id_salt((
                                                "farever-config-content",
                                                &active_menu.owner,
                                                &active_menu.id,
                                            ))
                                            .auto_shrink([false, false])
                                            .max_height(CONFIG_BODY_HEIGHT - 104.0)
                                            .show(ui, |ui| {
                                                ui.set_min_width(
                                                    CONFIG_SHELL_SIZE.x
                                                        - CONFIG_NAV_WIDTH
                                                        - 80.0,
                                                );
                                                render_document(
                                                    ui,
                                                    &active_menu.owner,
                                                    api::UiView::ConfigMenu(active_menu.id.clone()),
                                                    &active_menu.nodes,
                                                    &active_menu.canvas,
                                                    DocumentRenderResources {
                                                        images,
                                                        interactions,
                                                        config_host: Some(config_host),
                                                    },
                                                );
                                            });
                                    } else {
                                        ui.vertical_centered(|ui| {
                                            ui.add_space(170.0);
                                            ui.label(
                                                egui::RichText::new(
                                                    "No add-ons expose settings yet.",
                                                )
                                                .color(FAREVER_INK_MUTED),
                                            );
                                        });
                                    }
                                },
                            );
                        });
                });
            });
            paint_farever_panel_accents(ui, shell_response.response.rect);
        });
}

fn game_menu_gear_rect(
    viewport: egui::Rect,
    pixels_per_point: f32,
    game_menu_geometry: GameMenuGeometry,
) -> egui::Rect {
    let pixels_per_point = if pixels_per_point.is_finite() && pixels_per_point > 0.0 {
        pixels_per_point
    } else {
        1.0
    };
    let panel_physical_position = egui::vec2(game_menu_geometry.x, game_menu_geometry.y);
    let relative_scale = egui::vec2(
        game_menu_geometry.width / GAME_MENU_PANEL_PHYSICAL_SIZE.x,
        game_menu_geometry.height / GAME_MENU_PANEL_PHYSICAL_SIZE.y,
    );
    let gear_physical_position =
        panel_physical_position + GAME_MENU_GEAR_PHYSICAL_OFFSET * relative_scale;
    egui::Rect::from_min_size(
        viewport.min + gear_physical_position / pixels_per_point,
        GAME_MENU_ICON_PHYSICAL_SIZE * relative_scale / pixels_per_point,
    )
}

struct ConfigMenuGroup<'a> {
    owner: &'a str,
    label: String,
    menus: Vec<&'a api::ConfigMenu>,
}

fn config_menu_groups(menus: &[api::ConfigMenu]) -> Vec<ConfigMenuGroup<'_>> {
    let mut groups = Vec::<ConfigMenuGroup<'_>>::new();
    for menu in menus {
        if let Some(group) = groups.iter_mut().find(|group| group.owner == menu.owner) {
            group.menus.push(menu);
        } else {
            groups.push(ConfigMenuGroup {
                owner: &menu.owner,
                label: config_owner_label(&menu.owner),
                menus: vec![menu],
            });
        }
    }
    for group in &mut groups {
        group.menus.sort_by(|left, right| {
            left.title
                .to_lowercase()
                .cmp(&right.title.to_lowercase())
                .then_with(|| left.id.cmp(&right.id))
        });
        if group.menus.len() == 1 {
            group.label.clone_from(&group.menus[0].title);
        }
    }
    groups.sort_by(|left, right| {
        left.label
            .to_lowercase()
            .cmp(&right.label.to_lowercase())
            .then_with(|| left.owner.cmp(right.owner))
    });
    groups
}

fn config_owner_label(owner: &str) -> String {
    let words = owner
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .filter(|word| {
            !matches!(
                word.to_ascii_lowercase().as_str(),
                "addon" | "com" | "farever" | "org"
            )
        })
        .map(|word| {
            let mut characters = word.chars();
            let Some(first) = characters.next() else {
                return String::new();
            };
            first.to_uppercase().chain(characters).collect::<String>()
        })
        .collect::<Vec<_>>();
    if words.is_empty() {
        owner.to_owned()
    } else {
        words.join(" ")
    }
}

fn config_nav_section_label(ui: &mut egui::Ui, label: &str) {
    ui.label(
        egui::RichText::new(label.to_uppercase())
            .small()
            .strong()
            .color(FAREVER_INK_MUTED),
    );
    ui.add_space(3.0);
}

fn config_menu_initials(title: &str) -> String {
    let mut initials = title
        .split_whitespace()
        .filter_map(|word| word.chars().next())
        .flat_map(char::to_uppercase)
        .take(2)
        .collect::<String>();
    if initials.is_empty() {
        initials.push('A');
    }
    initials
}

fn render_config_nav_entry(
    ui: &mut egui::Ui,
    initials: &str,
    title: &str,
    selected: bool,
    action: ClickAction,
    interactions: &RefCell<Vec<InteractiveRegion>>,
) {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), CONFIG_NAV_ROW_HEIGHT),
        egui::Sense::click(),
    );
    let fill = if selected {
        FAREVER_CONTROL_ACTIVE
    } else if response.hovered() {
        FAREVER_CREAM.gamma_multiply(0.55)
    } else {
        egui::Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, 6.0, fill);
    if selected {
        ui.painter().rect_stroke(
            rect,
            6.0,
            egui::Stroke::new(1.0_f32, FAREVER_GOLD.gamma_multiply(0.7)),
            egui::StrokeKind::Inside,
        );
    }

    let badge = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 23.0, rect.center().y),
        egui::vec2(32.0, 32.0),
    );
    ui.painter().rect_filled(
        badge,
        6.0,
        if selected {
            FAREVER_CREAM
        } else {
            FAREVER_CONTROL
        },
    );
    ui.painter().text(
        badge.center(),
        egui::Align2::CENTER_CENTER,
        initials,
        egui::FontId::proportional(11.0),
        if selected {
            FAREVER_CONTROL_ACTIVE
        } else {
            FAREVER_CREAM
        },
    );

    let text_color = if selected { FAREVER_CREAM } else { FAREVER_INK };
    let painter = ui.painter().with_clip_rect(rect.shrink(4.0));
    painter.text(
        egui::pos2(rect.left() + 46.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        title,
        egui::FontId::proportional(14.0),
        text_color,
    );
    if let Some(rect) = clipped_interaction_rect(ui, response.rect) {
        record_interaction(interactions, rect, action);
    }
}

fn render_config_page_tabs(
    ui: &mut egui::Ui,
    group: &ConfigMenuGroup<'_>,
    active_menu: &api::ConfigMenu,
    interactions: &RefCell<Vec<InteractiveRegion>>,
) {
    ui.horizontal_wrapped(|ui| {
        for menu in &group.menus {
            let selected = menu.id == active_menu.id;
            let button = egui::Button::selectable(
                selected,
                egui::RichText::new(&menu.title).color(if selected {
                    FAREVER_CREAM
                } else {
                    FAREVER_INK
                }),
            )
            .corner_radius(6.0);
            let response = ui.add(button);
            if let Some(rect) = clipped_interaction_rect(ui, response.rect) {
                record_interaction(
                    interactions,
                    rect,
                    ClickAction::SelectConfigMenu(ConfigMenuKey {
                        owner: menu.owner.clone(),
                        id: menu.id.clone(),
                    }),
                );
            }
        }
    });
}

fn render_config_page_header(ui: &mut egui::Ui, breadcrumb: &str, title: &str) {
    ui.label(
        egui::RichText::new(breadcrumb)
            .small()
            .color(FAREVER_INK_MUTED),
    );
    ui.label(
        egui::RichText::new(title)
            .size(20.0)
            .strong()
            .color(FAREVER_CONTROL_ACTIVE),
    );
    ui.add_space(8.0);
    ui.separator();
}

/// Draws an add-on section header in the same visual language as the host's own
/// page headers, so an add-on settings page reads as one hierarchy.
fn render_section_header(ui: &mut egui::Ui, section: &api::SectionWidget) {
    ui.label(
        egui::RichText::new(&section.title)
            .size(16.0)
            .strong()
            .color(FAREVER_CONTROL_ACTIVE),
    );
    if let Some(description) = &section.description {
        ui.label(
            egui::RichText::new(description)
                .small()
                .color(FAREVER_INK_MUTED),
        );
    }
    ui.add_space(4.0);
    ui.separator();
    ui.add_space(2.0);
}

fn render_toggle_switch(ui: &mut egui::Ui, checked: bool, enabled: bool) -> egui::Response {
    let size = egui::vec2(46.0, 26.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let opacity = if enabled { 1.0 } else { 0.45 };
    let track_color = if checked {
        FAREVER_CONTROL_ACTIVE
    } else {
        FAREVER_CONTROL
    }
    .gamma_multiply(opacity);
    let stroke_color = if checked {
        FAREVER_GOLD.gamma_multiply(0.85)
    } else {
        FAREVER_BORDER_SOFT
    }
    .gamma_multiply(opacity);
    let radius = rect.height() * 0.5;
    ui.painter().rect(
        rect,
        radius,
        track_color,
        egui::Stroke::new(1.0_f32, stroke_color),
        egui::StrokeKind::Inside,
    );

    let knob_radius = radius - 4.0;
    let knob_x = if checked {
        rect.right() - radius
    } else {
        rect.left() + radius
    };
    let knob_center = egui::pos2(knob_x, rect.center().y);
    ui.painter().circle_filled(
        knob_center + egui::vec2(0.0, 1.0),
        knob_radius,
        egui::Color32::from_black_alpha(if enabled { 35 } else { 18 }),
    );
    ui.painter().circle_filled(
        knob_center,
        knob_radius,
        FAREVER_CREAM.gamma_multiply(opacity),
    );
    ui.painter().circle_stroke(
        knob_center,
        knob_radius,
        egui::Stroke::new(0.75_f32, FAREVER_BORDER_SOFT.gamma_multiply(opacity)),
    );
    response
}

fn render_farever_button(ui: &mut egui::Ui, label: &str, enabled: bool) -> egui::Response {
    let width = ui.available_width().max(1.0);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 50.0), egui::Sense::click());
    let fill = if enabled {
        if response.is_pointer_button_down_on() {
            FAREVER_CONTROL_ACTIVE
        } else if response.hovered() {
            FAREVER_CONTROL_HOVERED
        } else {
            FAREVER_CONTROL
        }
    } else {
        FAREVER_CONTROL_DISABLED
    };
    ui.painter().rect_filled(rect, 8.0, fill);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(17.0),
        if enabled {
            FAREVER_CREAM
        } else {
            FAREVER_TEXT_DISABLED
        },
    );
    response
}

pub(super) fn host_config_menu_key() -> ConfigMenuKey {
    ConfigMenuKey {
        owner: HOST_CONFIG_OWNER.to_owned(),
        id: HOST_CONFIG_MENU_ID.to_owned(),
    }
}

pub(super) fn is_host_config_menu(key: &ConfigMenuKey) -> bool {
    key.owner == HOST_CONFIG_OWNER && key.id == HOST_CONFIG_MENU_ID
}

fn paint_farever_panel_accents(ui: &egui::Ui, rect: egui::Rect) {
    let painter = ui.painter();
    painter.rect_stroke(
        rect.shrink(3.0),
        egui::CornerRadius::same(3),
        egui::Stroke::new(1.0_f32, FAREVER_BORDER_SOFT.gamma_multiply(0.55)),
        egui::StrokeKind::Inside,
    );
    for center in [
        egui::pos2(rect.left() + 8.0, rect.top() + 8.0),
        egui::pos2(rect.right() - 8.0, rect.top() + 8.0),
        egui::pos2(rect.left() + 8.0, rect.bottom() - 8.0),
        egui::pos2(rect.right() - 8.0, rect.bottom() - 8.0),
    ] {
        painter.circle_filled(center, 3.5, FAREVER_BORDER);
    }
}

fn record_interaction(
    interactions: &RefCell<Vec<InteractiveRegion>>,
    rect: egui::Rect,
    action: ClickAction,
) {
    interactions
        .borrow_mut()
        .push(InteractiveRegion { rect, action });
}

fn clipped_interaction_rect(ui: &egui::Ui, rect: egui::Rect) -> Option<egui::Rect> {
    let rect = rect.intersect(ui.clip_rect());
    (rect.width() > 0.0 && rect.height() > 0.0).then_some(rect)
}

fn responsive_surface_width(preferred: f32, viewport: f32, horizontal_margin: f32) -> f32 {
    let reserved = 2.0 * (horizontal_margin.abs() + SURFACE_VIEWPORT_GUTTER);
    preferred.min((viewport - reserved).max(MIN_RESPONSIVE_SURFACE_WIDTH))
}

fn surface_placement(surface: &api::UiSurface) -> (egui::Align2, egui::Vec2) {
    match surface.anchor {
        api::SurfaceAnchor::TopLeft => (
            egui::Align2::LEFT_TOP,
            egui::vec2(surface.margin_x, surface.margin_y),
        ),
        api::SurfaceAnchor::TopRight => (
            egui::Align2::RIGHT_TOP,
            egui::vec2(-surface.margin_x, surface.margin_y),
        ),
        api::SurfaceAnchor::BottomLeft => (
            egui::Align2::LEFT_BOTTOM,
            egui::vec2(surface.margin_x, -surface.margin_y),
        ),
        api::SurfaceAnchor::BottomRight => (
            egui::Align2::RIGHT_BOTTOM,
            egui::vec2(-surface.margin_x, -surface.margin_y),
        ),
        api::SurfaceAnchor::Center => (
            egui::Align2::CENTER_CENTER,
            egui::vec2(surface.margin_x, surface.margin_y),
        ),
        api::SurfaceAnchor::TopCenter => (
            egui::Align2::CENTER_TOP,
            egui::vec2(surface.margin_x, surface.margin_y),
        ),
    }
}

fn render_surface(
    ui: &mut egui::Ui,
    surface: &api::UiSurface,
    images: &HashMap<String, egui::TextureHandle>,
    interactions: &RefCell<Vec<InteractiveRegion>>,
) {
    render_document(
        ui,
        &surface.owner,
        api::UiView::Surface(surface.id.clone()),
        &surface.nodes,
        &surface.canvas,
        DocumentRenderResources {
            images,
            interactions,
            config_host: None,
        },
    );
}

#[derive(Clone, Copy)]
struct DocumentRenderResources<'a> {
    images: &'a HashMap<String, egui::TextureHandle>,
    interactions: &'a RefCell<Vec<InteractiveRegion>>,
    config_host: Option<&'a ConfigHostState>,
}

fn render_document(
    ui: &mut egui::Ui,
    owner: &str,
    view: api::UiView,
    nodes: &[api::UiNode],
    canvas_commands: &[api::CanvasCommand],
    resources: DocumentRenderResources<'_>,
) {
    let root = nodes.len();
    let mut children = vec![Vec::new(); nodes.len() + 1];
    for (index, node) in nodes.iter().enumerate() {
        children[node.parent.unwrap_or(root)].push(index);
    }
    let mut canvas = vec![Vec::new(); nodes.len()];
    for command in canvas_commands {
        canvas[command.canvas].push(&command.primitive);
    }
    render_children(
        ui,
        DocumentRenderContext {
            owner,
            view: &view,
            nodes,
            children: &children,
            canvas: &canvas,
            images: resources.images,
            interactions: resources.interactions,
            config_host: resources.config_host,
        },
        root,
    );
}

#[derive(Clone, Copy)]
struct DocumentRenderContext<'a> {
    owner: &'a str,
    view: &'a api::UiView,
    nodes: &'a [api::UiNode],
    children: &'a [Vec<usize>],
    canvas: &'a [Vec<&'a api::CanvasPrimitive>],
    images: &'a HashMap<String, egui::TextureHandle>,
    interactions: &'a RefCell<Vec<InteractiveRegion>>,
    config_host: Option<&'a ConfigHostState>,
}

fn render_children(ui: &mut egui::Ui, context: DocumentRenderContext<'_>, parent: usize) {
    for &child in &context.children[parent] {
        render_node(ui, context, child);
    }
}

fn render_node(ui: &mut egui::Ui, context: DocumentRenderContext<'_>, index: usize) {
    let node = &context.nodes[index];
    match &node.widget {
        api::Widget::Container(container) => {
            ui.scope(|ui| {
                if let Some(spacing) = container.spacing {
                    match container.direction {
                        api::LayoutDirection::Vertical => ui.spacing_mut().item_spacing.y = spacing,
                        api::LayoutDirection::Horizontal => {
                            ui.spacing_mut().item_spacing.x = spacing
                        }
                    }
                }
                let render = |ui: &mut egui::Ui| match container.direction {
                    api::LayoutDirection::Vertical => {
                        ui.vertical(|ui| render_children(ui, context, index));
                    }
                    api::LayoutDirection::Horizontal => {
                        ui.horizontal(|ui| render_children(ui, context, index));
                    }
                };
                match container.style {
                    api::ContainerStyle::Plain => render(ui),
                    api::ContainerStyle::Group => {
                        ui.group(render);
                    }
                    api::ContainerStyle::Scroll => {
                        let max_height = container.max_height.unwrap_or(360.0);
                        match container.direction {
                            api::LayoutDirection::Vertical => {
                                egui::ScrollArea::vertical()
                                    .max_height(max_height)
                                    .show(ui, render);
                            }
                            api::LayoutDirection::Horizontal => {
                                egui::ScrollArea::horizontal().show(ui, render);
                            }
                        }
                    }
                }
            });
        }
        api::Widget::Section(section) => {
            ui.vertical(|ui| {
                render_section_header(ui, section);
                render_children(ui, context, index);
            });
        }
        api::Widget::Text(text) => {
            let mut rich = egui::RichText::new(&text.text);
            if let Some(family) = &text.font_family {
                rich = rich.family(egui::FontFamily::Name(family.clone().into()));
            }
            rich = match text.style {
                api::TextStyle::Body => rich,
                api::TextStyle::Small => rich.small(),
                api::TextStyle::Strong => rich.strong(),
                api::TextStyle::Heading => rich.heading(),
                api::TextStyle::Monospace => rich.monospace(),
            };
            if let Some(color) = text.color {
                rich = rich.color(egui_color(color));
            }
            let label = egui::Label::new(rich);
            if let Some(outline) = text.outline.filter(|outline| outline.width > 0.0) {
                let label = if text.wrap { label.wrap() } else { label };
                let (position, galley, response) = label.selectable(false).layout_in_ui(ui);
                if ui.is_rect_visible(response.rect) {
                    let painter = ui.painter();
                    let outline_color = egui_color(outline.color);
                    for direction in [
                        egui::vec2(-1.0, 0.0),
                        egui::vec2(1.0, 0.0),
                        egui::vec2(0.0, -1.0),
                        egui::vec2(0.0, 1.0),
                        egui::vec2(-0.707_106_77, -0.707_106_77),
                        egui::vec2(0.707_106_77, -0.707_106_77),
                        egui::vec2(-0.707_106_77, 0.707_106_77),
                        egui::vec2(0.707_106_77, 0.707_106_77),
                    ] {
                        painter.galley_with_override_text_color(
                            position + direction * outline.width,
                            galley.clone(),
                            outline_color,
                        );
                    }
                    painter.galley(position, galley, ui.visuals().text_color());
                }
            } else if text.wrap {
                ui.add(label.wrap());
            } else {
                ui.add(label);
            }
        }
        api::Widget::Image(image) => {
            let size = pixel_aligned_size(
                egui::vec2(image.size.width, image.size.height),
                ui.pixels_per_point(),
            );
            if let Some(texture) = context.images.get(&image.source.id) {
                let mut widget = egui::Image::new((texture.id(), size));
                if let Some(tint) = image.tint {
                    widget = widget.tint(egui_color(tint));
                }
                let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
                widget.paint_at(ui, rect);
            } else {
                ui.allocate_space(size);
            }
        }
        api::Widget::Button(button) => {
            let response = render_farever_button(ui, &button.label, button.enabled);
            if button.enabled {
                if let Some(rect) = clipped_interaction_rect(ui, response.rect) {
                    context.interactions.borrow_mut().push(InteractiveRegion {
                        rect,
                        action: ClickAction::AddonButton(api::RoutedUiEvent {
                            owner: context.owner.to_owned(),
                            event: api::UiEvent::ButtonPressed {
                                view: context.view.clone(),
                                node_id: node.id.clone(),
                            },
                        }),
                    });
                }
            }
        }
        api::Widget::Checkbox(checkbox) => {
            let response = ui
                .allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), 34.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.label(
                            egui::RichText::new(&checkbox.label).color(if checkbox.enabled {
                                FAREVER_INK
                            } else {
                                FAREVER_INK_MUTED.gamma_multiply(0.65)
                            }),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            render_toggle_switch(ui, checkbox.checked, checkbox.enabled);
                        });
                    },
                )
                .response;
            if checkbox.enabled && matches!(context.view, api::UiView::ConfigMenu(_)) {
                if let Some(rect) = clipped_interaction_rect(ui, response.rect) {
                    context.interactions.borrow_mut().push(InteractiveRegion {
                        rect,
                        action: ClickAction::AddonCheckbox(api::RoutedUiEvent {
                            owner: context.owner.to_owned(),
                            event: api::UiEvent::CheckboxChanged {
                                node_id: node.id.clone(),
                                checked: !checkbox.checked,
                            },
                        }),
                    });
                }
            }
        }
        api::Widget::Dropdown(dropdown) => {
            let api::UiView::ConfigMenu(menu_id) = context.view else {
                return;
            };
            let control = ConfigControlKey {
                owner: context.owner.to_owned(),
                menu_id: menu_id.clone(),
                node_id: node.id.clone(),
            };
            let selected_label = dropdown
                .options
                .iter()
                .find(|option| option.id == dropdown.selected_id)
                .map_or(dropdown.selected_id.as_str(), |option| {
                    option.label.as_str()
                });
            ui.label(egui::RichText::new(&dropdown.label).strong());
            let response = ui.add_enabled(
                dropdown.enabled,
                egui::Button::new(format!("{selected_label}  ▾"))
                    .min_size(egui::vec2(ui.available_width(), 30.0)),
            );
            if dropdown.enabled {
                if let Some(rect) = clipped_interaction_rect(ui, response.rect) {
                    record_interaction(
                        context.interactions,
                        rect,
                        ClickAction::ToggleAddonDropdown(control.clone()),
                    );
                }
            }
            let is_open = context
                .config_host
                .and_then(|state| state.open_dropdown.as_ref())
                == Some(&control);
            if is_open {
                egui::Frame::new()
                    .fill(FAREVER_PARCHMENT_LIGHT)
                    .stroke(egui::Stroke::new(1.0_f32, FAREVER_BORDER_SOFT))
                    .corner_radius(4.0)
                    .inner_margin(4.0)
                    .show(ui, |ui| {
                        ui.set_min_width(response.rect.width() - 8.0);
                        for option in &dropdown.options {
                            let selected = option.id == dropdown.selected_id;
                            let option_response = ui.add_sized(
                                [ui.available_width(), 28.0],
                                egui::Button::selectable(selected, &option.label),
                            );
                            if !selected {
                                if let Some(rect) =
                                    clipped_interaction_rect(ui, option_response.rect)
                                {
                                    record_interaction(
                                        context.interactions,
                                        rect,
                                        ClickAction::AddonDropdown(api::RoutedUiEvent {
                                            owner: context.owner.to_owned(),
                                            event: api::UiEvent::DropdownChanged {
                                                node_id: node.id.clone(),
                                                selected_id: option.id.clone(),
                                            },
                                        }),
                                    );
                                }
                            }
                        }
                    });
            }
        }
        api::Widget::Slider(slider) => {
            let api::UiView::ConfigMenu(menu_id) = context.view else {
                return;
            };
            let control = ConfigControlKey {
                owner: context.owner.to_owned(),
                menu_id: menu_id.clone(),
                node_id: node.id.clone(),
            };
            let preview = context
                .config_host
                .and_then(|state| state.slider_preview.as_ref())
                .filter(|preview| preview.control == control)
                .map_or(slider.value, |preview| preview.value);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(&slider.label).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new(preview.to_string()).color(FAREVER_INK_MUTED));
                });
            });
            let mut value = preview;
            let mut widget =
                egui::Slider::new(&mut value, slider.minimum..=slider.maximum).show_value(false);
            if let Some(step) = slider.step {
                widget = widget.step_by(step);
            }
            let response = ui
                .add_enabled_ui(slider.enabled, |ui| {
                    ui.add_sized([ui.available_width(), 24.0], widget)
                })
                .inner;
            if slider.enabled {
                if let Some(rect) = clipped_interaction_rect(ui, response.rect) {
                    record_interaction(
                        context.interactions,
                        rect,
                        ClickAction::AddonSlider(SliderAction {
                            control,
                            value: preview,
                            minimum: slider.minimum,
                            maximum: slider.maximum,
                            step: slider.step,
                        }),
                    );
                }
            }
        }
        api::Widget::Progress(progress) => {
            let mut widget = egui::ProgressBar::new(progress.fraction);
            if let Some(label) = &progress.label {
                widget = widget.text(label);
            } else {
                widget = widget.show_percentage();
            }
            if let Some(color) = progress.color {
                widget = widget.fill(egui_color(color));
            }
            ui.add(widget);
        }
        api::Widget::Separator => {
            ui.separator();
        }
        api::Widget::Spacer(size) => {
            ui.add_space(*size);
        }
        api::Widget::Canvas(size) | api::Widget::PassiveCanvas(size) => {
            let rect = render_canvas(ui, *size, &context.canvas[index], context.images);
            // A canvas that draws something can be clicked: the press is routed
            // back to the add-on in canvas-local points so it can decide what,
            // if anything, was hit.
            if matches!(node.widget, api::Widget::Canvas(_)) && !context.canvas[index].is_empty() {
                if let Some(region) = clipped_interaction_rect(ui, rect) {
                    let pixels_per_point = ui.pixels_per_point();
                    context.interactions.borrow_mut().push(InteractiveRegion {
                        rect: region,
                        action: ClickAction::AddonCanvas(CanvasAction {
                            owner: context.owner.to_owned(),
                            view: context.view.clone(),
                            node_id: node.id.clone(),
                            origin: [rect.min.x, rect.min.y],
                            pixels_per_point,
                        }),
                    });
                }
            }
        }
        api::Widget::Table(table) => {
            render_table(ui, context, index, table);
        }
        api::Widget::TableRow(_) | api::Widget::TableCell(_) => {
            // Structural nodes are rendered by their owning table.
        }
    }
}

fn render_table(
    ui: &mut egui::Ui,
    context: DocumentRenderContext<'_>,
    table_index: usize,
    table: &api::TableWidget,
) {
    let cell_item_spacing_x = ui.spacing().item_spacing.x;
    ui.scope(|ui| {
        let pixels_per_point = ui.pixels_per_point();
        // Column content padding and row height are explicit in the add-on API.
        // Do not add egui's implicit gaps between cells or rows: zero padding
        // remains visually contiguous, and a row background occupies exactly
        // the same declared height as its content.
        ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
        let table_x_range = ui.available_rect_before_wrap().x_range();
        let visible_columns = visible_table_columns(table, ui.available_width());
        if visible_columns.is_empty() {
            return;
        }
        let mut builder = TableBuilder::new(ui)
            .id_salt((
                "farever-addon-table",
                context.owner,
                context.view,
                &context.nodes[table_index].id,
                visible_columns.as_slice(),
            ))
            .striped(table.striped)
            .resizable(false)
            // A table is a layout surface, not an intrinsic-size label. Keep
            // its horizontal viewport at the parent width so centered and
            // trailing-aligned cells share the same axis as sibling widgets.
            .auto_shrink([false, true])
            .vscroll(table.max_body_height.is_some());
        if let Some(max_body_height) = table.max_body_height {
            builder = builder.max_scroll_height(max_body_height);
        }
        for &column_index in &visible_columns {
            builder = builder.column(egui_table_column(
                table.columns[column_index].sizing,
                pixels_per_point,
            ));
        }

        let header = context.children[table_index]
            .iter()
            .copied()
            .find(|&row_index| {
                matches!(
                    context.nodes[row_index].widget,
                    api::Widget::TableRow(api::TableRowWidget {
                        kind: api::TableRowKind::Header,
                        ..
                    })
                )
            });
        let body_rows = context.children[table_index]
            .iter()
            .copied()
            .filter(|&row_index| {
                matches!(
                    context.nodes[row_index].widget,
                    api::Widget::TableRow(api::TableRowWidget {
                        kind: api::TableRowKind::Body,
                        ..
                    })
                )
            })
            .collect::<Vec<_>>();
        let row_context = TableRowRenderContext {
            document: context,
            table,
            visible_columns: &visible_columns,
            table_x_range,
            cell_item_spacing_x,
        };

        let render_body = |mut body: egui_extras::TableBody<'_>| {
            for &row_index in &body_rows {
                let api::Widget::TableRow(row_widget) = context.nodes[row_index].widget else {
                    unreachable!("validated table children are rows");
                };
                body.row(
                    pixel_aligned_length(row_widget.height, pixels_per_point),
                    |row| {
                        render_table_row(row, row_index, row_context);
                    },
                );
            }
        };

        if let Some(header_index) = header {
            let api::Widget::TableRow(header_widget) = context.nodes[header_index].widget else {
                unreachable!("validated table header is a row");
            };
            builder
                .header(
                    pixel_aligned_length(header_widget.height, pixels_per_point),
                    |row| {
                        render_table_row(row, header_index, row_context);
                    },
                )
                .body(render_body);
        } else {
            builder.body(render_body);
        }
    });
}

fn visible_table_columns(table: &api::TableWidget, available_width: f32) -> Vec<usize> {
    table
        .columns
        .iter()
        .enumerate()
        .filter_map(|(index, column)| {
            column
                .visible_from_width
                .is_none_or(|minimum| available_width >= minimum)
                .then_some(index)
        })
        .collect()
}

fn egui_table_column(sizing: api::TableColumnSizing, pixels_per_point: f32) -> Column {
    match sizing {
        api::TableColumnSizing::Auto => Column::auto(),
        api::TableColumnSizing::Exact(width) => {
            Column::exact(pixel_aligned_length(width, pixels_per_point))
        }
        api::TableColumnSizing::Remainder => Column::remainder(),
    }
    .clip(true)
}

#[derive(Clone, Copy)]
struct TableRowRenderContext<'a> {
    document: DocumentRenderContext<'a>,
    table: &'a api::TableWidget,
    visible_columns: &'a [usize],
    table_x_range: egui::Rangef,
    cell_item_spacing_x: f32,
}

fn render_table_row(
    mut row: TableRow<'_, '_>,
    row_index: usize,
    context: TableRowRenderContext<'_>,
) {
    let document = context.document;
    let api::Widget::TableRow(row_widget) = document.nodes[row_index].widget else {
        unreachable!("validated table rows use table-row widgets");
    };
    for (visible_position, &column_index) in context.visible_columns.iter().enumerate() {
        let cell_index = document.children[row_index]
            .iter()
            .copied()
            .find(|&cell_index| {
            matches!(
                document.nodes[cell_index].widget,
                api::Widget::TableCell(api::TableCellWidget { column }) if column == column_index
            )
        });
        row.col(|ui| {
            if visible_position == 0 {
                paint_table_row_background(ui, context.table_x_range, row_widget.background);
            }
            if let Some(progress) = row_widget.progress {
                let track_start = match progress.start_column {
                    None if visible_position == 0 => Some(context.table_x_range.min),
                    Some(start_column) if start_column == column_index => {
                        Some(ui.max_rect().left())
                    }
                    _ => None,
                };
                if let Some(track_start) = track_start {
                    paint_table_row_progress(ui, context.table_x_range, track_start, progress);
                }
            }
            if let Some(cell_index) = cell_index {
                // The table itself has contiguous columns, but widgets laid
                // out inside a cell retain the surrounding UI's normal item
                // spacing (for example, the title indicator beside its text).
                ui.spacing_mut().item_spacing.x = context.cell_item_spacing_x;
                let column = context.table.columns[column_index];
                let content_rect = table_cell_content_rect(ui.max_rect(), column.content_padding);
                ui.scope_builder(
                    egui::UiBuilder::new()
                        .max_rect(content_rect)
                        .layout(table_cell_layout(column.alignment)),
                    |ui| {
                        render_children(ui, document, cell_index);
                    },
                );
            }
        });
    }
}

fn paint_table_row_background(
    ui: &egui::Ui,
    table_x_range: egui::Rangef,
    background: Option<api::Rgba>,
) {
    let Some(background) = background else {
        return;
    };

    let row_rect = table_row_paint_rect(table_x_range, ui.max_rect(), ui.pixels_per_point());
    let row_clip_rect = egui::Rect::from_x_y_ranges(table_x_range, ui.clip_rect().y_range());
    let painter = ui
        .ctx()
        .layer_painter(ui.layer_id())
        .with_clip_rect(row_clip_rect);
    painter.rect_filled(row_rect, egui::CornerRadius::ZERO, egui_color(background));
}

fn paint_table_row_progress(
    ui: &egui::Ui,
    table_x_range: egui::Rangef,
    track_start: f32,
    progress: api::TableRowProgress,
) {
    let track_start = track_start.clamp(table_x_range.min, table_x_range.max);
    let track_x_range = egui::Rangef::new(track_start, table_x_range.max);
    let progress_right = egui::lerp(track_x_range, progress.fraction);
    if track_start >= progress_right {
        return;
    }

    let pixels_per_point = ui.pixels_per_point();
    let row_rect = table_row_paint_rect(table_x_range, ui.max_rect(), pixels_per_point);
    let progress_rect = pixel_aligned_rect(
        egui::Rect::from_min_max(
            egui::pos2(track_start, row_rect.top()),
            egui::pos2(progress_right.min(row_rect.right()), row_rect.bottom()),
        ),
        pixels_per_point,
    );
    let row_clip_rect = egui::Rect::from_x_y_ranges(table_x_range, ui.clip_rect().y_range());
    ui.ctx()
        .layer_painter(ui.layer_id())
        .with_clip_rect(row_clip_rect)
        .rect_filled(
            progress_rect,
            egui::CornerRadius::ZERO,
            egui_color(progress.color),
        );
}

fn table_row_paint_rect(
    table_x_range: egui::Rangef,
    cell_rect: egui::Rect,
    pixels_per_point: f32,
) -> egui::Rect {
    pixel_aligned_rect(
        egui::Rect::from_x_y_ranges(table_x_range, cell_rect.y_range()),
        pixels_per_point,
    )
}

fn pixel_aligned_size(size: egui::Vec2, pixels_per_point: f32) -> egui::Vec2 {
    egui::vec2(
        pixel_aligned_length(size.x, pixels_per_point),
        pixel_aligned_length(size.y, pixels_per_point),
    )
}

fn pixel_aligned_length(length: f32, pixels_per_point: f32) -> f32 {
    (length * pixels_per_point).round() / pixels_per_point
}

fn pixel_aligned_rect(rect: egui::Rect, pixels_per_point: f32) -> egui::Rect {
    let align = |point: f32| (point * pixels_per_point).round() / pixels_per_point;
    egui::Rect::from_min_max(
        egui::pos2(align(rect.min.x), align(rect.min.y)),
        egui::pos2(align(rect.max.x), align(rect.max.y)),
    )
}

fn table_cell_layout(alignment: api::HorizontalAlignment) -> egui::Layout {
    match alignment {
        api::HorizontalAlignment::Left => egui::Layout::left_to_right(egui::Align::Center),
        // In a horizontal layout `main_align` controls alignment inside each
        // widget; it does not center the widget group in the cell. A vertical
        // layout makes horizontal alignment the cross axis, which correctly
        // places each direct cell child on the shared centerline.
        api::HorizontalAlignment::Center => egui::Layout::top_down(egui::Align::Center),
        api::HorizontalAlignment::Right => egui::Layout::right_to_left(egui::Align::Center),
    }
    .with_main_wrap(false)
}

fn table_cell_content_rect(cell_rect: egui::Rect, horizontal_padding: f32) -> egui::Rect {
    let horizontal_padding = horizontal_padding.clamp(0.0, 0.5 * cell_rect.width());
    cell_rect.shrink2(egui::vec2(horizontal_padding, 0.0))
}

fn render_canvas(
    ui: &mut egui::Ui,
    size: api::Size,
    primitives: &[&api::CanvasPrimitive],
    images: &HashMap<String, egui::TextureHandle>,
) -> egui::Rect {
    let (response, painter) =
        ui.allocate_painter(egui::vec2(size.width, size.height), egui::Sense::click());
    let origin = response.rect.min;
    paint_canvas(&painter, origin, primitives, images);
    response.rect
}

fn paint_canvas(
    painter: &egui::Painter,
    origin: egui::Pos2,
    primitives: &[&api::CanvasPrimitive],
    images: &HashMap<String, egui::TextureHandle>,
) {
    for primitive in primitives {
        match primitive {
            api::CanvasPrimitive::Line { from, to, stroke } => {
                painter.line_segment(
                    [canvas_point(origin, *from), canvas_point(origin, *to)],
                    egui_stroke(*stroke),
                );
            }
            api::CanvasPrimitive::Rect {
                min,
                max,
                corner_radius,
                fill,
                stroke,
            } => {
                let rect = egui::Rect::from_two_pos(
                    canvas_point(origin, *min),
                    canvas_point(origin, *max),
                );
                let radius =
                    egui::CornerRadius::same(corner_radius.round().clamp(0.0, 255.0) as u8);
                if let Some(fill) = fill {
                    painter.rect_filled(rect, radius, egui_color(*fill));
                }
                if let Some(stroke) = stroke {
                    painter.rect_stroke(
                        rect,
                        radius,
                        egui_stroke(*stroke),
                        egui::StrokeKind::Middle,
                    );
                }
            }
            api::CanvasPrimitive::Circle {
                center,
                radius,
                fill,
                stroke,
            } => {
                let center = canvas_point(origin, *center);
                if let Some(fill) = fill {
                    painter.circle_filled(center, *radius, egui_color(*fill));
                }
                if let Some(stroke) = stroke {
                    painter.circle_stroke(center, *radius, egui_stroke(*stroke));
                }
            }
            api::CanvasPrimitive::Path {
                points,
                closed,
                fill,
                stroke,
            } => {
                let points = points
                    .iter()
                    .map(|point| canvas_point(origin, *point))
                    .collect::<Vec<_>>();
                // Egui's feathered PathShape uses sharp-corner miters. For an
                // acute or nearly edge-on triangle those miters can extend far
                // beyond the path and appear as rays across the canvas. A
                // direct mesh preserves the validated triangle exactly.
                if points.len() == 3 && stroke.is_none() {
                    if let Some(fill) = fill {
                        painter.add(egui::Shape::mesh(filled_triangle_mesh(
                            [points[0], points[1], points[2]],
                            egui_color(*fill),
                        )));
                        continue;
                    }
                }
                painter.add(egui::Shape::Path(egui::epaint::PathShape {
                    points,
                    closed: *closed || fill.is_some(),
                    fill: fill.map_or(egui::Color32::TRANSPARENT, egui_color),
                    stroke: stroke.map_or(egui::Stroke::NONE, egui_stroke).into(),
                }));
            }
            api::CanvasPrimitive::Text {
                position,
                text,
                color,
                size,
            } => {
                painter.text(
                    canvas_point(origin, *position),
                    egui::Align2::LEFT_TOP,
                    text,
                    egui::FontId::proportional(*size),
                    egui_color(*color),
                );
            }
            api::CanvasPrimitive::Image {
                source,
                destination_min,
                destination_max,
                uv_min,
                uv_max,
                rotation_radians,
                tint,
                corner_radius,
            } => {
                let Some(texture) = images.get(&source.id) else {
                    continue;
                };
                paint_canvas_image(
                    painter,
                    texture,
                    origin,
                    *destination_min,
                    *destination_max,
                    *uv_min,
                    *uv_max,
                    *rotation_radians,
                    tint.map_or(egui::Color32::WHITE, egui_color),
                    *corner_radius,
                );
            }
        }
    }
}

fn paint_canvas_image(
    painter: &egui::Painter,
    texture: &egui::TextureHandle,
    origin: egui::Pos2,
    destination_min: api::Point,
    destination_max: api::Point,
    uv_min: api::Point,
    uv_max: api::Point,
    rotation_radians: f32,
    color: egui::Color32,
    corner_radius: f32,
) {
    let rect = egui::Rect::from_two_pos(
        canvas_point(origin, destination_min),
        canvas_point(origin, destination_max),
    );
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
        return;
    }

    let radius = corner_radius
        .clamp(0.0, rect.width().min(rect.height()) * 0.5)
        .max(0.0);
    let center = rect.center();
    let uv_center = egui::pos2((uv_min.x + uv_max.x) * 0.5, (uv_min.y + uv_max.y) * 0.5);
    let mut mesh = egui::Mesh::with_texture(texture.id());
    mesh.vertices.push(egui::epaint::Vertex {
        pos: center,
        uv: uv_center,
        color,
    });

    let boundary = rounded_rect_boundary(rect, radius);
    for point in &boundary {
        let uv = egui::pos2(
            uv_min.x + (point.x - rect.min.x) / rect.width() * (uv_max.x - uv_min.x),
            uv_min.y + (point.y - rect.min.y) / rect.height() * (uv_max.y - uv_min.y),
        );
        mesh.vertices.push(egui::epaint::Vertex {
            pos: rotate_canvas_point(*point, center, rotation_radians),
            uv,
            color,
        });
    }
    for index in 0..boundary.len() {
        let next = (index + 1) % boundary.len();
        mesh.add_triangle(0, (index + 1) as u32, (next + 1) as u32);
    }
    painter.add(egui::Shape::mesh(mesh));
}

fn rounded_rect_boundary(rect: egui::Rect, radius: f32) -> Vec<egui::Pos2> {
    if radius <= f32::EPSILON {
        return vec![
            rect.left_top(),
            rect.right_top(),
            rect.right_bottom(),
            rect.left_bottom(),
        ];
    }

    const CORNER_STEPS: usize = 8;
    let corners = [
        (
            egui::pos2(rect.right() - radius, rect.top() + radius),
            -std::f32::consts::FRAC_PI_2,
        ),
        (
            egui::pos2(rect.right() - radius, rect.bottom() - radius),
            0.0,
        ),
        (
            egui::pos2(rect.left() + radius, rect.bottom() - radius),
            std::f32::consts::FRAC_PI_2,
        ),
        (
            egui::pos2(rect.left() + radius, rect.top() + radius),
            std::f32::consts::PI,
        ),
    ];
    let mut boundary = Vec::with_capacity(CORNER_STEPS * corners.len());
    for (center, start) in corners {
        for step in 0..CORNER_STEPS {
            let angle = start + std::f32::consts::FRAC_PI_2 * step as f32 / CORNER_STEPS as f32;
            let (sin, cos) = angle.sin_cos();
            boundary.push(egui::pos2(center.x + cos * radius, center.y + sin * radius));
        }
    }
    boundary
}

fn rotate_canvas_point(point: egui::Pos2, center: egui::Pos2, radians: f32) -> egui::Pos2 {
    let (sin, cos) = radians.sin_cos();
    let delta = point - center;
    center + egui::vec2(delta.x * cos - delta.y * sin, delta.x * sin + delta.y * cos)
}

fn filled_triangle_mesh(points: [egui::Pos2; 3], color: egui::Color32) -> egui::Mesh {
    let mut mesh = egui::Mesh::default();
    for point in points {
        mesh.colored_vertex(point, color);
    }
    mesh.add_triangle(0, 1, 2);
    mesh
}

fn canvas_point(origin: egui::Pos2, point: api::Point) -> egui::Pos2 {
    origin + egui::vec2(point.x, point.y)
}

fn egui_color(color: api::Rgba) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(
        (color.red * 255.0).round() as u8,
        (color.green * 255.0).round() as u8,
        (color.blue * 255.0).round() as u8,
        (color.alpha * 255.0).round() as u8,
    )
}

fn egui_stroke(stroke: api::Stroke) -> egui::Stroke {
    egui::Stroke::new(stroke.width, egui_color(stroke.color))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct GameBounds {
    pub(super) x: i32,
    pub(super) y: i32,
    pub(super) width: i32,
    pub(super) height: i32,
}

pub(super) fn game_bounds(window: HWND) -> Option<GameBounds> {
    let mut rect: RECT = unsafe { zeroed() };
    if unsafe { GetClientRect(window, &mut rect) } == 0 {
        return None;
    }
    let mut origin = POINT { x: 0, y: 0 };
    if unsafe { ClientToScreen(window, &mut origin) } == 0 {
        return None;
    }
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    (width > 0 && height > 0).then_some(GameBounds {
        x: origin.x,
        y: origin.y,
        width,
        height,
    })
}

struct WindowSearch {
    pid: u32,
    excluded: HWND,
    result: HWND,
    result_area: i64,
    pid_windows: usize,
    eligible_windows: usize,
}

#[derive(Default)]
pub(super) struct WindowSearchResult {
    pub(super) window: Option<HWND>,
    pub(super) area: i64,
    pub(super) pid_windows: usize,
    pub(super) eligible_windows: usize,
}

pub(super) fn find_main_window(pid: u32, excluded: HWND) -> WindowSearchResult {
    let mut search = WindowSearch {
        pid,
        excluded,
        result: std::ptr::null_mut(),
        result_area: 0,
        pid_windows: 0,
        eligible_windows: 0,
    };
    unsafe {
        EnumWindows(
            Some(enum_window),
            (&mut search as *mut WindowSearch) as LPARAM,
        );
    }
    WindowSearchResult {
        window: (!search.result.is_null()).then_some(search.result),
        area: search.result_area,
        pid_windows: search.pid_windows,
        eligible_windows: search.eligible_windows,
    }
}

pub(crate) fn largest_visible_client_area(pid: u32) -> i64 {
    find_main_window(pid, std::ptr::null_mut()).area
}

unsafe extern "system" fn enum_window(window: HWND, parameter: LPARAM) -> BOOL {
    let search = unsafe { &mut *(parameter as *mut WindowSearch) };
    let mut pid = 0_u32;
    unsafe {
        GetWindowThreadProcessId(window, &mut pid);
    }
    if pid != search.pid {
        return 1;
    }
    search.pid_windows += 1;
    if window == search.excluded
        || unsafe { IsWindowVisible(window) } == 0
        || !unsafe { GetWindow(window, GW_OWNER) }.is_null()
    {
        return 1;
    }
    let mut rect: RECT = unsafe { zeroed() };
    if unsafe { GetClientRect(window, &mut rect) } == 0 || rect.right <= 0 || rect.bottom <= 0 {
        return 1;
    }
    search.eligible_windows += 1;
    let area = i64::from(rect.right - rect.left) * i64::from(rect.bottom - rect.top);
    if area > search.result_area {
        search.result = window;
        search.result_area = area;
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn measured_game_menu_geometry() -> Option<GameMenuGeometry> {
        Some(GameMenuGeometry {
            x: 23.0,
            y: 40.0,
            width: GAME_MENU_PANEL_PHYSICAL_SIZE.x,
            height: GAME_MENU_PANEL_PHYSICAL_SIZE.y,
        })
    }

    #[test]
    fn ui_events_from_an_old_addon_revision_are_discarded() {
        let (sender, receiver) = mpsc::sync_channel(2);
        let event = |addon_revision| StampedUiEvent {
            addon_revision,
            routed: api::RoutedUiEvent {
                owner: "addon".to_owned(),
                event: api::UiEvent::ConfigMenuShown("settings".to_owned()),
            },
        };
        sender.send(event(7)).expect("queue stale event");
        sender.send(event(8)).expect("queue current event");

        let current = drain_ui_events(&receiver, 8);

        assert_eq!(current.len(), 1);
        assert_eq!(current[0].owner, "addon");
    }

    #[test]
    fn passive_map_canvas_paints_at_exact_position_with_clipping_and_no_hit_regions() {
        let frame = api::UiFrame {
            surfaces: vec![api::UiSurface {
                owner: "map-waypoints".into(),
                id: "map".into(),
                title: "".into(),
                anchor: api::SurfaceAnchor::TopLeft,
                margin_x: 100.0,
                margin_y: 200.0,
                width: None,
                style: None,
                nodes: vec![api::UiNode {
                    id: "canvas".into(),
                    parent: None,
                    widget: api::Widget::PassiveCanvas(api::Size {
                        width: 400.0,
                        height: 300.0,
                    }),
                }],
                canvas: vec![api::CanvasCommand {
                    canvas: 0,
                    primitive: api::CanvasPrimitive::Rect {
                        min: api::Point { x: -10.0, y: 20.0 },
                        max: api::Point { x: 22.0, y: 52.0 },
                        corner_radius: 0.0,
                        fill: Some(api::Rgba {
                            red: 1.0,
                            green: 0.0,
                            blue: 0.0,
                            alpha: 1.0,
                        }),
                        stroke: None,
                    },
                }],
            }],
            config_menus: vec![],
        };
        let context = egui::Context::default();
        let mut interactions = Vec::new();
        let output = context.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                ..Default::default()
            },
            |context| {
                interactions = render_interactive_frame(
                    context,
                    &frame,
                    &HashMap::new(),
                    false,
                    None,
                    true,
                    &ConfigHostState::default(),
                );
            },
        );
        assert!(
            interactions.is_empty(),
            "native map must receive mouse input"
        );
        let shape = output
            .shapes
            .iter()
            .find(|shape| matches!(shape.shape, egui::epaint::Shape::Rect(_)))
            .unwrap();
        assert_eq!(
            shape.clip_rect,
            egui::Rect::from_min_size(egui::pos2(100.0, 200.0), egui::vec2(400.0, 300.0))
        );
        let egui::epaint::Shape::Rect(rect) = &shape.shape else {
            unreachable!()
        };
        assert_eq!(
            rect.rect.min,
            egui::pos2(90.0, 220.0),
            "no window gutter or automatic repositioning"
        );
    }

    #[test]
    fn host_theme_uses_farevers_warm_light_palette() {
        let visuals = farever_visuals();

        assert!(!visuals.dark_mode);
        assert_eq!(visuals.panel_fill, egui::Color32::TRANSPARENT);
        assert_eq!(visuals.window_fill, FAREVER_PARCHMENT);
        assert_eq!(visuals.window_stroke.color, FAREVER_BORDER);
        assert_eq!(visuals.widgets.noninteractive.fg_stroke.color, FAREVER_INK);
        assert_eq!(visuals.widgets.inactive.weak_bg_fill, FAREVER_CONTROL);
        assert_eq!(visuals.selection.bg_fill, FAREVER_CONTROL_ACTIVE);
        assert_eq!(visuals.selection.stroke.color, FAREVER_CREAM);
    }

    #[test]
    fn overlay_defaults_slash_commands_on() {
        assert!(OverlayPacket::default().slash_commands_enabled);
        assert!(OverlayHostState::default().slash_commands_enabled);
    }

    #[test]
    fn responsive_columns_use_inclusive_logical_width_breakpoints() {
        let table = api::TableWidget {
            columns: vec![
                api::TableColumn {
                    sizing: api::TableColumnSizing::Remainder,
                    alignment: api::HorizontalAlignment::Left,
                    visible_from_width: None,
                    content_padding: 0.0,
                },
                api::TableColumn {
                    sizing: api::TableColumnSizing::Auto,
                    alignment: api::HorizontalAlignment::Right,
                    visible_from_width: Some(320.0),
                    content_padding: 0.0,
                },
                api::TableColumn {
                    sizing: api::TableColumnSizing::Exact(64.0),
                    alignment: api::HorizontalAlignment::Center,
                    visible_from_width: Some(520.0),
                    content_padding: 0.0,
                },
            ],
            striped: true,
            max_body_height: Some(300.0),
        };

        assert_eq!(visible_table_columns(&table, 319.9), vec![0]);
        assert_eq!(visible_table_columns(&table, 320.0), vec![0, 1]);
        assert_eq!(visible_table_columns(&table, 520.0), vec![0, 1, 2]);
    }

    #[test]
    fn right_aligned_table_cells_place_content_from_the_trailing_edge() {
        assert!(table_cell_layout(api::HorizontalAlignment::Right).prefer_right_to_left());
        assert!(!table_cell_layout(api::HorizontalAlignment::Left).prefer_right_to_left());
    }

    #[test]
    fn table_cell_content_padding_insets_both_horizontal_edges() {
        let rect = table_cell_content_rect(
            egui::Rect::from_min_max(egui::pos2(10.0, 20.0), egui::pos2(110.0, 40.0)),
            6.0,
        );

        assert_eq!(rect.left(), 16.0);
        assert_eq!(rect.right(), 104.0);
        assert_eq!(rect.top(), 20.0);
        assert_eq!(rect.bottom(), 40.0);
    }

    #[test]
    fn filled_triangle_mesh_has_no_feathering_geometry() {
        let points = [
            egui::pos2(1.0, 2.0),
            egui::pos2(30.0, 4.0),
            egui::pos2(8.0, 40.0),
        ];
        let color = egui::Color32::from_rgb(255, 200, 32);

        let mesh = filled_triangle_mesh(points, color);

        assert_eq!(mesh.indices, [0, 1, 2]);
        assert_eq!(mesh.vertices.len(), 3);
        for (vertex, point) in mesh.vertices.iter().zip(points) {
            assert_eq!(vertex.pos, point);
            assert_eq!(vertex.color, color);
        }
    }

    #[test]
    fn preferred_surface_width_is_clamped_to_the_viewport() {
        assert_eq!(responsive_surface_width(680.0, 1920.0, 20.0), 680.0);
        assert_eq!(responsive_surface_width(680.0, 640.0, 20.0), 584.0);
        assert_eq!(responsive_surface_width(680.0, 100.0, 20.0), 120.0);
    }

    #[test]
    fn top_center_surface_uses_viewport_top_center() {
        let surface = api::UiSurface {
            owner: "test-addon".to_owned(),
            id: "wayfinder".to_owned(),
            title: "Wayfinder".to_owned(),
            anchor: api::SurfaceAnchor::TopCenter,
            margin_x: -12.0,
            margin_y: 52.0,
            width: Some(176.0),
            style: None,
            nodes: Vec::new(),
            canvas: Vec::new(),
        };

        let (anchor, offset) = surface_placement(&surface);
        assert_eq!(anchor, egui::Align2::CENTER_TOP);
        assert_eq!(offset, egui::vec2(-12.0, 52.0));
    }

    #[test]
    fn table_row_background_spans_the_table_instead_of_one_cell() {
        let rect = table_row_paint_rect(
            egui::Rangef::new(20.0, 420.0),
            egui::Rect::from_min_max(egui::pos2(20.0, 10.0), egui::pos2(100.0, 30.0)),
            1.0,
        );

        assert_eq!(rect.left(), 20.0);
        assert_eq!(rect.right(), 420.0);
        assert_eq!(rect.top(), 10.0);
        assert_eq!(rect.bottom(), 30.0);
    }

    #[test]
    fn repeated_row_geometry_uses_one_integer_physical_pixel_unit() {
        let pixels_per_point = 1376.0 / 1080.0;
        let row_height = pixel_aligned_length(21.0, pixels_per_point);
        let icon_size = pixel_aligned_size(egui::Vec2::splat(21.0), pixels_per_point);

        assert_eq!((row_height * pixels_per_point).round() as u32, 27);
        assert_eq!((icon_size.x * pixels_per_point).round() as u32, 27);
        assert_eq!((icon_size.y * pixels_per_point).round() as u32, 27);

        let first_top = 73.4 / pixels_per_point;
        let rows = (0..3)
            .map(|index| {
                pixel_aligned_rect(
                    egui::Rect::from_min_size(
                        egui::pos2(0.0, first_top + index as f32 * row_height),
                        egui::vec2(21.0, row_height),
                    ),
                    pixels_per_point,
                )
            })
            .collect::<Vec<_>>();

        for row in &rows {
            assert_eq!((row.height() * pixels_per_point).round() as u32, 27);
        }
        assert_eq!(
            (rows[0].bottom() * pixels_per_point).round() as u32,
            (rows[1].top() * pixels_per_point).round() as u32
        );
        assert_eq!(
            (rows[1].bottom() * pixels_per_point).round() as u32,
            (rows[2].top() * pixels_per_point).round() as u32
        );
    }

    #[test]
    fn renderer_accepts_a_validated_table_tree() {
        let frame = api::UiFrame {
            surfaces: vec![api::UiSurface {
                owner: "test-addon".to_owned(),
                id: "main".to_owned(),
                title: "Damage".to_owned(),
                anchor: api::SurfaceAnchor::TopLeft,
                margin_x: 0.0,
                margin_y: 0.0,
                width: Some(360.0),
                style: Some(api::SurfaceStyle {
                    title_bar: false,
                    fill: api::Rgba {
                        red: 0.02,
                        green: 0.02,
                        blue: 0.02,
                        alpha: 0.9,
                    },
                    stroke: None,
                    corner_radius: 2.0,
                    padding: 2.0,
                }),
                nodes: vec![
                    api::UiNode {
                        id: "skills".to_owned(),
                        parent: None,
                        widget: api::Widget::Table(api::TableWidget {
                            columns: vec![api::TableColumn {
                                sizing: api::TableColumnSizing::Remainder,
                                alignment: api::HorizontalAlignment::Left,
                                visible_from_width: None,
                                content_padding: 0.0,
                            }],
                            striped: true,
                            max_body_height: Some(120.0),
                        }),
                    },
                    api::UiNode {
                        id: "header".to_owned(),
                        parent: Some(0),
                        widget: api::Widget::TableRow(api::TableRowWidget {
                            kind: api::TableRowKind::Header,
                            height: 20.0,
                            background: None,
                            progress: None,
                        }),
                    },
                    api::UiNode {
                        id: "header-skill".to_owned(),
                        parent: Some(1),
                        widget: api::Widget::TableCell(api::TableCellWidget { column: 0 }),
                    },
                    api::UiNode {
                        id: "header-label".to_owned(),
                        parent: Some(2),
                        widget: api::Widget::Text(api::TextWidget {
                            text: "Skill".to_owned(),
                            style: api::TextStyle::Strong,
                            font_family: None,
                            color: None,
                            outline: None,
                            wrap: false,
                        }),
                    },
                    api::UiNode {
                        id: "body".to_owned(),
                        parent: Some(0),
                        widget: api::Widget::TableRow(api::TableRowWidget {
                            kind: api::TableRowKind::Body,
                            height: 22.0,
                            background: Some(api::Rgba {
                                red: 0.05,
                                green: 0.05,
                                blue: 0.05,
                                alpha: 0.8,
                            }),
                            progress: Some(api::TableRowProgress {
                                fraction: 0.5,
                                color: api::Rgba {
                                    red: 0.2,
                                    green: 0.5,
                                    blue: 0.8,
                                    alpha: 0.8,
                                },
                                start_column: None,
                            }),
                        }),
                    },
                    api::UiNode {
                        id: "body-skill".to_owned(),
                        parent: Some(4),
                        widget: api::Widget::TableCell(api::TableCellWidget { column: 0 }),
                    },
                    api::UiNode {
                        id: "body-label".to_owned(),
                        parent: Some(5),
                        widget: api::Widget::Text(api::TextWidget {
                            text: "Base Attack".to_owned(),
                            style: api::TextStyle::Body,
                            font_family: None,
                            color: None,
                            outline: None,
                            wrap: false,
                        }),
                    },
                ],
                canvas: Vec::new(),
            }],
            config_menus: Vec::new(),
        };
        let context = egui::Context::default();
        let output = context.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                ..Default::default()
            },
            |context| render_frame(context, &frame, &HashMap::new()),
        );

        assert!(!output.shapes.is_empty());
    }

    #[test]
    fn centered_table_text_shares_a_canvas_centerline() {
        let marker_color = api::Rgba {
            red: 1.0,
            green: 0.0,
            blue: 1.0,
            alpha: 1.0,
        };
        let frame = api::UiFrame {
            surfaces: vec![api::UiSurface {
                owner: "test-addon".to_owned(),
                id: "wayfinder".to_owned(),
                title: "Wayfinder".to_owned(),
                anchor: api::SurfaceAnchor::TopLeft,
                margin_x: 0.0,
                margin_y: 0.0,
                width: Some(176.0),
                style: Some(api::SurfaceStyle {
                    title_bar: false,
                    fill: api::Rgba {
                        red: 0.0,
                        green: 0.0,
                        blue: 0.0,
                        alpha: 0.0,
                    },
                    stroke: None,
                    corner_radius: 0.0,
                    padding: 0.0,
                }),
                nodes: vec![
                    api::UiNode {
                        id: "arrow".to_owned(),
                        parent: None,
                        widget: api::Widget::Canvas(api::Size {
                            width: 176.0,
                            height: 4.0,
                        }),
                    },
                    api::UiNode {
                        id: "label-table".to_owned(),
                        parent: None,
                        widget: api::Widget::Table(api::TableWidget {
                            columns: vec![api::TableColumn {
                                sizing: api::TableColumnSizing::Remainder,
                                alignment: api::HorizontalAlignment::Center,
                                visible_from_width: None,
                                content_padding: 0.0,
                            }],
                            striped: false,
                            max_body_height: None,
                        }),
                    },
                    api::UiNode {
                        id: "label-row".to_owned(),
                        parent: Some(1),
                        widget: api::Widget::TableRow(api::TableRowWidget {
                            kind: api::TableRowKind::Body,
                            height: 20.0,
                            background: None,
                            progress: None,
                        }),
                    },
                    api::UiNode {
                        id: "label-cell".to_owned(),
                        parent: Some(2),
                        widget: api::Widget::TableCell(api::TableCellWidget { column: 0 }),
                    },
                    api::UiNode {
                        id: "label".to_owned(),
                        parent: Some(3),
                        widget: api::Widget::Text(api::TextWidget {
                            text: "Waypoint".to_owned(),
                            style: api::TextStyle::Strong,
                            font_family: None,
                            color: None,
                            outline: None,
                            wrap: false,
                        }),
                    },
                ],
                canvas: vec![api::CanvasCommand {
                    canvas: 0,
                    primitive: api::CanvasPrimitive::Circle {
                        center: api::Point { x: 88.0, y: 2.0 },
                        radius: 1.0,
                        fill: Some(marker_color),
                        stroke: None,
                    },
                }],
            }],
            config_menus: Vec::new(),
        };
        let context = egui::Context::default();
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400.0, 300.0),
            )),
            ..Default::default()
        };
        let _ = context.run(input(), |context| {
            render_frame(context, &frame, &HashMap::new());
        });
        let output = context.run(input(), |context| {
            render_frame(context, &frame, &HashMap::new());
        });

        fn find_centers(
            shape: &egui::Shape,
            marker_center_x: &mut Option<f32>,
            label_center_x: &mut Option<f32>,
        ) {
            match shape {
                egui::Shape::Circle(circle) => {
                    *marker_center_x = Some(circle.center.x);
                }
                egui::Shape::Text(text) if text.galley.text() == "Waypoint" => {
                    *label_center_x = Some(text.visual_bounding_rect().center().x);
                }
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        find_centers(shape, marker_center_x, label_center_x);
                    }
                }
                _ => {}
            }
        }

        let mut marker_center_x = None;
        let mut label_center_x = None;
        for clipped in &output.shapes {
            find_centers(&clipped.shape, &mut marker_center_x, &mut label_center_x);
        }

        let marker_center_x = marker_center_x.expect("canvas center marker");
        let label_center_x = label_center_x.expect("centered label");
        assert!(
            (marker_center_x - label_center_x).abs() < 0.1,
            "canvas center {marker_center_x}, label center {label_center_x}"
        );
    }

    #[test]
    fn section_nodes_render_their_children_as_controls() {
        // A section is host-styled chrome around ordinary children, so the
        // children must keep their interactions instead of the header
        // swallowing the layout.
        let frame = api::UiFrame {
            surfaces: Vec::new(),
            config_menus: vec![api::ConfigMenu {
                owner: "test-addon".to_owned(),
                id: "settings".to_owned(),
                title: "Test Add-on".to_owned(),
                nodes: vec![
                    api::UiNode {
                        id: "poi".to_owned(),
                        parent: None,
                        widget: api::Widget::Section(api::SectionWidget {
                            title: "Points of interest".to_owned(),
                            description: Some("Cached from the POI database".to_owned()),
                        }),
                    },
                    api::UiNode {
                        id: "markers".to_owned(),
                        parent: Some(0),
                        widget: api::Widget::Checkbox(api::CheckboxWidget {
                            label: "Show on map".to_owned(),
                            checked: true,
                            enabled: true,
                        }),
                    },
                ],
                canvas: Vec::new(),
            }],
        };
        let context = egui::Context::default();
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 800.0),
            )),
            ..Default::default()
        };
        let config_host = ConfigHostState {
            open: true,
            active_menu: Some(ConfigMenuKey {
                owner: "test-addon".to_owned(),
                id: "settings".to_owned(),
            }),
            ..Default::default()
        };
        let mut regions = Vec::new();
        let _ = context.run(input(), |context| {
            regions = render_interactive_frame(
                context,
                &frame,
                &HashMap::new(),
                true,
                None,
                false,
                &config_host,
            );
        });
        assert!(
            regions.iter().any(|region| matches!(
                &region.action,
                ClickAction::AddonCheckbox(api::RoutedUiEvent {
                    owner,
                    event: api::UiEvent::CheckboxChanged { node_id, checked },
                }) if owner == "test-addon" && node_id == "markers" && !checked
            )),
            "the checkbox under a section must stay interactive"
        );
    }

    #[test]
    fn config_shell_exposes_only_semantic_interactions() {
        let frame = api::UiFrame {
            surfaces: Vec::new(),
            config_menus: vec![api::ConfigMenu {
                owner: "test-addon".to_owned(),
                id: "settings".to_owned(),
                title: "Test Add-on".to_owned(),
                nodes: vec![
                    api::UiNode {
                        id: "enabled".to_owned(),
                        parent: None,
                        widget: api::Widget::Checkbox(api::CheckboxWidget {
                            label: "Enabled".to_owned(),
                            checked: true,
                            enabled: true,
                        }),
                    },
                    api::UiNode {
                        id: "do-something".to_owned(),
                        parent: None,
                        widget: api::Widget::Button(api::ButtonWidget {
                            label: "Do something".to_owned(),
                            enabled: true,
                        }),
                    },
                    api::UiNode {
                        id: "theme".to_owned(),
                        parent: None,
                        widget: api::Widget::Dropdown(api::DropdownWidget {
                            label: "Theme".to_owned(),
                            selected_id: "warm".to_owned(),
                            options: vec![
                                api::DropdownOption {
                                    id: "warm".to_owned(),
                                    label: "Warm".to_owned(),
                                },
                                api::DropdownOption {
                                    id: "cool".to_owned(),
                                    label: "Cool".to_owned(),
                                },
                            ],
                            enabled: true,
                        }),
                    },
                    api::UiNode {
                        id: "scale".to_owned(),
                        parent: None,
                        widget: api::Widget::Slider(api::SliderWidget {
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
            }],
        };
        let context = egui::Context::default();
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 800.0),
            )),
            ..Default::default()
        };

        let mut closed = Vec::new();
        let _ = context.run(input(), |context| {
            closed = render_interactive_frame(
                context,
                &frame,
                &HashMap::new(),
                false,
                None,
                false,
                &ConfigHostState::default(),
            );
        });
        assert!(closed.is_empty(), "the cog is absent outside the Game Menu");

        let mut entry = Vec::new();
        let _ = context.run(input(), |context| {
            entry = render_interactive_frame(
                context,
                &frame,
                &HashMap::new(),
                true,
                None,
                false,
                &ConfigHostState::default(),
            );
        });
        assert!(
            entry.is_empty(),
            "the cog waits for measured geometry instead of flashing at the fallback"
        );
        let _ = context.run(input(), |context| {
            entry = render_interactive_frame(
                context,
                &frame,
                &HashMap::new(),
                true,
                measured_game_menu_geometry(),
                false,
                &ConfigHostState::default(),
            );
        });
        assert!(entry
            .iter()
            .any(|region| region.action == ClickAction::OpenConfigMenu));

        let key = ConfigMenuKey {
            owner: "test-addon".to_owned(),
            id: "settings".to_owned(),
        };
        let config_host = ConfigHostState {
            open: true,
            active_menu: Some(key.clone()),
            ..Default::default()
        };
        let mut open = Vec::new();
        let _ = context.run(input(), |context| {
            open = render_interactive_frame(
                context,
                &frame,
                &HashMap::new(),
                true,
                None,
                false,
                &config_host,
            );
        });
        assert!(open
            .iter()
            .any(|region| region.action == ClickAction::CloseConfigMenu));
        assert!(open.iter().any(|region| {
            matches!(
                &region.action,
                ClickAction::SelectConfigMenu(selected) if selected == &key
            )
        }));
        assert!(open.iter().any(|region| {
            matches!(
                &region.action,
                ClickAction::AddonButton(api::RoutedUiEvent {
                    owner,
                    event: api::UiEvent::ButtonPressed {
                        view: api::UiView::ConfigMenu(menu_id),
                        node_id,
                    },
                }) if owner == "test-addon" && menu_id == "settings" && node_id == "do-something"
            )
        }));
        let addon_button = open
            .iter()
            .find(|region| {
                matches!(
                    &region.action,
                    ClickAction::AddonButton(api::RoutedUiEvent {
                        event: api::UiEvent::ButtonPressed { node_id, .. },
                        ..
                    }) if node_id == "do-something"
                )
            })
            .expect("themed add-on button interaction");
        assert_eq!(addon_button.rect.height(), 50.0);
        assert!(addon_button.rect.width() > 400.0);
        assert!(open.iter().any(|region| {
            matches!(
                &region.action,
                ClickAction::AddonCheckbox(api::RoutedUiEvent {
                    owner,
                    event: api::UiEvent::CheckboxChanged { node_id, checked },
                }) if owner == "test-addon" && node_id == "enabled" && !checked
            )
        }));
        assert!(open.iter().any(|region| {
            matches!(
                &region.action,
                ClickAction::ToggleAddonDropdown(control)
                    if control.owner == "test-addon"
                        && control.menu_id == "settings"
                        && control.node_id == "theme"
            )
        }));
        assert!(open.iter().any(|region| {
            matches!(
                &region.action,
                ClickAction::AddonSlider(slider)
                    if slider.control.owner == "test-addon"
                        && slider.control.menu_id == "settings"
                        && slider.control.node_id == "scale"
                        && slider.minimum == 0.5
                        && slider.maximum == 2.0
                        && slider.step == Some(0.25)
            )
        }));

        let mut dropdown_open = config_host.clone();
        dropdown_open.open_dropdown = Some(ConfigControlKey {
            owner: "test-addon".to_owned(),
            menu_id: "settings".to_owned(),
            node_id: "theme".to_owned(),
        });
        let mut dropdown_options = Vec::new();
        let _ = context.run(input(), |context| {
            dropdown_options = render_interactive_frame(
                context,
                &frame,
                &HashMap::new(),
                true,
                None,
                false,
                &dropdown_open,
            );
        });
        assert!(dropdown_options.iter().any(|region| {
            matches!(
                &region.action,
                ClickAction::AddonDropdown(api::RoutedUiEvent {
                    owner,
                    event: api::UiEvent::DropdownChanged {
                        node_id,
                        selected_id,
                    },
                }) if owner == "test-addon" && node_id == "theme" && selected_id == "cool"
            )
        }));
    }

    #[test]
    fn config_entry_point_and_host_settings_do_not_require_addon_pages() {
        let frame = api::UiFrame::default();
        let context = egui::Context::default();
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 800.0),
            )),
            ..Default::default()
        };

        let mut entry = Vec::new();
        let _ = context.run(input(), |context| {
            entry = render_interactive_frame(
                context,
                &frame,
                &HashMap::new(),
                true,
                measured_game_menu_geometry(),
                false,
                &ConfigHostState::default(),
            );
        });
        let cog = entry
            .iter()
            .find(|region| region.action == ClickAction::OpenConfigMenu)
            .expect("config cog interaction");
        assert_eq!(cog.rect.min, egui::pos2(362.0, 638.0));

        let mut host_shell = Vec::new();
        let _ = context.run(input(), |context| {
            host_shell = render_interactive_frame(
                context,
                &frame,
                &HashMap::new(),
                true,
                None,
                true,
                &ConfigHostState {
                    open: true,
                    active_menu: Some(host_config_menu_key()),
                    ..Default::default()
                },
            );
        });
        assert_eq!(
            host_shell
                .iter()
                .filter(|region| region.action == ClickAction::CloseConfigMenu)
                .count(),
            1,
            "the title-bar X is the only close control"
        );
        assert!(host_shell
            .iter()
            .any(|region| region.action == ClickAction::SetSlashCommandsEnabled(false)));
        assert!(host_shell.iter().any(|region| {
            region.action == ClickAction::SelectConfigMenu(host_config_menu_key())
        }));
    }

    #[test]
    fn config_entry_point_tracks_measured_game_menu_position_and_scale() {
        let ui_scale = 1376.0 / 1080.0;
        let scaled = egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(2507.0 / ui_scale, 1376.0 / ui_scale),
        );
        let measured_gear = game_menu_gear_rect(
            scaled,
            ui_scale,
            GameMenuGeometry {
                x: 187.0,
                y: 275.0,
                width: 493.0,
                height: 714.0,
            },
        );
        assert!((measured_gear.min.x * ui_scale - 526.0).abs() < 0.01);
        assert!((measured_gear.min.y * ui_scale - 873.0).abs() < 0.01);

        let half_scale_gear = game_menu_gear_rect(
            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1430.0, 900.0)),
            1.0,
            GameMenuGeometry {
                x: 100.0,
                y: 200.0,
                width: GAME_MENU_PANEL_PHYSICAL_SIZE.x * 0.5,
                height: GAME_MENU_PANEL_PHYSICAL_SIZE.y * 0.5,
            },
        );
        assert_eq!(half_scale_gear.min, egui::pos2(269.5, 499.0));
        assert_eq!(half_scale_gear.size(), egui::vec2(36.0, 36.0));
    }

    #[test]
    fn config_navigation_is_alphabetical_and_clips_offscreen_pages() {
        let frame = api::UiFrame {
            surfaces: Vec::new(),
            config_menus: (0..20)
                .map(|index| api::ConfigMenu {
                    owner: format!("test-addon-{index:02}"),
                    id: format!("page-{index:02}"),
                    title: format!("Add-on {:02}", 19 - index),
                    nodes: Vec::new(),
                    canvas: Vec::new(),
                })
                .collect(),
        };
        let context = egui::Context::default();
        let mut interactions = Vec::new();
        let _ = context.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1280.0, 800.0),
                )),
                ..Default::default()
            },
            |context| {
                interactions = render_interactive_frame(
                    context,
                    &frame,
                    &HashMap::new(),
                    true,
                    None,
                    true,
                    &ConfigHostState {
                        open: true,
                        active_menu: Some(host_config_menu_key()),
                        ..Default::default()
                    },
                );
            },
        );

        let menu_keys = interactions
            .iter()
            .filter_map(|region| match &region.action {
                ClickAction::SelectConfigMenu(key) if !is_host_config_menu(key) => Some(key),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            menu_keys.first().map(|key| key.id.as_str()),
            Some("page-19")
        );
        assert!(!menu_keys.is_empty());
        assert!(
            menu_keys.len() < frame.config_menus.len(),
            "offscreen navigation entries must not create native hit regions"
        );
    }

    #[test]
    fn config_navigation_initials_are_bounded_and_have_a_fallback() {
        assert_eq!(config_menu_initials("Damage Meter"), "DM");
        assert_eq!(config_menu_initials("Minimap"), "M");
        assert_eq!(config_menu_initials("  "), "A");
        assert_eq!(config_owner_label("org.dyno-addon"), "Dyno");
    }

    #[test]
    fn config_navigation_groups_and_sorts_pages_from_the_same_addon() {
        let menus = vec![
            api::ConfigMenu {
                owner: "dyno".to_owned(),
                id: "general".to_owned(),
                title: "General".to_owned(),
                nodes: Vec::new(),
                canvas: Vec::new(),
            },
            api::ConfigMenu {
                owner: "dyno".to_owned(),
                id: "advanced".to_owned(),
                title: "Advanced".to_owned(),
                nodes: Vec::new(),
                canvas: Vec::new(),
            },
        ];

        let groups = config_menu_groups(&menus);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].label, "Dyno");
        assert_eq!(groups[0].menus[0].id, "advanced");
        assert_eq!(groups[0].menus[1].id, "general");
    }

    #[test]
    fn button_primitive_routes_its_surface_and_node_identity() {
        let frame = api::UiFrame {
            surfaces: vec![api::UiSurface {
                owner: "test-addon".to_owned(),
                id: "actions".to_owned(),
                title: "Actions".to_owned(),
                anchor: api::SurfaceAnchor::TopLeft,
                margin_x: 8.0,
                margin_y: 8.0,
                width: Some(180.0),
                style: None,
                nodes: vec![api::UiNode {
                    id: "run".to_owned(),
                    parent: None,
                    widget: api::Widget::Button(api::ButtonWidget {
                        label: "Run".to_owned(),
                        enabled: true,
                    }),
                }],
                canvas: Vec::new(),
            }],
            config_menus: Vec::new(),
        };
        let context = egui::Context::default();
        let mut interactions = Vec::new();
        let _ = context.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                ..Default::default()
            },
            |context| {
                interactions = render_interactive_frame(
                    context,
                    &frame,
                    &HashMap::new(),
                    false,
                    None,
                    false,
                    &ConfigHostState::default(),
                );
            },
        );

        assert!(interactions.iter().any(|region| {
            matches!(
                &region.action,
                ClickAction::AddonButton(api::RoutedUiEvent {
                    owner,
                    event: api::UiEvent::ButtonPressed {
                        view: api::UiView::Surface(surface_id),
                        node_id,
                    },
                }) if owner == "test-addon" && surface_id == "actions" && node_id == "run"
            )
        }));
    }

    #[test]
    fn renderer_draws_a_host_image_reference_at_the_requested_size() {
        let context = egui::Context::default();
        let texture = context.load_texture(
            "game/skill-icon/fixture",
            egui::ColorImage::from_rgba_unmultiplied([1, 1], &[255, 0, 0, 255]),
            egui::TextureOptions::LINEAR,
        );
        let texture_id = texture.id();
        let images = HashMap::from([("game/skill-icon/fixture".to_owned(), texture)]);
        let frame = api::UiFrame {
            surfaces: vec![api::UiSurface {
                owner: "test-addon".to_owned(),
                id: "main".to_owned(),
                title: "Image".to_owned(),
                anchor: api::SurfaceAnchor::TopLeft,
                margin_x: 0.0,
                margin_y: 0.0,
                width: Some(100.0),
                style: None,
                nodes: vec![api::UiNode {
                    id: "icon".to_owned(),
                    parent: None,
                    widget: api::Widget::Image(api::ImageWidget {
                        source: api::ImageRef {
                            id: "game/skill-icon/fixture".to_owned(),
                        },
                        size: api::Size {
                            width: 21.0,
                            height: 21.0,
                        },
                        tint: None,
                    }),
                }],
                canvas: Vec::new(),
            }],
            config_menus: Vec::new(),
        };
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };
        let _ = context.run(input.clone(), |context| {
            render_frame(context, &frame, &images)
        });
        let output = context.run(input, |context| render_frame(context, &frame, &images));

        assert!(
            output
                .shapes
                .iter()
                .any(|shape| shape.shape.texture_id() == texture_id),
            "shapes={:?}",
            output.shapes
        );
    }
}

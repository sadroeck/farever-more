use crate::__wit::exports::farever::addon::plugin as raw_plugin;
use crate::__wit::farever::addon::overlay as raw_overlay;
use crate::assets::Assets;
use crate::bus::{Bus, Messages, Subscriptions};
use crate::chat::Chat;
use crate::config::{ActivationConfig, Config};
use crate::dependencies::Dependencies;
use crate::events::{
    Combat, Damage, EventLoss, InstanceChanged, PartyChanged, WindowChanged, ZoneChanged,
};
use crate::game::Game;
use crate::runtime::{Logger, ShutdownReason, Tick, Timer};
use crate::ui::{Frame, UiEvent};

pub type SdkResult<T> = Result<T, String>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AddonInfo {
    pub id: String,
    pub version: Option<String>,
    pub instance_id: u64,
    pub activated_at_ms: u64,
}

impl From<raw_plugin::ActivationContext> for AddonInfo {
    fn from(value: raw_plugin::ActivationContext) -> Self {
        Self {
            id: value.addon_id,
            version: value.addon_version,
            instance_id: value.instance_id,
            activated_at_ms: value.monotonic_ms,
        }
    }
}

#[derive(Default)]
enum UiIntent {
    #[default]
    Unchanged,
    Render,
    Replace(Frame),
    Clear,
}

pub struct ActivateContext {
    info: AddonInfo,
    ui: UiIntent,
}

impl ActivateContext {
    #[doc(hidden)]
    pub fn new(activation: raw_plugin::ActivationContext) -> Self {
        Self {
            info: activation.into(),
            ui: UiIntent::Unchanged,
        }
    }

    #[must_use]
    pub fn info(&self) -> &AddonInfo {
        &self.info
    }

    pub fn render(&mut self) {
        self.ui = UiIntent::Render;
    }

    pub fn replace_ui(&mut self, frame: Frame) {
        self.ui = UiIntent::Replace(frame);
    }

    pub fn clear_ui(&mut self) {
        self.ui = UiIntent::Clear;
    }

    #[must_use]
    pub fn config(&self) -> ActivationConfig {
        ActivationConfig::new()
    }

    #[must_use]
    pub fn assets(&self) -> Assets {
        Assets::new()
    }

    #[must_use]
    pub fn bus(&self) -> Subscriptions {
        Subscriptions::new()
    }

    #[must_use]
    pub fn dependencies(&self) -> Dependencies {
        Dependencies::new()
    }

    #[must_use]
    pub fn game(&self) -> Game {
        Game::new()
    }

    #[must_use]
    pub fn log(&self) -> Logger {
        Logger::new()
    }

    #[must_use]
    pub fn timer(&self) -> Timer {
        Timer::new()
    }

    #[doc(hidden)]
    pub fn finish(self, addon: &mut impl Addon) -> SdkResult<raw_overlay::UiUpdate> {
        finish_ui(self.ui, addon)
    }
}

#[derive(Default)]
pub struct Context {
    ui: UiIntent,
}

impl Context {
    #[doc(hidden)]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn render(&mut self) {
        self.ui = UiIntent::Render;
    }

    pub fn replace_ui(&mut self, frame: Frame) {
        self.ui = UiIntent::Replace(frame);
    }

    pub fn clear_ui(&mut self) {
        self.ui = UiIntent::Clear;
    }

    #[must_use]
    pub fn config(&self) -> Config {
        Config::new()
    }

    #[must_use]
    pub fn bus(&self) -> Bus {
        Bus::new()
    }

    #[must_use]
    pub fn dependencies(&self) -> Dependencies {
        Dependencies::new()
    }

    #[must_use]
    pub fn chat(&self) -> Chat {
        Chat::new()
    }

    #[must_use]
    pub fn game(&self) -> Game {
        Game::new()
    }

    #[must_use]
    pub fn log(&self) -> Logger {
        Logger::new()
    }

    #[must_use]
    pub fn timer(&self) -> Timer {
        Timer::new()
    }

    #[doc(hidden)]
    #[must_use]
    pub fn render_requested(&self) -> bool {
        matches!(self.ui, UiIntent::Render)
    }

    #[doc(hidden)]
    pub fn finish(self, addon: &mut impl Addon) -> SdkResult<raw_overlay::UiUpdate> {
        finish_ui(self.ui, addon)
    }
}

pub struct ViewContext {
    _private: (),
}

impl ViewContext {
    fn new() -> Self {
        Self { _private: () }
    }

    #[must_use]
    pub fn game(&self) -> Game {
        Game::new()
    }

    #[must_use]
    pub fn log(&self) -> Logger {
        Logger::new()
    }
}

fn finish_ui(intent: UiIntent, addon: &mut impl Addon) -> SdkResult<raw_overlay::UiUpdate> {
    Ok(match intent {
        UiIntent::Unchanged => raw_overlay::UiUpdate::Unchanged,
        UiIntent::Render => {
            raw_overlay::UiUpdate::Replace(addon.view(&ViewContext::new())?.into_raw())
        }
        UiIntent::Replace(frame) => raw_overlay::UiUpdate::Replace(frame.into_raw()),
        UiIntent::Clear => raw_overlay::UiUpdate::Clear,
    })
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TickControl {
    Continue,
    #[default]
    Stop,
}

/// Stateful, use-case-level façade over the WIT component callbacks.
pub trait Addon: Sized + 'static {
    fn activate(context: &mut ActivateContext) -> SdkResult<Self>;

    fn call_service(
        &mut self,
        _service: &str,
        _operation: u32,
        _request: &[u8],
    ) -> SdkResult<Vec<u8>> {
        Err("add-on does not provide services".to_owned())
    }

    fn on_ui_event(&mut self, _context: &mut Context, _event: UiEvent) -> SdkResult<()> {
        Ok(())
    }

    fn on_events_lost(&mut self, _context: &mut Context, _loss: EventLoss) -> SdkResult<()> {
        Ok(())
    }

    fn on_damage(&mut self, _context: &mut Context, _event: Damage) -> SdkResult<()> {
        Ok(())
    }

    fn on_combat_started(&mut self, _context: &mut Context, _event: Combat) -> SdkResult<()> {
        Ok(())
    }

    fn on_combat_ended(&mut self, _context: &mut Context, _event: Combat) -> SdkResult<()> {
        Ok(())
    }

    fn on_party_changed(&mut self, _context: &mut Context, _event: PartyChanged) -> SdkResult<()> {
        Ok(())
    }

    fn on_instance_changed(
        &mut self,
        _context: &mut Context,
        _event: InstanceChanged,
    ) -> SdkResult<()> {
        Ok(())
    }

    fn on_zone_changed(&mut self, _context: &mut Context, _event: ZoneChanged) -> SdkResult<()> {
        Ok(())
    }

    fn on_window_opened(&mut self, _context: &mut Context, _event: WindowChanged) -> SdkResult<()> {
        Ok(())
    }

    fn on_window_closed(&mut self, _context: &mut Context, _event: WindowChanged) -> SdkResult<()> {
        Ok(())
    }

    fn on_player_disconnected(
        &mut self,
        _context: &mut Context,
        _event: crate::events::PlayerDisconnected,
    ) -> SdkResult<()> {
        Ok(())
    }

    fn on_messages(&mut self, _context: &mut Context, _messages: Messages) -> SdkResult<()> {
        Ok(())
    }

    fn on_tick(&mut self, _context: &mut Context, _tick: Tick) -> SdkResult<TickControl> {
        Ok(TickControl::Stop)
    }

    fn view(&mut self, _context: &ViewContext) -> SdkResult<Frame> {
        Ok(Frame::empty())
    }

    fn deactivate(&mut self, _reason: ShutdownReason) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AddonWithView;

    impl Addon for AddonWithView {
        fn activate(_context: &mut ActivateContext) -> SdkResult<Self> {
            Ok(Self)
        }
    }

    #[test]
    fn unchanged_is_the_default_callback_output() {
        let ui = Context::new().finish(&mut AddonWithView).expect("finish");
        assert!(matches!(ui, raw_overlay::UiUpdate::Unchanged));
    }

    #[test]
    fn the_last_ui_request_wins() {
        let mut context = Context::new();
        context.clear_ui();
        context.render();
        let ui = context.finish(&mut AddonWithView).expect("finish");
        assert!(matches!(ui, raw_overlay::UiUpdate::Replace(_)));
    }
}

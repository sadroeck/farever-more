//! Rust SDK for Farever add-ons targeting `farever:addon@1.1.0`.
//!
//! Generated WIT bindings are an adapter detail. Add-ons implement [`Addon`]
//! and use SDK-owned events, snapshots, services, and UI builders.

#[allow(warnings)]
#[doc(hidden)]
#[path = "bindings.rs"]
pub mod __wit;

/// Unsupported escape hatch for WIT APIs that do not have an SDK façade yet.
#[cfg(feature = "raw-wit")]
pub use __wit as raw;

pub mod assets;
pub mod bus;
pub mod chat;
mod common;
pub mod config;
pub mod dependencies;
pub mod events;
pub mod game;
mod lifecycle;
pub mod runtime;
pub mod ui;

pub use lifecycle::{
    ActivateContext, Addon, AddonInfo, Context, SdkResult, TickControl, ViewContext,
};
pub use ui::Frame;

/// The exact WIT package implemented by this SDK release line.
pub const WIT_PACKAGE: &str = "farever:addon@1.1.0";

/// Common author-facing SDK types.
pub mod prelude {
    pub use crate::assets::{Image, TextStyle};
    pub use crate::bus::{Message, Messages, Target as MessageTarget};
    pub use crate::common::{EventHeader, StateStatus, UnavailableReason, Vec3};
    pub use crate::config::Setting;
    pub use crate::dependencies::{Dependencies, Service};
    pub use crate::events::{
        ActorRelation, Combat, CombatActor, Damage, DisconnectReason, EventLoss, InstanceChanged,
        PartyChanged, PlayerDisconnected, WindowChanged, ZoneChanged,
    };
    pub use crate::game::{
        Camera, CombatReference, CombatReferenceSlot, CombatState, GameSnapshot, Instance,
        InstanceKind, MapBounds, MapTransform, Observation, Party, PartyMember, Player, Session,
        Snapshot, VisibleMap, Windows, Zone,
    };
    pub use crate::runtime::{ShutdownReason, Tick};
    pub use crate::ui::{
        Alignment, Anchor, CanvasBuilder, Color, Column, ColumnSizing, Frame, FrameBuilder,
        NodeKind, PrimitiveRef, RowKind, RowProgress, Section, Stroke, SurfaceOptions,
        SurfaceStyle, Text, UiEvent, View,
    };
    pub use crate::{
        export, ActivateContext, Addon, AddonInfo, Context, SdkResult, TickControl, ViewContext,
        WIT_PACKAGE,
    };
    pub use std::time::Duration;
}

/// Exports one ordinary Rust [`Addon`] as the Farever WIT component world.
#[macro_export]
macro_rules! export {
    ($addon:ident) => {
        #[doc(hidden)]
        mod __farever_addon_export {
            use super::$addon;

            struct Component;

            std::thread_local! {
                static INSTANCE: std::cell::RefCell<Option<$addon>> =
                    const { std::cell::RefCell::new(None) };
            }

            impl $crate::__wit::exports::farever::addon::plugin::Guest for Component {
                fn activate(
                    context: $crate::__wit::exports::farever::addon::plugin::ActivationContext,
                ) -> Result<
                    $crate::__wit::exports::farever::addon::plugin::ActivationOutput,
                    String,
                > {
                    INSTANCE.with(|instance| {
                        if instance.borrow().is_some() {
                            return Err("Farever add-on was activated more than once".to_owned());
                        }
                        let mut context = $crate::ActivateContext::new(context);
                        let mut addon = <$addon as $crate::Addon>::activate(&mut context)?;
                        let ui = context.finish(&mut addon)?;
                        *instance.borrow_mut() = Some(addon);
                        Ok($crate::__wit::exports::farever::addon::plugin::ActivationOutput { ui })
                    })
                }

                fn call_service(
                    service: String,
                    operation: u32,
                    request: Vec<u8>,
                ) -> Result<Vec<u8>, String> {
                    INSTANCE.with(|instance| {
                        let mut instance = instance.borrow_mut();
                        let addon = instance
                            .as_mut()
                            .ok_or_else(|| "Farever add-on is not active".to_owned())?;
                        <$addon as $crate::Addon>::call_service(
                            addon,
                            &service,
                            operation,
                            &request,
                        )
                    })
                }

                fn on_ui_event(
                    event: $crate::__wit::farever::addon::overlay::UiEvent,
                ) -> Result<
                    $crate::__wit::exports::farever::addon::plugin::CallbackOutput,
                    String,
                > {
                    INSTANCE.with(|instance| {
                        let mut instance = instance.borrow_mut();
                        let addon = instance.as_mut().ok_or_else(|| "Farever add-on is not active".to_owned())?;
                        let mut context = $crate::Context::new();
                        <$addon as $crate::Addon>::on_ui_event(
                            addon,
                            &mut context,
                            $crate::ui::UiEvent::from_raw(event),
                        )?;
                        let ui = context.finish(addon)?;
                        Ok($crate::__wit::exports::farever::addon::plugin::CallbackOutput { ui })
                    })
                }

                fn on_event(
                    batch: $crate::__wit::farever::addon::events::EventBatch,
                ) -> Result<
                    $crate::__wit::exports::farever::addon::plugin::CallbackOutput,
                    String,
                > {
                    INSTANCE.with(|instance| {
                        let mut instance = instance.borrow_mut();
                        let addon = instance.as_mut().ok_or_else(|| "Farever add-on is not active".to_owned())?;
                        let mut context = $crate::Context::new();
                        $crate::events::dispatch(addon, &mut context, batch)?;
                        let ui = context.finish(addon)?;
                        Ok($crate::__wit::exports::farever::addon::plugin::CallbackOutput { ui })
                    })
                }

                fn on_message(
                    dropped_before: u64,
                    messages: Vec<$crate::__wit::farever::addon::bus::AddonMessage>,
                ) -> Result<
                    $crate::__wit::exports::farever::addon::plugin::CallbackOutput,
                    String,
                > {
                    INSTANCE.with(|instance| {
                        let mut instance = instance.borrow_mut();
                        let addon = instance.as_mut().ok_or_else(|| "Farever add-on is not active".to_owned())?;
                        let mut context = $crate::Context::new();
                        <$addon as $crate::Addon>::on_messages(
                            addon,
                            &mut context,
                            $crate::bus::Messages::from_raw(dropped_before, messages),
                        )?;
                        let ui = context.finish(addon)?;
                        Ok($crate::__wit::exports::farever::addon::plugin::CallbackOutput { ui })
                    })
                }

                fn on_tick(
                    tick: $crate::__wit::exports::farever::addon::plugin::Tick,
                ) -> Result<
                    $crate::__wit::exports::farever::addon::plugin::TickOutput,
                    String,
                > {
                    INSTANCE.with(|instance| {
                        let mut instance = instance.borrow_mut();
                        let addon = instance.as_mut().ok_or_else(|| "Farever add-on is not active".to_owned())?;
                        let mut context = $crate::Context::new();
                        let control = <$addon as $crate::Addon>::on_tick(
                            addon,
                            &mut context,
                            $crate::runtime::Tick::from_raw(tick),
                        )?;
                        let ui = context.finish(addon)?;
                        Ok($crate::__wit::exports::farever::addon::plugin::TickOutput {
                            continue_ticking: matches!(control, $crate::TickControl::Continue),
                            ui,
                        })
                    })
                }

                fn deactivate(
                    reason: $crate::__wit::exports::farever::addon::plugin::DeactivationReason,
                ) {
                    INSTANCE.with(|instance| {
                        if let Some(mut addon) = instance.borrow_mut().take() {
                            <$addon as $crate::Addon>::deactivate(
                                &mut addon,
                                $crate::runtime::ShutdownReason::from_raw(reason),
                            );
                        }
                    });
                }
            }

            $crate::__wit::export!(Component with_types_in $crate::__wit);
        }
    };
}

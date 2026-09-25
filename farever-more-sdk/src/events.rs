//! SDK-owned game events and internal WIT batch dispatch.

use crate::__wit::farever::addon::{combat as raw_combat, events as raw_events};
use crate::__wit::farever::addon::{instance_session as raw_instance, party as raw_party};
use crate::__wit::farever::addon::{windows as raw_windows, zone as raw_zone};
use crate::assets::Image;
use crate::common::EventHeader;
pub use crate::game::{Instance, InstanceKind};
use crate::game::{Party, PartyMember};
use crate::{Addon, Context, SdkResult};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EventLoss {
    pub process_session: u64,
    pub dropped_before: u64,
    pub snapshot_required: bool,
    pub resume_at_sequence: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActorRelation {
    LocalPlayer,
    GroupMember,
    Other,
    Unknown,
}

impl From<raw_combat::ActorRelation> for ActorRelation {
    fn from(value: raw_combat::ActorRelation) -> Self {
        match value {
            raw_combat::ActorRelation::LocalPlayer => Self::LocalPlayer,
            raw_combat::ActorRelation::GroupMember => Self::GroupMember,
            raw_combat::ActorRelation::Other => Self::Other,
            raw_combat::ActorRelation::Unknown => Self::Unknown,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CombatActor {
    pub actor_id: Option<String>,
    pub relation: ActorRelation,
    pub kind: Option<String>,
}

impl From<raw_combat::CombatActorRef> for CombatActor {
    fn from(value: raw_combat::CombatActorRef) -> Self {
        Self {
            actor_id: value.actor_id,
            relation: value.relation.into(),
            kind: value.kind,
        }
    }
}

impl CombatActor {
    #[must_use]
    pub const fn is_local_player(&self) -> bool {
        matches!(self.relation, ActorRelation::LocalPlayer)
    }

    #[must_use]
    pub fn party_member<'a>(&self, party: &'a Party) -> Option<&'a PartyMember> {
        if !matches!(
            self.relation,
            ActorRelation::LocalPlayer | ActorRelation::GroupMember
        ) {
            return None;
        }
        self.actor_id
            .as_deref()
            .and_then(|actor_id| party.member(actor_id))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Damage {
    pub header: EventHeader,
    pub source: CombatActor,
    pub target: CombatActor,
    pub skill_id: String,
    pub skill_display_name: Option<String>,
    pub skill_icon: Option<Image>,
    pub amount: f64,
    pub hit_count: u32,
    pub critical: bool,
    pub killed: bool,
    pub blocked: Option<f64>,
}

impl From<raw_combat::DamageEvent> for Damage {
    fn from(value: raw_combat::DamageEvent) -> Self {
        Self {
            header: value.header.into(),
            source: value.source.into(),
            target: value.target.into(),
            skill_id: value.skill_id,
            skill_display_name: value.skill_display_name,
            skill_icon: value.skill_icon.map(Into::into),
            amount: value.amount,
            hit_count: value.hit_count,
            critical: value.critical,
            killed: value.killed,
            blocked: value.blocked,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Combat {
    pub header: EventHeader,
    pub fight_id: u64,
}

impl From<raw_combat::CombatEvent> for Combat {
    fn from(value: raw_combat::CombatEvent) -> Self {
        Self {
            header: value.header.into(),
            fight_id: value.fight_id,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PartyChanged {
    pub header: EventHeader,
    pub revision: u64,
}

impl From<raw_party::PartyEvent> for PartyChanged {
    fn from(value: raw_party::PartyEvent) -> Self {
        Self {
            header: value.header.into(),
            revision: value.revision,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstanceChanged {
    pub header: EventHeader,
    pub previous: Option<Instance>,
    pub current: Option<Instance>,
}

impl From<raw_instance::InstanceEvent> for InstanceChanged {
    fn from(value: raw_instance::InstanceEvent) -> Self {
        Self {
            header: value.header.into(),
            previous: value.previous.map(Into::into),
            current: value.current.map(Into::into),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ZoneChanged {
    pub header: EventHeader,
    pub previous_area_id: Option<String>,
    pub area_id: Option<String>,
}

impl From<raw_zone::ZoneEvent> for ZoneChanged {
    fn from(value: raw_zone::ZoneEvent) -> Self {
        Self {
            header: value.header.into(),
            previous_area_id: value.previous_area_id,
            area_id: value.area_id,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowChanged {
    pub header: EventHeader,
    pub window_id: String,
}

impl From<raw_windows::WindowEvent> for WindowChanged {
    fn from(value: raw_windows::WindowEvent) -> Self {
        Self {
            header: value.header.into(),
            window_id: value.window_id,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DisconnectReason {
    ManualExit,
    Kick,
    Timeout,
    SwitchingServer,
    Unknown,
}

impl From<raw_events::DisconnectReason> for DisconnectReason {
    fn from(value: raw_events::DisconnectReason) -> Self {
        match value {
            raw_events::DisconnectReason::ManualExit => Self::ManualExit,
            raw_events::DisconnectReason::Kick => Self::Kick,
            raw_events::DisconnectReason::Timeout => Self::Timeout,
            raw_events::DisconnectReason::SwitchingServer => Self::SwitchingServer,
            raw_events::DisconnectReason::Unknown => Self::Unknown,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlayerDisconnected {
    pub header: EventHeader,
    pub reason: DisconnectReason,
}

impl From<raw_events::PlayerDisconnectedEvent> for PlayerDisconnected {
    fn from(value: raw_events::PlayerDisconnectedEvent) -> Self {
        Self {
            header: value.header.into(),
            reason: value.reason.into(),
        }
    }
}
#[doc(hidden)]
pub fn dispatch<A: Addon>(
    addon: &mut A,
    context: &mut Context,
    batch: raw_events::EventBatch,
) -> SdkResult<()> {
    if batch.dropped_before > 0 || batch.snapshot_required {
        addon.on_events_lost(
            context,
            EventLoss {
                process_session: batch.process_session,
                dropped_before: batch.dropped_before,
                snapshot_required: batch.snapshot_required,
                resume_at_sequence: batch.next_sequence,
            },
        )?;
    }

    for event in batch.events {
        match event {
            raw_events::Event::Damage(event) => addon.on_damage(context, event.into())?,
            raw_events::Event::CombatStarted(event) => {
                addon.on_combat_started(context, event.into())?
            }
            raw_events::Event::CombatEnded(event) => {
                addon.on_combat_ended(context, event.into())?
            }
            raw_events::Event::PartyChanged(event) => {
                addon.on_party_changed(context, event.into())?
            }
            raw_events::Event::InstanceChanged(event) => {
                addon.on_instance_changed(context, event.into())?
            }
            raw_events::Event::ZoneChanged(event) => {
                addon.on_zone_changed(context, event.into())?
            }
            raw_events::Event::WindowOpened(event) => {
                addon.on_window_opened(context, event.into())?
            }
            raw_events::Event::WindowClosed(event) => {
                addon.on_window_closed(context, event.into())?
            }
            raw_events::Event::PlayerDisconnected(event) => {
                addon.on_player_disconnected(context, event.into())?
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::__wit::farever::addon::common::EventHeader as RawEventHeader;
    use crate::{ActivateContext, Frame, ViewContext};

    #[derive(Default)]
    struct Recorder {
        calls: Vec<&'static str>,
        loss: Option<EventLoss>,
    }

    impl Addon for Recorder {
        fn activate(_context: &mut ActivateContext) -> SdkResult<Self> {
            Ok(Self::default())
        }

        fn on_events_lost(&mut self, context: &mut Context, loss: EventLoss) -> SdkResult<()> {
            self.calls.push("loss");
            self.loss = Some(loss);
            context.render();
            Ok(())
        }

        fn on_zone_changed(
            &mut self,
            _context: &mut Context,
            _event: ZoneChanged,
        ) -> SdkResult<()> {
            self.calls.push("zone");
            Ok(())
        }

        fn view(&mut self, _context: &ViewContext) -> SdkResult<Frame> {
            Ok(Frame::empty())
        }
    }

    #[test]
    fn reports_loss_before_dispatching_typed_events() {
        let mut recorder = Recorder::default();
        let mut context = Context::new();
        let batch = raw_events::EventBatch {
            process_session: 42,
            first_sequence: Some(10),
            next_sequence: 11,
            dropped_before: 3,
            snapshot_required: true,
            events: vec![raw_events::Event::ZoneChanged(raw_zone::ZoneEvent {
                header: RawEventHeader {
                    sequence: 10,
                    monotonic_ms: 500,
                },
                previous_area_id: Some("old".to_owned()),
                area_id: Some("new".to_owned()),
            })],
        };

        dispatch(&mut recorder, &mut context, batch).expect("dispatch should succeed");

        assert_eq!(recorder.calls, ["loss", "zone"]);
        assert!(context.render_requested());
        assert_eq!(
            recorder.loss,
            Some(EventLoss {
                process_session: 42,
                dropped_before: 3,
                snapshot_required: true,
                resume_at_sequence: 11,
            })
        );
    }

    #[test]
    fn combat_actor_resolves_only_validated_party_relations() {
        let party = Party {
            party_id: Some("party-1".to_owned()),
            members: vec![PartyMember {
                actor_id: "actor-1".to_owned(),
                is_local: false,
                name: Some("Ally".to_owned()),
                class_id: None,
                class_icon: None,
                in_combat: Some(true),
            }],
        };
        let member = CombatActor {
            actor_id: Some("actor-1".to_owned()),
            relation: ActorRelation::GroupMember,
            kind: None,
        };
        let unrelated = CombatActor {
            relation: ActorRelation::Other,
            ..member.clone()
        };

        assert_eq!(
            member
                .party_member(&party)
                .and_then(|member| member.name.as_deref()),
            Some("Ally")
        );
        assert!(unrelated.party_member(&party).is_none());
    }
}

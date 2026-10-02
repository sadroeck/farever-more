//! One coherent, callback-frozen view of current game state.

use crate::__wit::farever::addon::{
    camera as raw_camera, combat as raw_combat, game as raw_game, instance_session as raw_instance,
    map as raw_map, party as raw_party, player as raw_player, windows as raw_windows,
    zone as raw_zone,
};
use crate::assets::Image;
use crate::common::{StateStatus, Vec3};

#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot<T> {
    pub status: StateStatus,
    pub value: Option<T>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MapBounds {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MapTransform {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub tx: f32,
    pub ty: f32,
}

impl MapTransform {
    /// Project world X/Y into physical game-client pixels.
    #[must_use]
    pub fn project(self, position: [f32; 2]) -> [f32; 2] {
        [
            self.a * position[0] + self.c * position[1] + self.tx,
            self.b * position[0] + self.d * position[1] + self.ty,
        ]
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct VisibleMap {
    pub world: String,
    pub bounds: MapBounds,
    pub world_to_client: MapTransform,
    pub pixels_per_point: f32,
}

impl From<raw_map::VisibleMap> for VisibleMap {
    fn from(value: raw_map::VisibleMap) -> Self {
        let bounds = value.bounds;
        let transform = value.world_to_client;
        Self {
            world: value.world,
            bounds: MapBounds {
                left: bounds.left,
                top: bounds.top,
                width: bounds.width,
                height: bounds.height,
            },
            world_to_client: MapTransform {
                a: transform.a,
                b: transform.b,
                c: transform.c,
                d: transform.d,
                tx: transform.tx,
                ty: transform.ty,
            },
            pixels_per_point: value.pixels_per_point,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Observation {
    pub sequence: u64,
    pub captured_at_ms: u64,
    pub process_session: u64,
}

impl From<raw_game::ObservationMetadata> for Observation {
    fn from(value: raw_game::ObservationMetadata) -> Self {
        Self {
            sequence: value.sequence,
            captured_at_ms: value.captured_at_ms,
            process_session: value.process_session,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Session {
    pub process_session: u64,
    pub in_world: bool,
}

impl From<raw_game::SessionState> for Session {
    fn from(value: raw_game::SessionState) -> Self {
        Self {
            process_session: value.process_session,
            in_world: value.in_world,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Player {
    pub runtime_id: Option<String>,
    pub name: Option<String>,
    pub class_id: Option<String>,
    pub level: Option<u32>,
    pub position: Option<Vec3>,
    pub heading_radians: Option<f32>,
    pub health: Option<f64>,
    pub max_health: Option<f64>,
}

impl From<raw_player::PlayerState> for Player {
    fn from(value: raw_player::PlayerState) -> Self {
        Self {
            runtime_id: value.runtime_id,
            name: value.name,
            class_id: value.class_id,
            level: value.level,
            position: value.position.map(Into::into),
            heading_radians: value.heading_radians,
            health: value.health,
            max_health: value.max_health,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CombatReferenceSlot {
    Target,
    LockedTarget,
    AutoTarget,
}

impl From<raw_combat::CombatReferenceSlot> for CombatReferenceSlot {
    fn from(value: raw_combat::CombatReferenceSlot) -> Self {
        match value {
            raw_combat::CombatReferenceSlot::Target => Self::Target,
            raw_combat::CombatReferenceSlot::LockedTarget => Self::LockedTarget,
            raw_combat::CombatReferenceSlot::AutoTarget => Self::AutoTarget,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CombatReference {
    pub slot: CombatReferenceSlot,
    pub position: Option<Vec3>,
}

impl From<raw_combat::CombatReference> for CombatReference {
    fn from(value: raw_combat::CombatReference) -> Self {
        Self {
            slot: value.slot.into(),
            position: value.position.map(Into::into),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CombatState {
    pub in_combat: Option<bool>,
    pub references: Vec<CombatReference>,
}

impl From<raw_combat::CombatState> for CombatState {
    fn from(value: raw_combat::CombatState) -> Self {
        Self {
            in_combat: value.in_combat,
            references: value.references.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PartyMember {
    pub actor_id: String,
    pub is_local: bool,
    pub name: Option<String>,
    pub class_id: Option<String>,
    pub class_icon: Option<Image>,
    pub in_combat: Option<bool>,
}

impl From<raw_party::PartyMember> for PartyMember {
    fn from(value: raw_party::PartyMember) -> Self {
        Self {
            actor_id: value.actor_id,
            is_local: value.is_local,
            name: value.name,
            class_id: value.class_id,
            class_icon: value.class_icon.map(Into::into),
            in_combat: value.in_combat,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Party {
    pub party_id: Option<String>,
    pub members: Vec<PartyMember>,
}

impl Party {
    #[must_use]
    pub fn local_member(&self) -> Option<&PartyMember> {
        self.members.iter().find(|member| member.is_local)
    }

    #[must_use]
    pub fn member(&self, actor_id: &str) -> Option<&PartyMember> {
        self.members
            .iter()
            .find(|member| member.actor_id == actor_id)
    }
}

impl From<raw_party::PartyState> for Party {
    fn from(value: raw_party::PartyState) -> Self {
        Self {
            party_id: value.party_id,
            members: value.members.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstanceKind {
    OpenWorld,
    Dungeon,
    Other,
    Unknown,
}

impl From<raw_instance::InstanceKind> for InstanceKind {
    fn from(value: raw_instance::InstanceKind) -> Self {
        match value {
            raw_instance::InstanceKind::OpenWorld => Self::OpenWorld,
            raw_instance::InstanceKind::Dungeon => Self::Dungeon,
            raw_instance::InstanceKind::Other => Self::Other,
            raw_instance::InstanceKind::Unknown => Self::Unknown,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Instance {
    pub session_id: u64,
    pub kind: InstanceKind,
    pub area_id: Option<String>,
}

impl From<raw_instance::InstanceState> for Instance {
    fn from(value: raw_instance::InstanceState) -> Self {
        Self {
            session_id: value.session_id,
            kind: value.kind.into(),
            area_id: value.area_id,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Zone {
    pub area_id: Option<String>,
    pub display_name: Option<String>,
}

impl From<raw_zone::ZoneState> for Zone {
    fn from(value: raw_zone::ZoneState) -> Self {
        Self {
            area_id: value.area_id,
            display_name: value.display_name,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    pub heading_radians: f32,
}

impl From<raw_camera::CameraState> for Camera {
    fn from(value: raw_camera::CameraState) -> Self {
        Self {
            heading_radians: value.heading_radians,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Windows {
    pub open: Vec<String>,
    pub focused: Option<String>,
}

impl From<raw_windows::WindowsState> for Windows {
    fn from(value: raw_windows::WindowsState) -> Self {
        Self {
            open: value.open_windows,
            focused: value.focused_window,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct GameSnapshot {
    pub observation: Observation,
    pub session: Session,
    pub player: Snapshot<Player>,
    pub camera: Snapshot<Camera>,
    pub windows: Snapshot<Windows>,
}

pub struct Game {
    _private: (),
}

impl Game {
    #[doc(hidden)]
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Captures all commonly used game domains. The host guarantees that every
    /// getter called in one callback refers to the same observation.
    #[must_use]
    pub fn snapshot(&self) -> GameSnapshot {
        GameSnapshot {
            observation: raw_game::observation().into(),
            session: raw_game::session().into(),
            player: self.player(),
            camera: self.camera(),
            windows: self.windows(),
        }
    }

    #[must_use]
    pub fn observation(&self) -> Observation {
        raw_game::observation().into()
    }

    #[must_use]
    pub fn player(&self) -> Snapshot<Player> {
        let snapshot = raw_player::current();
        Snapshot {
            status: snapshot.status.into(),
            value: snapshot.value.map(Into::into),
        }
    }

    #[must_use]
    pub fn camera(&self) -> Snapshot<Camera> {
        let snapshot = raw_camera::current();
        Snapshot {
            status: snapshot.status.into(),
            value: snapshot.value.map(Into::into),
        }
    }

    #[must_use]
    pub fn windows(&self) -> Snapshot<Windows> {
        let snapshot = raw_windows::current();
        Snapshot {
            status: snapshot.status.into(),
            value: snapshot.value.map(Into::into),
        }
    }

    /// Visible full-map geometry frozen for this callback.
    #[must_use]
    pub fn map(&self) -> Snapshot<VisibleMap> {
        let snapshot = raw_map::current();
        Snapshot {
            status: snapshot.status.into(),
            value: snapshot.value.map(Into::into),
        }
    }

    #[must_use]
    pub fn combat(&self) -> Snapshot<CombatState> {
        let snapshot = raw_combat::current();
        Snapshot {
            status: snapshot.status.into(),
            value: snapshot.value.map(Into::into),
        }
    }

    #[must_use]
    pub fn party(&self) -> Snapshot<Party> {
        let snapshot = raw_party::current();
        Snapshot {
            status: snapshot.status.into(),
            value: snapshot.value.map(Into::into),
        }
    }

    #[must_use]
    pub fn instance(&self) -> Snapshot<Instance> {
        let snapshot = raw_instance::current();
        Snapshot {
            status: snapshot.status.into(),
            value: snapshot.value.map(Into::into),
        }
    }

    #[must_use]
    pub fn zone(&self) -> Snapshot<Zone> {
        let snapshot = raw_zone::current();
        Snapshot {
            status: snapshot.status.into(),
            value: snapshot.value.map(Into::into),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn party_helpers_use_opaque_actor_ids() {
        let party = Party {
            party_id: Some("party-1".to_owned()),
            members: vec![
                PartyMember {
                    actor_id: "actor-local".to_owned(),
                    is_local: true,
                    name: Some("Local".to_owned()),
                    class_id: None,
                    class_icon: None,
                    in_combat: Some(true),
                },
                PartyMember {
                    actor_id: "actor-remote".to_owned(),
                    is_local: false,
                    name: Some("Remote".to_owned()),
                    class_id: None,
                    class_icon: None,
                    in_combat: Some(true),
                },
            ],
        };

        assert_eq!(
            party.local_member().map(|member| member.actor_id.as_str()),
            Some("actor-local")
        );
        assert_eq!(
            party
                .member("actor-remote")
                .and_then(|member| member.name.as_deref()),
            Some("Remote")
        );
        assert!(party.member("missing").is_none());
    }

    #[test]
    fn hook_backed_domain_values_convert_to_sdk_types() {
        let combat: CombatState = raw_combat::CombatState {
            in_combat: Some(true),
            references: vec![raw_combat::CombatReference {
                slot: raw_combat::CombatReferenceSlot::LockedTarget,
                position: Some(crate::__wit::farever::addon::common::Vec3 {
                    x: 1.0,
                    y: 2.0,
                    z: 3.0,
                }),
            }],
        }
        .into();
        let instance: Instance = raw_instance::InstanceState {
            session_id: 7,
            kind: raw_instance::InstanceKind::Other,
            area_id: Some("rift".to_owned()),
        }
        .into();
        let zone: Zone = raw_zone::ZoneState {
            area_id: Some("World/W1_Siagarta".to_owned()),
            display_name: Some("Siagarta".to_owned()),
        }
        .into();

        assert_eq!(combat.in_combat, Some(true));
        assert_eq!(combat.references[0].slot, CombatReferenceSlot::LockedTarget);
        assert_eq!(
            combat.references[0].position.map(|position| position.z),
            Some(3.0)
        );
        assert_eq!(instance.session_id, 7);
        assert_eq!(instance.kind, InstanceKind::Other);
        assert_eq!(instance.area_id.as_deref(), Some("rift"));
        assert_eq!(zone.area_id.as_deref(), Some("World/W1_Siagarta"));
        assert_eq!(zone.display_name.as_deref(), Some("Siagarta"));
    }
}

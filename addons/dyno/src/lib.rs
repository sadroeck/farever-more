use farever_more_sdk::prelude::*;
use farever_more_sdk::ui;
use std::collections::BTreeMap;

const DYNO_TOPIC: &str = "dyno";
const DPS_TOPIC: &str = "dps";
const HOST_MESSAGE_SOURCE_ID: &str = "farever.host";
const METER_WIDTH: f32 = 440.0;
const METER_RIGHT_MARGIN: f32 = 24.0;
const METER_BOTTOM_MARGIN: f32 = 190.0;
const MAX_VISIBLE_ROWS: usize = 30;
const RENDER_INTERVAL: Duration = Duration::from_millis(250);
const VISIBLE: Setting<bool> = Setting::boolean("meter-visible", true)
    .label("Enable damage meter")
    .description("Show the meter when encounter data is available.");
const SKILL_ROW_HEIGHT: f32 = 21.0;
const SKILL_ICON_SIZE: f32 = SKILL_ROW_HEIGHT;
const NOTO_REGULAR: &[u8] = include_bytes!("../assets/fonts/NotoSans-Regular.ttf");
const NOTO_BOLD: &[u8] = include_bytes!("../assets/fonts/NotoSans-Bold.ttf");
const CLASS_ICON_PNGS: [(&str, &str, &[u8]); 4] = [
    (
        "warrior",
        "class-warrior",
        include_bytes!("../assets/classes/warrior.png"),
    ),
    (
        "mage",
        "class-mage",
        include_bytes!("../assets/classes/mage.png"),
    ),
    (
        "rogue",
        "class-rogue",
        include_bytes!("../assets/classes/rogue.png"),
    ),
    (
        "priest",
        "class-priest",
        include_bytes!("../assets/classes/priest.png"),
    ),
];

const ACTIVE_COLOR: Color = Color::rgba(0.82, 0.35, 0.27, 1.0);
const IDLE_COLOR: Color = Color::rgba(0.48, 0.41, 0.38, 1.0);
const SURFACE_FILL: Color = Color::rgba(0.79, 0.72, 0.68, 0.96);
const SURFACE_STROKE: Color = Color::rgba(0.61, 0.47, 0.4, 1.0);
const TITLE_BACKGROUND: Color = Color::rgba(0.92, 0.83, 0.79, 1.0);
const ROW_BACKGROUND: Color = Color::rgba(0.82, 0.75, 0.71, 0.88);
const ROW_BACKGROUND_ALT: Color = Color::rgba(0.79, 0.71, 0.67, 0.78);
const TEXT_INK: Color = Color::rgba(0.12, 0.09, 0.075, 1.0);
const WARNING_TEXT: Color = Color::rgba(0.58, 0.25, 0.19, 1.0);

#[derive(Default)]
struct SkillTotal {
    display_name: Option<String>,
    icon: Option<Image>,
    damage: f64,
    hits: u64,
    criticals: u64,
    kills: u64,
    max_hit: f64,
    /// Raw `_block` observations are deliberately not added to damage done.
    blocked_reported: f64,
    block_observations: u64,
}

#[derive(Default)]
struct PlayerTotal {
    name: Option<String>,
    class_id: Option<String>,
    class_icon: Option<Image>,
    damage: f64,
}

#[derive(Default)]
struct ClassIcons {
    by_class: BTreeMap<String, Image>,
}

impl ClassIcons {
    fn register(context: &ActivateContext) -> SdkResult<Self> {
        let assets = context.assets();
        let mut by_class = BTreeMap::new();
        for (class_id, image_id, png) in CLASS_ICON_PNGS {
            let image = assets
                .register_image(image_id, png)
                .map_err(|error| format!("failed to register {class_id} class icon: {error}"))?;
            by_class.insert(class_id.to_owned(), image);
        }
        Ok(Self { by_class })
    }

    fn resolve(&self, class_id: &str) -> Option<&Image> {
        let class_id = class_id.trim();
        let class_id = class_id
            .get(..6)
            .filter(|prefix| {
                prefix.eq_ignore_ascii_case("class_") || prefix.eq_ignore_ascii_case("class-")
            })
            .and_then(|_| class_id.get(6..))
            .unwrap_or(class_id);
        self.by_class
            .iter()
            .find_map(|(known, icon)| known.eq_ignore_ascii_case(class_id).then_some(icon))
    }

    fn apply(&self, party: &mut Party) {
        for member in &mut party.members {
            if let Some(icon) = member
                .class_id
                .as_deref()
                .and_then(|class_id| self.resolve(class_id))
            {
                member.class_icon = Some(icon.clone());
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DpsCommand {
    Reset,
    Show,
    Hide,
}

struct MeterState {
    fight_id: Option<u64>,
    active: bool,
    ticking: bool,
    started_ms: Option<u64>,
    ended_ms: Option<u64>,
    total_damage: f64,
    hit_count: u64,
    critical_count: u64,
    kill_count: u64,
    blocked_reported: f64,
    block_observations: u64,
    event_gap: bool,
    visible: bool,
    skills: BTreeMap<String, SkillTotal>,
    current_players: BTreeMap<String, PlayerTotal>,
    overall_players: BTreeMap<String, PlayerTotal>,
    overall_elapsed_ms: u64,
    current_committed_duration_ms: u64,
    party: Option<Party>,
    instance: Option<Instance>,
}

impl Default for MeterState {
    fn default() -> Self {
        Self {
            fight_id: None,
            active: false,
            ticking: false,
            started_ms: None,
            ended_ms: None,
            total_damage: 0.0,
            hit_count: 0,
            critical_count: 0,
            kill_count: 0,
            blocked_reported: 0.0,
            block_observations: 0,
            event_gap: false,
            visible: true,
            skills: BTreeMap::new(),
            current_players: BTreeMap::new(),
            overall_players: BTreeMap::new(),
            overall_elapsed_ms: 0,
            current_committed_duration_ms: 0,
            party: None,
            instance: None,
        }
    }
}

impl MeterState {
    fn apply_command(&mut self, command: DpsCommand) -> &'static str {
        match command {
            DpsCommand::Reset => {
                self.reset_encounter();
                "Damage meter reset"
            }
            DpsCommand::Show => {
                self.visible = true;
                "Damage meter shown"
            }
            DpsCommand::Hide => {
                self.visible = false;
                "Damage meter hidden"
            }
        }
    }

    fn begin(&mut self, fight_id: Option<u64>, started_ms: u64) {
        if self.active {
            self.finish_encounter_duration(started_ms);
        }
        self.clear_current_encounter();
        self.fight_id = fight_id;
        self.active = true;
        self.started_ms = Some(started_ms);
    }

    fn reset_encounter(&mut self) {
        for (actor_id, current) in &self.current_players {
            if let Some(overall) = self.overall_players.get_mut(actor_id) {
                overall.damage = (overall.damage - current.damage).max(0.0);
            }
        }
        self.overall_players.retain(|_, player| player.damage > 0.0);
        self.overall_elapsed_ms = self
            .overall_elapsed_ms
            .saturating_sub(self.current_committed_duration_ms);
        self.clear_current_encounter();
    }

    fn clear_current_encounter(&mut self) {
        self.fight_id = None;
        self.active = false;
        self.ticking = false;
        self.started_ms = None;
        self.ended_ms = None;
        self.total_damage = 0.0;
        self.hit_count = 0;
        self.critical_count = 0;
        self.kill_count = 0;
        self.blocked_reported = 0.0;
        self.block_observations = 0;
        self.skills.clear();
        self.current_players.clear();
        self.current_committed_duration_ms = 0;
    }

    fn observe_game_state(&mut self, party: Option<Party>, instance: Option<Instance>) {
        self.observe_instance(instance);
        self.observe_party(party);
    }

    fn observe_instance(&mut self, instance: Option<Instance>) {
        let Some(instance) = instance else {
            // Loading and transient provider failures are not instance exits.
            return;
        };
        if self
            .instance
            .as_ref()
            .is_some_and(|current| current.session_id != instance.session_id)
        {
            self.clear_current_encounter();
            self.overall_players.clear();
            self.overall_elapsed_ms = 0;
            self.current_committed_duration_ms = 0;
            self.event_gap = false;
        }
        self.instance = Some(instance);
    }

    fn observe_instance_edge(&mut self, instance: Option<Instance>) {
        if instance.is_some() {
            self.observe_instance(instance);
        } else {
            self.clear_current_encounter();
            self.overall_players.clear();
            self.overall_elapsed_ms = 0;
            self.current_committed_duration_ms = 0;
            self.instance = None;
            self.event_gap = false;
        }
    }

    fn observe_party(&mut self, party: Option<Party>) {
        let Some(party) = party else {
            // Preserve the last validated roster through transient provider
            // gaps. A later live snapshot proves membership changes.
            return;
        };
        for member in &party.members {
            update_player_metadata(&mut self.current_players, member);
            update_player_metadata(&mut self.overall_players, member);
        }
        self.party = Some(party);
    }

    fn is_instanced(&self) -> bool {
        self.instance.as_ref().is_some_and(|instance| {
            matches!(instance.kind, InstanceKind::Dungeon | InstanceKind::Other)
        })
    }

    fn is_group_instance(&self) -> bool {
        self.is_instanced()
            && self
                .party
                .as_ref()
                .is_some_and(|party| party.members.len() > 1)
    }

    fn finish_encounter_duration(&mut self, ended_ms: u64) {
        if self.is_instanced() {
            if let Some(started_ms) = self.started_ms {
                let duration_ms = ended_ms.saturating_sub(started_ms);
                self.overall_elapsed_ms = self.overall_elapsed_ms.saturating_add(duration_ms);
                self.current_committed_duration_ms = duration_ms;
            }
        }
    }

    fn elapsed_seconds(&self, now_ms: u64) -> f64 {
        self.started_ms.map_or(1.0, |started_ms| {
            let end_ms = self.ended_ms.unwrap_or(now_ms).max(started_ms);
            ((end_ms - started_ms) as f64 / 1000.0).max(1.0)
        })
    }

    fn sorted_skills(&self) -> Vec<(&str, &SkillTotal)> {
        let mut skills = self
            .skills
            .iter()
            .map(|(id, skill)| (id.as_str(), skill))
            .collect::<Vec<_>>();
        skills.sort_by(|left, right| {
            right
                .1
                .damage
                .total_cmp(&left.1.damage)
                .then_with(|| left.0.cmp(right.0))
        });
        skills
    }

    fn max_skill_damage(&self) -> f64 {
        self.skills
            .values()
            .map(|skill| skill.damage)
            .fold(0.0, f64::max)
    }

    fn sorted_players(&self, overall: bool) -> Vec<(&str, &PlayerTotal)> {
        let totals = if overall {
            &self.overall_players
        } else {
            &self.current_players
        };
        let mut players = totals
            .iter()
            .filter(|(_, player)| player.damage > 0.0)
            .map(|(id, player)| (id.as_str(), player))
            .collect::<Vec<_>>();
        players.sort_by(|left, right| {
            right
                .1
                .damage
                .total_cmp(&left.1.damage)
                .then_with(|| left.0.cmp(right.0))
        });
        players
    }

    fn player_total_damage(&self, overall: bool) -> f64 {
        let totals = if overall {
            &self.overall_players
        } else {
            &self.current_players
        };
        totals.values().map(|player| player.damage).sum()
    }

    fn player_elapsed_seconds(&self, overall: bool, now_ms: u64) -> f64 {
        if !overall {
            return self.elapsed_seconds(now_ms);
        }
        let current_ms = if self.active {
            self.started_ms
                .map(|started_ms| now_ms.saturating_sub(started_ms))
                .unwrap_or(0)
        } else {
            0
        };
        ((self.overall_elapsed_ms.saturating_add(current_ms)) as f64 / 1000.0).max(1.0)
    }

    fn on_events_lost(&mut self) {
        self.event_gap = true;
    }

    fn on_combat_started(&mut self, event: Combat) -> SdkResult<()> {
        if self.active && self.fight_id == Some(event.fight_id) {
            return Ok(());
        }
        if self.active && self.fight_id.is_none() {
            self.fight_id = Some(event.fight_id);
            self.started_ms = Some(
                self.started_ms
                    .map_or(event.header.monotonic_ms, |started| {
                        started.min(event.header.monotonic_ms)
                    }),
            );
            return Ok(());
        }
        self.begin(Some(event.fight_id), event.header.monotonic_ms);
        Ok(())
    }

    fn on_combat_ended(&mut self, event: Combat) -> SdkResult<()> {
        if !self.active || self.fight_id.is_some_and(|id| id != event.fight_id) {
            return Ok(());
        }
        self.finish_encounter_duration(event.header.monotonic_ms);
        self.active = false;
        self.fight_id.get_or_insert(event.fight_id);
        self.ended_ms = Some(event.header.monotonic_ms);
        Ok(())
    }

    fn on_damage(&mut self, damage: Damage) -> SdkResult<()> {
        if !(damage.amount.is_finite() && damage.amount > 0.0) {
            return Ok(());
        }
        let timestamp = damage.header.monotonic_ms;
        if !self.active {
            self.begin(None, timestamp);
        }

        if matches!(
            damage.source.relation,
            ActorRelation::LocalPlayer | ActorRelation::GroupMember
        ) {
            if let Some(actor_id) = damage.source.actor_id.as_deref() {
                let member = self.party.as_ref().and_then(|party| party.member(actor_id));
                record_player_damage(&mut self.current_players, actor_id, damage.amount, member);
                if self.is_instanced() {
                    record_player_damage(
                        &mut self.overall_players,
                        actor_id,
                        damage.amount,
                        member,
                    );
                }
            }
        }

        if !damage.source.is_local_player() {
            return Ok(());
        }

        let hits = u64::from(damage.hit_count);
        self.total_damage += damage.amount;
        self.hit_count += hits;
        self.critical_count += u64::from(damage.critical);
        self.kill_count += u64::from(damage.killed);

        let blocked = damage
            .blocked
            .filter(|value| value.is_finite() && *value >= 0.0);
        if let Some(blocked) = blocked {
            self.blocked_reported += blocked;
            self.block_observations += 1;
        }

        let (skill_id, display_name) =
            skill_identity(&damage.skill_id, damage.skill_display_name.as_deref());
        let skill = self.skills.entry(skill_id).or_default();
        skill.display_name.get_or_insert(display_name);
        if skill.icon.is_none() {
            skill.icon = damage.skill_icon;
        }
        skill.damage += damage.amount;
        skill.hits += hits;
        skill.criticals += u64::from(damage.critical);
        skill.kills += u64::from(damage.killed);
        skill.max_hit = skill.max_hit.max(damage.amount);
        if let Some(blocked) = blocked {
            skill.blocked_reported += blocked;
            skill.block_observations += 1;
        }
        Ok(())
    }
}

fn update_player_metadata(totals: &mut BTreeMap<String, PlayerTotal>, member: &PartyMember) {
    if let Some(total) = totals.get_mut(&member.actor_id) {
        if let Some(name) = &member.name {
            total.name = Some(name.clone());
        }
        if let Some(class_id) = &member.class_id {
            total.class_id = Some(class_id.clone());
        }
        if let Some(class_icon) = &member.class_icon {
            total.class_icon = Some(class_icon.clone());
        }
    }
}

fn record_player_damage(
    totals: &mut BTreeMap<String, PlayerTotal>,
    actor_id: &str,
    amount: f64,
    member: Option<&PartyMember>,
) {
    let total = totals.entry(actor_id.to_owned()).or_default();
    if let Some(member) = member {
        if let Some(name) = &member.name {
            total.name = Some(name.clone());
        }
        if let Some(class_id) = &member.class_id {
            total.class_id = Some(class_id.clone());
        }
        if let Some(class_icon) = &member.class_icon {
            total.class_icon = Some(class_icon.clone());
        }
    }
    total.damage += amount;
}

struct DamageMeter {
    state: MeterState,
    class_icons: ClassIcons,
}

impl DamageMeter {
    fn sync_game_state(&mut self, mut party: Option<Party>, instance: Option<Instance>) {
        if let Some(party) = &mut party {
            self.class_icons.apply(party);
        }
        self.state.observe_game_state(party, instance);
    }

    fn ensure_ticking(&mut self, context: &Context) {
        if self.state.active && !self.state.ticking {
            self.state.ticking = true;
            context.timer().schedule(RENDER_INTERVAL);
        }
    }
}

impl Addon for DamageMeter {
    fn activate(context: &mut ActivateContext) -> SdkResult<Self> {
        let visible = context.config().register(&VISIBLE)?;
        for topic in [DYNO_TOPIC, DPS_TOPIC] {
            context
                .bus()
                .subscribe(topic)
                .map_err(|error| format!("failed to subscribe to /{topic} commands: {error:?}"))?;
        }
        context.assets().register_font(
            "noto-sans-regular",
            &[TextStyle::Body, TextStyle::Small],
            NOTO_REGULAR,
        )?;
        context.assets().register_font(
            "noto-sans-bold",
            &[TextStyle::Strong, TextStyle::Heading],
            NOTO_BOLD,
        )?;
        let class_icons = ClassIcons::register(context)?;
        context.log().info("Dyno damage meter activated");
        let game = context.game();
        let mut addon = Self {
            state: MeterState {
                visible,
                ..MeterState::default()
            },
            class_icons,
        };
        addon.sync_game_state(game.party().value, game.instance().value);
        context.render();
        Ok(addon)
    }

    fn on_ui_event(&mut self, context: &mut Context, event: UiEvent) -> SdkResult<()> {
        if let UiEvent::CheckboxChanged { id, checked } = &event {
            if id == VISIBLE.key() {
                context.config().set(&VISIBLE, checked)?;
            }
        }
        if apply_ui_event(&mut self.state, event) {
            context.render();
        }
        Ok(())
    }

    fn on_events_lost(&mut self, context: &mut Context, _loss: EventLoss) -> SdkResult<()> {
        let game = context.game();
        self.sync_game_state(game.party().value, game.instance().value);
        self.state.on_events_lost();
        context.render();
        Ok(())
    }

    fn on_messages(&mut self, context: &mut Context, batch: Messages) -> SdkResult<()> {
        if batch.dropped_before() > 0 {
            let warning = format!(
                "Dropped {} queued damage-meter command(s) before delivery",
                batch.dropped_before()
            );
            context.log().warning(&warning);
            context.chat().error(&warning);
        }

        let mut confirmation = None;
        for message in batch.into_messages() {
            let Some(command) = host_meter_command(&message) else {
                continue;
            };
            match command {
                Ok(command) => {
                    if matches!(command, DpsCommand::Show | DpsCommand::Hide) {
                        let visible = matches!(command, DpsCommand::Show);
                        context.config().set(&VISIBLE, &visible)?;
                    }
                    confirmation = Some(self.state.apply_command(command));
                }
                Err(error) => {
                    let warning = format!("Ignored /{} command: {error}", message.topic);
                    context.log().warning(&warning);
                    context.chat().error(&warning);
                }
            }
        }
        if let Some(confirmation) = confirmation {
            context.render();
            context.chat().print(confirmation);
        }
        Ok(())
    }

    fn on_combat_started(&mut self, context: &mut Context, event: Combat) -> SdkResult<()> {
        let game = context.game();
        self.sync_game_state(game.party().value, game.instance().value);
        self.state.on_combat_started(event)?;
        self.ensure_ticking(context);
        context.render();
        Ok(())
    }

    fn on_combat_ended(&mut self, context: &mut Context, event: Combat) -> SdkResult<()> {
        let game = context.game();
        self.sync_game_state(game.party().value, game.instance().value);
        self.state.on_combat_ended(event)?;
        context.render();
        Ok(())
    }

    fn on_damage(&mut self, context: &mut Context, damage: Damage) -> SdkResult<()> {
        let game = context.game();
        self.sync_game_state(game.party().value, game.instance().value);
        self.state.on_damage(damage)?;
        self.ensure_ticking(context);
        Ok(())
    }

    fn on_party_changed(&mut self, context: &mut Context, _event: PartyChanged) -> SdkResult<()> {
        let game = context.game();
        self.sync_game_state(game.party().value, game.instance().value);
        context.render();
        Ok(())
    }

    fn on_instance_changed(
        &mut self,
        context: &mut Context,
        event: InstanceChanged,
    ) -> SdkResult<()> {
        self.state.observe_instance_edge(event.current);
        self.state.observe_party(context.game().party().value);
        context.render();
        Ok(())
    }

    fn on_window_opened(&mut self, context: &mut Context, _event: WindowChanged) -> SdkResult<()> {
        context.render();
        Ok(())
    }

    fn on_window_closed(&mut self, context: &mut Context, _event: WindowChanged) -> SdkResult<()> {
        context.render();
        Ok(())
    }

    fn on_tick(&mut self, context: &mut Context, _tick: Tick) -> SdkResult<TickControl> {
        if self.state.active {
            context.render();
        }
        self.state.ticking = self.state.active;
        Ok(if self.state.ticking {
            TickControl::Continue
        } else {
            TickControl::Stop
        })
    }

    fn view(&mut self, context: &ViewContext) -> SdkResult<Frame> {
        let game = context.game();
        let snapshot = game.snapshot();
        self.sync_game_state(game.party().value, game.instance().value);
        let open_windows = snapshot
            .windows
            .value
            .as_ref()
            .map(|windows| windows.open.as_slice())
            .unwrap_or_default();
        Ok(render_meter(
            &self.state,
            snapshot.observation.captured_at_ms,
            open_windows,
        ))
    }

    fn deactivate(&mut self, _reason: ShutdownReason) {
        self.state = MeterState::default();
    }
}

fn apply_ui_event(meter: &mut MeterState, event: UiEvent) -> bool {
    match event {
        UiEvent::CheckboxChanged { id, checked } if id == VISIBLE.key() => {
            meter.visible = checked;
        }
        _ => return false,
    }
    true
}

fn render_meter(meter: &MeterState, now_ms: u64, open_windows: &[String]) -> Frame {
    let mut frame = FrameBuilder::new();
    frame.config_menu("settings", "Dyno", |ui| {
        ui.checkbox(VISIBLE.key(), "Enable damage meter", meter.visible, true);
    });

    let group_mode = meter.is_group_instance();
    let group_overall = group_mode && !meter.active;
    let has_visible_segment = if group_overall {
        !meter.overall_players.is_empty()
    } else {
        meter.started_ms.is_some()
    };
    if !has_visible_segment || !meter.visible || should_hide_for_game_ui(open_windows) {
        return frame.finish();
    }

    let elapsed = if group_mode {
        meter.player_elapsed_seconds(group_overall, now_ms)
    } else {
        meter.elapsed_seconds(now_ms)
    };
    let displayed_damage = if group_mode {
        meter.player_total_damage(group_overall)
    } else {
        meter.total_damage
    };
    let encounter_dps = displayed_damage / elapsed;
    let max_skill_damage = meter.max_skill_damage().max(1.0);
    let skills = meter.sorted_skills();
    let players = meter.sorted_players(group_overall);
    let max_player_damage = players
        .iter()
        .map(|(_, player)| player.damage)
        .fold(0.0, f64::max)
        .max(1.0);

    frame.surface(
        "main",
        "Damage",
        SurfaceOptions::new(Anchor::BottomRight)
            .margin(METER_RIGHT_MARGIN, METER_BOTTOM_MARGIN)
            .width(METER_WIDTH)
            .style(meter_surface_style()),
        |ui| {
            ui.table("meter-title", title_columns(), false, None, |ui| {
                ui.styled_table_row(
                    "meter-title-row",
                    RowKind::Body,
                    27.0,
                    Some(TITLE_BACKGROUND),
                    None,
                    |ui| {
                        ui.table_cell("meter-title-left", 0, |ui| {
                            ui.row("meter-title-identity", Some(5.0), |ui| {
                                ui.canvas("combat-state", [10.0, 16.0], |canvas| {
                                    canvas.circle(
                                        [5.0, 8.0],
                                        3.5,
                                        Some(if meter.active {
                                            ACTIVE_COLOR
                                        } else {
                                            IDLE_COLOR
                                        }),
                                        None,
                                    );
                                });
                                ui.text(
                                    "meter-title-label",
                                    format!(
                                        "[{}] Damage: {}",
                                        format_duration(elapsed),
                                        meter_label(meter, group_mode)
                                    ),
                                    Text::new(TextStyle::Strong).color(TEXT_INK).no_wrap(),
                                );
                            });
                        });
                        ui.table_cell("meter-title-summary", 1, |ui| {
                            ui.text(
                                "meter-title-summary-value",
                                format!("{} DPS", compact_number(encounter_dps)),
                                Text::new(TextStyle::Body).color(TEXT_INK).no_wrap(),
                            );
                        });
                    },
                );
            });
            if meter.event_gap {
                ui.text(
                    "event-gap",
                    "Event gap detected; this encounter may be incomplete",
                    Text::new(TextStyle::Small).color(WARNING_TEXT),
                );
            }
            if group_mode {
                ui.table("players", player_columns(), false, Some(360.0), |ui| {
                    for (index, (_actor_id, player)) in
                        players.into_iter().take(MAX_VISIBLE_ROWS).enumerate()
                    {
                        let fraction = (player.damage / max_player_damage).clamp(0.0, 1.0) as f32;
                        let percent = player.damage / displayed_damage.max(1.0) * 100.0;
                        ui.styled_table_row(
                            format!("player-row-{index}"),
                            RowKind::Body,
                            SKILL_ROW_HEIGHT,
                            Some(if index % 2 == 0 {
                                ROW_BACKGROUND
                            } else {
                                ROW_BACKGROUND_ALT
                            }),
                            Some(
                                RowProgress::new(
                                    fraction,
                                    player_bar_color(player.class_id.as_deref()),
                                )
                                .starting_at(1),
                            ),
                            |ui| {
                                ui.table_cell(format!("player-icon-cell-{index}"), 0, |ui| {
                                    if let Some(icon) = &player.class_icon {
                                        ui.image(
                                            format!("player-icon-{index}"),
                                            icon.clone(),
                                            [SKILL_ICON_SIZE, SKILL_ICON_SIZE],
                                            None,
                                        );
                                    }
                                });
                                ui.table_cell(format!("player-name-cell-{index}"), 1, |ui| {
                                    ui.text(
                                        format!("player-name-{index}"),
                                        format!("{}. {}", index + 1, player_display_name(player)),
                                        Text::new(TextStyle::Body).color(TEXT_INK).no_wrap(),
                                    );
                                });
                                table_text_cell(
                                    ui,
                                    format!("player-value-{index}"),
                                    2,
                                    format!(
                                        "{} ({}, {percent:.1}%)",
                                        compact_number(player.damage),
                                        compact_number(player.damage / elapsed)
                                    ),
                                );
                            },
                        );
                    }
                });
            } else {
                ui.table("skills", skill_columns(), false, Some(360.0), |ui| {
                    for (index, (id, skill)) in
                        skills.into_iter().take(MAX_VISIBLE_ROWS).enumerate()
                    {
                        let fraction = (skill.damage / max_skill_damage).clamp(0.0, 1.0) as f32;
                        let percent = skill.damage / meter.total_damage.max(1.0) * 100.0;
                        ui.styled_table_row(
                            format!("skill-row-{index}"),
                            RowKind::Body,
                            SKILL_ROW_HEIGHT,
                            Some(if index % 2 == 0 {
                                ROW_BACKGROUND
                            } else {
                                ROW_BACKGROUND_ALT
                            }),
                            Some(RowProgress::new(fraction, skill_bar_color(id)).starting_at(1)),
                            |ui| {
                                ui.table_cell(format!("skill-icon-cell-{index}"), 0, |ui| {
                                    if let Some(icon) = &skill.icon {
                                        ui.image(
                                            format!("skill-icon-{index}"),
                                            icon.clone(),
                                            [SKILL_ICON_SIZE, SKILL_ICON_SIZE],
                                            None,
                                        );
                                    }
                                });
                                ui.table_cell(format!("skill-name-cell-{index}"), 1, |ui| {
                                    ui.text(
                                        format!("skill-name-{index}"),
                                        skill
                                            .display_name
                                            .clone()
                                            .unwrap_or_else(|| readable_skill_id(id)),
                                        Text::new(TextStyle::Body).color(TEXT_INK).no_wrap(),
                                    );
                                });
                                table_text_cell(
                                    ui,
                                    format!("skill-value-{index}"),
                                    2,
                                    format!("{} ({percent:.1}%)", compact_number(skill.damage)),
                                );
                            },
                        );
                    }
                });
            }
        },
    );
    frame.finish()
}

fn should_hide_for_game_ui(open_windows: &[String]) -> bool {
    // Farever's active window registry is empty during normal HUD-only play.
    // Treat every registered game window as modal for this compact HUD surface
    // so new shops, crafting screens, dialogs, and menus hide automatically.
    !open_windows.is_empty()
}

fn meter_surface_style() -> SurfaceStyle {
    SurfaceStyle::new(SURFACE_FILL)
        .stroke(Stroke::new(1.5, SURFACE_STROKE))
        .corner_radius(5.0)
}

fn title_columns() -> Vec<Column> {
    vec![
        Column::flex().align(Alignment::Left).padding(6.0),
        Column::pixels(100.0).align(Alignment::Right).padding(6.0),
    ]
}

fn skill_columns() -> Vec<Column> {
    vec![
        Column::pixels(SKILL_ICON_SIZE).align(Alignment::Center),
        Column::flex().align(Alignment::Left).padding(6.0),
        Column::pixels(150.0).align(Alignment::Right).padding(6.0),
    ]
}

fn player_columns() -> Vec<Column> {
    vec![
        Column::pixels(SKILL_ICON_SIZE).align(Alignment::Center),
        Column::flex().align(Alignment::Left).padding(6.0),
        Column::pixels(190.0).align(Alignment::Right).padding(6.0),
    ]
}

fn table_text_cell(
    ui: &mut ui::SurfaceBuilder<'_>,
    id: impl Into<String>,
    column: u32,
    text: impl Into<String>,
) {
    let id = id.into();
    ui.table_cell(format!("{id}-cell"), column, |ui| {
        ui.text(
            id,
            text,
            Text::new(TextStyle::Body).color(TEXT_INK).no_wrap(),
        );
    });
}

fn parse_meter_command(payload: &[u8]) -> Result<DpsCommand, String> {
    let input =
        std::str::from_utf8(payload).map_err(|_| "the payload is not valid UTF-8".to_owned())?;
    let mut arguments = input.split_whitespace();
    let command = arguments
        .next()
        .ok_or_else(|| "expected `clear`, `reset`, `show`, or `hide`".to_owned())?;
    if arguments.next().is_some() {
        return Err(format!("`{command}` does not accept arguments"));
    }
    match command {
        "clear" | "reset" => Ok(DpsCommand::Reset),
        "show" => Ok(DpsCommand::Show),
        "hide" => Ok(DpsCommand::Hide),
        _ => Err("expected `clear`, `reset`, `show`, or `hide`".to_owned()),
    }
}

fn host_meter_command(message: &Message) -> Option<Result<DpsCommand, String>> {
    ((message.topic == DYNO_TOPIC || message.topic == DPS_TOPIC)
        && message.source_addon_id == HOST_MESSAGE_SOURCE_ID)
        .then(|| parse_meter_command(&message.payload))
}

fn compact_number(value: f64) -> String {
    let value = if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    };
    if value >= 1_000_000_000.0 {
        format!("{:.1}B", value / 1_000_000_000.0)
    } else if value >= 1_000_000.0 {
        format!("{:.1}M", value / 1_000_000.0)
    } else if value >= 1_000.0 {
        format!("{:.1}K", value / 1_000.0)
    } else {
        format!("{value:.0}")
    }
}

fn format_duration(seconds: f64) -> String {
    let seconds = seconds.max(0.0).floor() as u64;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

fn meter_label(meter: &MeterState, group_mode: bool) -> &'static str {
    if meter.active {
        "Current fight"
    } else if group_mode && !meter.overall_players.is_empty() {
        "Overall"
    } else if meter.total_damage > 0.0 {
        "Last fight"
    } else {
        "Ready"
    }
}

fn player_display_name(player: &PlayerTotal) -> String {
    player
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .or_else(|| {
            player
                .class_id
                .as_deref()
                .map(str::trim)
                .filter(|class_id| !class_id.is_empty())
        })
        .unwrap_or("Unknown player")
        .to_owned()
}

fn player_bar_color(class_id: Option<&str>) -> Color {
    match class_id {
        Some("Mage") => Color::rgba8(58, 121, 143, 174),
        Some("Priest") => Color::rgba8(152, 121, 77, 168),
        Some("Rogue") => Color::rgba8(75, 72, 89, 168),
        Some("Warrior") => Color::rgba8(188, 87, 73, 174),
        _ => Color::rgba(0.58, 0.42, 0.67, 0.62),
    }
}

fn skill_bar_color(skill_id: &str) -> Color {
    match skill_id.split_once('_').map(|(class, _)| class) {
        Some("Mage") => Color::rgba8(58, 121, 143, 174),
        Some("Priest") => Color::rgba8(152, 121, 77, 168),
        Some("Rogue") => Color::rgba8(75, 72, 89, 168),
        Some("Warrior") => Color::rgba8(188, 87, 73, 174),
        _ => Color::rgba(0.58, 0.42, 0.67, 0.62),
    }
}

fn skill_identity(skill_id: &str, public_name: Option<&str>) -> (String, String) {
    if is_base_attack(skill_id) {
        return ("BaseAttack".to_owned(), "Attack".to_owned());
    }
    let display_name = public_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map_or_else(|| readable_skill_id(skill_id), str::to_owned);
    (skill_id.to_owned(), display_name)
}

fn is_base_attack(skill_id: &str) -> bool {
    let without_stage = skill_id.trim_end_matches(|character: char| character.is_ascii_digit());
    without_stage
        .strip_suffix("_Base_Attack")
        .is_some_and(|weapon| !weapon.is_empty())
}

fn readable_skill_id(skill_id: &str) -> String {
    let raw_name = skill_id
        .split_once('_')
        .filter(|(prefix, _)| matches!(*prefix, "Mage" | "Priest" | "Rogue" | "Warrior"))
        .map_or(skill_id, |(_, name)| name);
    let mut output = String::with_capacity(raw_name.len() + 8);
    let mut previous_is_lower_or_digit = false;
    for character in raw_name.chars() {
        if character == '_' || character == '-' {
            if !output.ends_with(' ') && !output.is_empty() {
                output.push(' ');
            }
            previous_is_lower_or_digit = false;
            continue;
        }
        if character.is_uppercase() && previous_is_lower_or_digit && !output.ends_with(' ') {
            output.push(' ');
        }
        previous_is_lower_or_digit = character.is_lowercase() || character.is_ascii_digit();
        output.push(character);
    }
    if output.is_empty() {
        skill_id.to_owned()
    } else {
        output
    }
}

farever_more_sdk::export!(DamageMeter);

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn header(sequence: u64, monotonic_ms: u64) -> EventHeader {
        EventHeader {
            sequence,
            monotonic_ms,
        }
    }

    fn damage(sequence: u64, at_ms: u64, skill_id: &str, amount: f64) -> Damage {
        Damage {
            header: header(sequence, at_ms),
            source: CombatActor {
                actor_id: Some("local-player".to_owned()),
                relation: ActorRelation::LocalPlayer,
                kind: Some("ent.Hero".to_owned()),
            },
            target: CombatActor {
                actor_id: None,
                relation: ActorRelation::Other,
                kind: Some("TrainingDummy".to_owned()),
            },
            skill_id: skill_id.to_owned(),
            skill_display_name: None,
            skill_icon: None,
            amount,
            hit_count: 1,
            critical: false,
            killed: false,
            blocked: Some(0.0),
        }
    }

    fn group_party() -> Party {
        Party {
            party_id: Some("party-1".to_owned()),
            members: vec![
                PartyMember {
                    actor_id: "local-player".to_owned(),
                    is_local: true,
                    name: Some("Local Hero".to_owned()),
                    class_id: Some("Warrior".to_owned()),
                    class_icon: Some(Image {
                        id: "game/class-icon/warrior".to_owned(),
                    }),
                    in_combat: Some(true),
                },
                PartyMember {
                    actor_id: "remote-player".to_owned(),
                    is_local: false,
                    name: Some("Remote Hero".to_owned()),
                    class_id: Some("Mage".to_owned()),
                    class_icon: Some(Image {
                        id: "game/class-icon/mage".to_owned(),
                    }),
                    in_combat: Some(true),
                },
            ],
        }
    }

    #[test]
    fn packaged_class_icons_override_transitional_host_icons() {
        let class_icons = ClassIcons {
            by_class: BTreeMap::from([
                (
                    "warrior".to_owned(),
                    Image {
                        id: "addon-image/dyno/class-warrior".to_owned(),
                    },
                ),
                (
                    "mage".to_owned(),
                    Image {
                        id: "addon-image/dyno/class-mage".to_owned(),
                    },
                ),
            ]),
        };
        let mut party = group_party();
        party.members[1].class_id = Some("Class_MAGE".to_owned());

        class_icons.apply(&mut party);

        assert_eq!(
            party.members[0]
                .class_icon
                .as_ref()
                .map(|icon| icon.id.as_str()),
            Some("addon-image/dyno/class-warrior")
        );
        assert_eq!(
            party.members[1]
                .class_icon
                .as_ref()
                .map(|icon| icon.id.as_str()),
            Some("addon-image/dyno/class-mage")
        );
    }

    fn rift(session_id: u64) -> Instance {
        Instance {
            session_id,
            kind: InstanceKind::Other,
            area_id: Some("POI/Rifts/POI_Rift_01".to_owned()),
        }
    }

    fn remote_damage(sequence: u64, at_ms: u64, amount: f64) -> Damage {
        let mut event = damage(sequence, at_ms, "Mage_RayOfSpark", amount);
        event.source = CombatActor {
            actor_id: Some("remote-player".to_owned()),
            relation: ActorRelation::GroupMember,
            kind: Some("ent.Hero".to_owned()),
        };
        event
    }

    #[test]
    fn encounter_boundaries_preserve_the_completed_skill_list() {
        let mut meter = MeterState::default();
        meter
            .on_combat_started(Combat {
                header: header(1, 1_000),
                fight_id: 7,
            })
            .unwrap();
        meter
            .on_damage(damage(2, 2_000, "Mage_RayOfSpark", 300.0))
            .unwrap();
        meter
            .on_combat_ended(Combat {
                header: header(3, 5_000),
                fight_id: 7,
            })
            .unwrap();

        assert!(!meter.active);
        assert_eq!(meter.total_damage, 300.0);
        assert_eq!(meter.elapsed_seconds(99_000), 4.0);
        assert_eq!(meter.sorted_skills()[0].0, "Mage_RayOfSpark");
    }

    #[test]
    fn group_instance_renders_player_rows_without_skill_distribution() {
        let mut meter = MeterState::default();
        meter.observe_game_state(Some(group_party()), Some(rift(1)));
        meter
            .on_combat_started(Combat {
                header: header(1, 1_000),
                fight_id: 7,
            })
            .unwrap();
        meter
            .on_damage(damage(2, 2_000, "Warrior_Attack", 800.0))
            .unwrap();
        meter.on_damage(remote_damage(3, 2_000, 200.0)).unwrap();

        let frame = render_meter(&meter, 3_000, &[]);
        let surface = frame.surface("main").expect("group meter surface");
        assert!(surface.node("players").is_some());
        assert!(surface.node("skills").is_none());
        assert_eq!(
            surface
                .node("meter-title-summary-value")
                .and_then(|node| node.text()),
            Some("500 DPS")
        );
        assert_eq!(
            surface.node("player-name-0").and_then(|node| node.text()),
            Some("1. Local Hero")
        );
        assert_eq!(
            surface.node("player-value-0").and_then(|node| node.text()),
            Some("800 (400, 80.0%)")
        );
        assert_eq!(
            surface.node("player-name-1").and_then(|node| node.text()),
            Some("2. Remote Hero")
        );
        assert_eq!(
            surface.node("player-value-1").and_then(|node| node.text()),
            Some("200 (100, 20.0%)")
        );
        let (icon, size) = surface
            .node("player-icon-0")
            .and_then(|node| node.image())
            .expect("class icon");
        assert_eq!(icon.id, "game/class-icon/warrior");
        assert_eq!(size, [SKILL_ICON_SIZE, SKILL_ROW_HEIGHT]);
        let progress = surface
            .nodes()
            .filter(|node| node.id().starts_with("player-row-"))
            .map(|node| node.row_progress().expect("player progress").fraction)
            .collect::<Vec<_>>();
        assert_eq!(progress, [1.0, 0.25]);
        assert_eq!(
            meter.skills.len(),
            1,
            "remote skills stay out of solo detail"
        );
    }

    #[test]
    fn multi_member_open_world_party_keeps_the_solo_skill_view() {
        let mut meter = MeterState::default();
        meter.observe_game_state(
            Some(group_party()),
            Some(Instance {
                session_id: 1,
                kind: InstanceKind::OpenWorld,
                area_id: Some("World/Town".to_owned()),
            }),
        );
        meter
            .on_damage(damage(1, 1_000, "Warrior_Attack", 400.0))
            .unwrap();

        let frame = render_meter(&meter, 2_000, &[]);
        let surface = frame.surface("main").expect("solo meter surface");
        assert!(surface.node("skills").is_some());
        assert!(surface.node("players").is_none());
    }

    #[test]
    fn group_overall_sums_encounter_damage_and_only_combat_duration() {
        let mut meter = MeterState::default();
        meter.observe_game_state(Some(group_party()), Some(rift(1)));
        meter
            .on_combat_started(Combat {
                header: header(1, 1_000),
                fight_id: 7,
            })
            .unwrap();
        meter
            .on_damage(damage(2, 2_000, "Warrior_Attack", 400.0))
            .unwrap();
        meter
            .on_combat_ended(Combat {
                header: header(3, 5_000),
                fight_id: 7,
            })
            .unwrap();

        meter
            .on_combat_started(Combat {
                header: header(4, 10_000),
                fight_id: 8,
            })
            .unwrap();
        meter.on_damage(remote_damage(5, 11_000, 600.0)).unwrap();
        meter
            .on_combat_ended(Combat {
                header: header(6, 12_000),
                fight_id: 8,
            })
            .unwrap();

        assert_eq!(meter.overall_elapsed_ms, 6_000);
        let frame = render_meter(&meter, 99_000, &[]);
        let surface = frame.surface("main").expect("overall group meter");
        assert_eq!(
            surface
                .node("meter-title-label")
                .and_then(|node| node.text()),
            Some("[0:06] Damage: Overall")
        );
        assert_eq!(
            surface
                .node("meter-title-summary-value")
                .and_then(|node| node.text()),
            Some("167 DPS")
        );
        assert_eq!(
            surface.node("player-value-0").and_then(|node| node.text()),
            Some("600 (100, 60.0%)")
        );
        assert_eq!(
            surface.node("player-value-1").and_then(|node| node.text()),
            Some("400 (67, 40.0%)")
        );
    }

    #[test]
    fn instance_transition_clears_group_segments_and_hides_until_combat() {
        let mut meter = MeterState::default();
        meter.observe_game_state(Some(group_party()), Some(rift(1)));
        meter
            .on_damage(damage(1, 1_000, "Warrior_Attack", 400.0))
            .unwrap();
        assert_eq!(meter.overall_players.len(), 1);

        meter.observe_instance(Some(rift(2)));

        assert!(meter.overall_players.is_empty());
        assert!(meter.current_players.is_empty());
        assert_eq!(meter.started_ms, None);
        assert_eq!(render_meter(&meter, 2_000, &[]).surface_count(), 0);
    }

    #[test]
    fn first_late_instance_snapshot_does_not_discard_an_active_encounter() {
        let mut meter = MeterState::default();
        meter
            .on_damage(damage(1, 1_000, "Warrior_Attack", 400.0))
            .unwrap();

        meter.observe_instance(Some(Instance {
            session_id: 1,
            kind: InstanceKind::OpenWorld,
            area_id: Some("World/Field".to_owned()),
        }));

        assert!(meter.active);
        assert_eq!(meter.total_damage, 400.0);
        assert_eq!(meter.skills.len(), 1);
    }

    #[test]
    fn resetting_a_completed_group_encounter_removes_its_damage_and_duration() {
        let mut meter = MeterState::default();
        meter.observe_game_state(Some(group_party()), Some(rift(1)));
        meter
            .on_combat_started(Combat {
                header: header(1, 1_000),
                fight_id: 7,
            })
            .unwrap();
        meter
            .on_damage(damage(2, 2_000, "Warrior_Attack", 400.0))
            .unwrap();
        meter
            .on_combat_ended(Combat {
                header: header(3, 5_000),
                fight_id: 7,
            })
            .unwrap();

        meter.reset_encounter();

        assert!(meter.overall_players.is_empty());
        assert_eq!(meter.overall_elapsed_ms, 0);
        assert_eq!(render_meter(&meter, 6_000, &[]).surface_count(), 0);
    }

    #[test]
    fn damage_done_excludes_the_reported_blocked_portion() {
        let mut meter = MeterState::default();
        let mut hit = damage(1, 1_000, "Warrior_Attack", 70.0);
        hit.blocked = Some(30.0);
        meter.on_damage(hit).unwrap();

        assert_eq!(meter.total_damage, 70.0);
        assert_eq!(meter.blocked_reported, 30.0);
        assert_eq!(meter.skills["Warrior_Attack"].damage, 70.0);
        assert_eq!(meter.skills["Warrior_Attack"].blocked_reported, 30.0);
    }

    #[test]
    fn bars_scale_against_the_largest_skill() {
        let mut meter = MeterState::default();
        meter
            .on_damage(damage(1, 1_000, "Mage_Primary", 800.0))
            .unwrap();
        meter
            .on_damage(damage(2, 2_000, "Mage_Secondary", 200.0))
            .unwrap();

        let max = meter.max_skill_damage();
        assert_eq!(meter.skills["Mage_Primary"].damage / max, 1.0);
        assert_eq!(meter.skills["Mage_Secondary"].damage / max, 0.25);
    }

    #[test]
    fn skill_rows_are_stable_when_damage_ties() {
        let mut meter = MeterState::default();
        meter
            .on_damage(damage(1, 1_000, "Mage_Zeta", 50.0))
            .unwrap();
        meter
            .on_damage(damage(2, 2_000, "Mage_Alpha", 50.0))
            .unwrap();

        let rows = meter.sorted_skills();
        assert_eq!(rows[0].0, "Mage_Alpha");
        assert_eq!(rows[1].0, "Mage_Zeta");
    }

    #[test]
    fn meter_is_hidden_until_combat_and_has_no_empty_state_row() {
        let mut meter = MeterState::default();
        let initial = render_meter(&meter, 1_000, &[]);
        assert_eq!(initial.surface_count(), 0);
        assert_eq!(initial.config_menu_count(), 1);
        assert_eq!(initial.config_menu("settings").unwrap().id(), "settings");

        meter
            .on_combat_started(Combat {
                header: header(1, 1_000),
                fight_id: 7,
            })
            .unwrap();
        let frame = render_meter(&meter, 2_000, &[]);
        let surface = frame.surface("main").unwrap();
        assert!(surface.node("skills").is_some());
        assert!(!surface.nodes().any(|node| {
            node.id().starts_with("skill-empty") || node.id().starts_with("skill-row-")
        }));
    }

    #[test]
    fn config_visibility_updates_addon_owned_state_immediately() {
        let mut meter = MeterState::default();
        meter
            .on_damage(damage(1, 1_000, "Mage_RayOfSpark", 300.0))
            .unwrap();

        assert!(apply_ui_event(
            &mut meter,
            UiEvent::CheckboxChanged {
                id: VISIBLE.key().to_owned(),
                checked: false,
            },
        ));
        assert!(!meter.visible);
        let hidden = render_meter(&meter, 2_000, &[]);
        assert_eq!(hidden.surface_count(), 0);
        let menu = hidden.config_menu("settings").unwrap();
        let toggle = menu.node(VISIBLE.key()).expect("visibility checkbox");
        assert_eq!(
            toggle.checkbox(),
            Some(("Enable damage meter", false, true))
        );
        assert!(menu.node("reset-encounter").is_none());
    }

    #[test]
    fn config_lifecycle_events_and_foreign_controls_do_not_mutate_state() {
        let mut meter = MeterState::default();

        assert!(!apply_ui_event(
            &mut meter,
            UiEvent::ConfigMenuShown {
                id: "settings".to_owned(),
            },
        ));
        assert!(!apply_ui_event(
            &mut meter,
            UiEvent::CheckboxChanged {
                id: "unknown-setting".to_owned(),
                checked: false,
            },
        ));
        assert!(!apply_ui_event(
            &mut meter,
            UiEvent::ButtonPressed {
                view: View::ConfigMenu,
                view_id: "settings".to_owned(),
                id: "reset-encounter".to_owned(),
            },
        ));
        assert!(meter.visible);
    }

    #[test]
    fn dps_commands_reset_and_toggle_visibility_without_losing_the_other_setting() {
        let mut meter = MeterState::default();
        meter
            .on_damage(damage(1, 1_000, "Mage_RayOfSpark", 300.0))
            .unwrap();

        assert_eq!(meter.apply_command(DpsCommand::Hide), "Damage meter hidden");
        assert!(!meter.visible);
        assert_eq!(meter.total_damage, 300.0);

        assert_eq!(meter.apply_command(DpsCommand::Show), "Damage meter shown");
        assert!(meter.visible);
        assert_eq!(meter.total_damage, 300.0);

        assert_eq!(meter.apply_command(DpsCommand::Reset), "Damage meter reset");
        assert!(meter.visible);
        assert_eq!(meter.started_ms, None);
        assert_eq!(meter.total_damage, 0.0);
    }

    #[test]
    fn meter_command_parser_accepts_clear_reset_show_and_hide() {
        assert_eq!(parse_meter_command(b"clear"), Ok(DpsCommand::Reset));
        assert_eq!(parse_meter_command(b"reset"), Ok(DpsCommand::Reset));
        assert_eq!(parse_meter_command(b" show "), Ok(DpsCommand::Show));
        assert_eq!(parse_meter_command(b"hide"), Ok(DpsCommand::Hide));
        assert!(parse_meter_command(b"").is_err());
        assert!(parse_meter_command(b"show now").is_err());
        assert!(parse_meter_command(b"SHOW").is_err());
        assert!(parse_meter_command(&[0xFF]).is_err());
    }

    #[test]
    fn only_host_originated_meter_messages_are_commands() {
        let message = |topic: &str, source: &str| Message {
            id: 1,
            monotonic_ms: 1_000,
            source_addon_id: source.to_owned(),
            topic: topic.to_owned(),
            correlation_id: None,
            payload: b"hide".to_vec(),
        };

        assert_eq!(
            host_meter_command(&message(DYNO_TOPIC, HOST_MESSAGE_SOURCE_ID)),
            Some(Ok(DpsCommand::Hide))
        );
        assert_eq!(
            host_meter_command(&message(DPS_TOPIC, HOST_MESSAGE_SOURCE_ID)),
            Some(Ok(DpsCommand::Hide))
        );
        assert_eq!(
            host_meter_command(&message("other", HOST_MESSAGE_SOURCE_ID)),
            None
        );
        assert_eq!(
            host_meter_command(&message(DYNO_TOPIC, "other-addon")),
            None
        );
    }

    #[test]
    fn active_game_windows_hide_the_meter_until_the_registry_is_empty() {
        let mut meter = MeterState::default();
        meter
            .on_damage(damage(1, 1_000, "Mage_RayOfSpark", 300.0))
            .unwrap();
        assert!(render_meter(&meter, 2_000, &[]).surface_count() > 0);

        for window_id in [
            "ui.win.CharacterUI",
            "ui.win.InventoryUI",
            "ui.win.MerchantUI",
            "ui.win.CraftUI",
            "ui.win.BankWindow",
            "ui.win.Scrap",
            "ui.win.EscapeMenu",
            "ui.win.BaseDialog",
            "ui.win.DialogBar",
            "ui.win.element.InstanceSelectScreen",
            "ui.win.LoadingScreen",
            "ui.win.FutureModal",
        ] {
            assert!(
                render_meter(&meter, 2_000, &[window_id.to_owned()]).surface_count() == 0,
                "{window_id} should suppress the HUD meter"
            );
        }

        assert!(render_meter(&meter, 2_000, &[]).surface_count() > 0);
    }

    #[test]
    fn ui_window_edges_request_visibility_reconciliation() {
        let mut addon = DamageMeter {
            state: MeterState::default(),
            class_icons: ClassIcons::default(),
        };
        let mut opened = Context::default();
        addon
            .on_window_opened(
                &mut opened,
                WindowChanged {
                    header: header(1, 1_000),
                    window_id: "ui.win.MerchantUI".to_owned(),
                },
            )
            .unwrap();
        assert!(opened.render_requested());

        let mut closed = Context::default();
        addon
            .on_window_closed(
                &mut closed,
                WindowChanged {
                    header: header(2, 2_000),
                    window_id: "ui.win.MerchantUI".to_owned(),
                },
            )
            .unwrap();
        assert!(closed.render_requested());
    }

    #[test]
    fn rendered_skill_list_uses_the_responsive_table_and_max_skill_scale() {
        let mut meter = MeterState::default();
        let mut primary = damage(1, 1_000, "Mage_Primary", 800.0);
        primary.skill_icon = Some(Image {
            id: "game/skill-icon/fixture".to_owned(),
        });
        meter.on_damage(primary).unwrap();
        meter
            .on_damage(damage(2, 2_000, "Mage_Secondary", 200.0))
            .unwrap();

        let frame = render_meter(&meter, 3_000, &[]);
        let surface = frame.surface("main").expect("meter surface");
        let nodes = surface.nodes().collect::<Vec<_>>();
        let ids = nodes.iter().map(|node| node.id()).collect::<HashSet<_>>();
        assert_eq!(ids.len(), nodes.len());
        assert_eq!(surface.anchor(), Anchor::BottomRight);
        assert_eq!(surface.margin()[1], METER_BOTTOM_MARGIN);
        assert_eq!(surface.width(), Some(METER_WIDTH));
        let style = surface.style().expect("meter style");
        assert!(!style.has_title_bar());
        assert_eq!(style.content_padding(), 0.0);

        let columns = surface
            .node("skills")
            .and_then(|node| node.columns())
            .expect("skill table");
        assert_eq!(columns.len(), 3);
        assert!(matches!(
            columns[0].sizing,
            ColumnSizing::Pixels(width) if width == SKILL_ICON_SIZE
        ));
        assert_eq!(columns[0].padding, None);
        assert!(
            columns
                .iter()
                .all(|column| column.visible_from_width.is_none()),
            "icon, skill, and the combined DPS/percentage value remain aligned and always visible"
        );
        assert!(!ids.contains("skill-header"));
        assert!(ids.contains("meter-title-summary"));
        assert!(ids.contains("skill-icon-cell-0"));

        let (first_icon, first_icon_size) = surface
            .node("skill-icon-0")
            .and_then(|node| node.image())
            .expect("first skill icon");
        assert_eq!(first_icon.id, "game/skill-icon/fixture");
        assert_eq!(first_icon_size, [SKILL_ICON_SIZE, SKILL_ROW_HEIGHT]);

        assert_eq!(
            surface
                .node("skill-row-0")
                .and_then(|node| node.row_height()),
            Some(SKILL_ROW_HEIGHT)
        );
        assert_eq!(
            surface
                .node("meter-title-summary-value")
                .and_then(|node| node.text()),
            Some("500 DPS")
        );
        assert_eq!(
            surface.node("skill-name-0").and_then(|node| node.text()),
            Some("Primary")
        );
        assert_eq!(
            surface.node("skill-value-0").and_then(|node| node.text()),
            Some("800 (80.0%)")
        );

        let progress_tracks = surface
            .nodes()
            .filter(|node| node.id().starts_with("skill-row-"))
            .map(|node| {
                let progress = node.row_progress().expect("skill row progress");
                (progress.fraction, progress.start_column)
            })
            .collect::<Vec<_>>();
        assert_eq!(progress_tracks, [(1.0, Some(1)), (0.25, Some(1))]);
    }

    #[test]
    fn base_attack_stages_share_one_row_but_combo_remains_distinct() {
        let mut meter = MeterState::default();
        meter
            .on_damage(damage(1, 1_000, "GS_Base_Attack", 100.0))
            .unwrap();
        meter
            .on_damage(damage(2, 1_100, "GS_Base_Attack2", 150.0))
            .unwrap();
        meter
            .on_damage(damage(3, 1_200, "GS_Base_Attack3", 200.0))
            .unwrap();
        let mut combo = damage(4, 1_300, "GS_Nova_Combo", 300.0);
        combo.skill_display_name = Some("Mania".to_owned());
        meter.on_damage(combo).unwrap();

        assert_eq!(meter.skills.len(), 2);
        assert_eq!(meter.skills["BaseAttack"].damage, 450.0);
        assert_eq!(
            meter.skills["BaseAttack"].display_name.as_deref(),
            Some("Attack")
        );
        assert_eq!(
            meter.skills["GS_Nova_Combo"].display_name.as_deref(),
            Some("Mania")
        );
    }

    #[test]
    fn prefers_public_names_and_formats_unknown_internal_ids() {
        assert_eq!(skill_identity("GS_Nova_Combo", Some("Mania")).1, "Mania");
        assert_eq!(readable_skill_id("Mage_RayOfSpark"), "Ray Of Spark");
        assert_eq!(readable_skill_id("basic_attack"), "basic attack");
    }
}

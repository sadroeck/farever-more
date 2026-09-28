mod arrow_mesh;

use arrow_mesh::{Material, TRIANGLES, VERTICES};
use farever_more_sdk::prelude::*;
use std::collections::VecDeque;
use std::f32::consts::{PI, TAU};

/// The arrow's command topic. The host forwards the player's own `/gps`
/// commands here; other add-ons publish waypoint requests on the same topic and
/// are told apart by their source, so the player's `map-clicks` setting gates
/// peer requests without weakening commands.
const GPS_TOPIC: &str = "gps";
/// Native full-map observations use their own topic so the map-click setting
/// also gates host-originated clicks without disabling explicit /gps commands.
const MAP_CLICK_TOPIC: &str = "farever.map-click@1";
/// Whether a marker press elsewhere opens an arrow here.
const MAP_CLICKS: Setting<bool> = Setting::boolean("map-clicks", true)
    .label("Arrows from the map")
    .description("Open an arrow to a target clicked on the full map or minimap.");
const HOST_MESSAGE_SOURCE_ID: &str = "farever.host";
const DEFAULT_TARGET_NAME: &str = "Waypoint";
const RENDER_INTERVAL: Duration = Duration::from_millis(50);
const NOTO_REGULAR: &[u8] = include_bytes!("../../dyno/assets/fonts/NotoSans-Regular.ttf");
const NOTO_BOLD: &[u8] = include_bytes!("../../dyno/assets/fonts/NotoSans-Bold.ttf");
const SURFACE_WIDTH: f32 = 176.0;
const ARROW_CANVAS_HEIGHT: f32 = 104.0;
const SURFACE_TOP_MARGIN: f32 = 52.0;
const CENTER: [f32; 2] = [SURFACE_WIDTH * 0.5, ARROW_CANVAS_HEIGHT * 0.5];
const ARRIVAL_DISTANCE_METERS: f32 = 3.0;
const ARRIVAL_DISTANCE_SQUARED: f32 = ARRIVAL_DISTANCE_METERS * ARRIVAL_DISTANCE_METERS;
const MESH_SCALE: f32 = 38.0;
const CAMERA_DISTANCE: f32 = 4.0;
const VIEW_SIN: f32 = 0.573_576_45;
const VIEW_COS: f32 = 0.819_152;
const MAX_MESH_PITCH: f32 = PI * 0.25;
const SHADOW_OFFSET: [f32; 2] = [5.0, 5.0];
const ARROW_BOTTOM_PADDING: f32 = 1.0;
const STACK_SPACING: f32 = 2.0;
const VIEW_TO_CAMERA: [f32; 3] = [0.0, -VIEW_SIN, VIEW_COS];
const LIGHT_DIRECTION: [f32; 3] = [-0.42, -0.34, 0.84];

const ARROW_TOP_LIGHT: Color = Color::rgba(1.0, 0.88, 0.34, 1.0);
const ARROW_SIDE_LIGHT: Color = Color::rgba(0.9, 0.54, 0.06, 1.0);
const ARROW_SIDE_DARK: Color = Color::rgba(0.4, 0.2, 0.02, 1.0);
const ARROW_UNDERSIDE: Color = Color::rgba(0.24, 0.11, 0.01, 1.0);
const SHADOW: Color = Color::rgba(0.0, 0.0, 0.0, 0.32);
const PRIMARY_TEXT: Color = Color::rgba(0.96, 0.97, 1.0, 1.0);
const TEXT_OUTLINE: Stroke = Stroke::new(1.0, Color::rgba(0.04, 0.04, 0.035, 0.95));

struct WayfinderArrow {
    state: WayfinderState,
}

type GameState = GameSnapshot;

impl Addon for WayfinderArrow {
    fn activate(context: &mut ActivateContext) -> SdkResult<Self> {
        context
            .bus()
            .subscribe(GPS_TOPIC)
            .map_err(|error| format!("failed to subscribe to /gps commands: {error:?}"))?;
        context
            .bus()
            .subscribe(MAP_CLICK_TOPIC)
            .map_err(|error| format!("failed to subscribe to full-map clicks: {error:?}"))?;
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
        context.log().info("GPS arrow activated");
        context.timer().schedule(RENDER_INTERVAL);
        Ok(Self {
            state: WayfinderState {
                map_clicks: context.config().register(&MAP_CLICKS)?,
                ..WayfinderState::default()
            },
        })
    }

    fn on_messages(&mut self, context: &mut Context, batch: Messages) -> SdkResult<()> {
        if batch.dropped_before() > 0 {
            let warning = format!(
                "Dropped {} queued /gps command(s) before delivery",
                batch.dropped_before()
            );
            context.log().warning(&warning);
            context.chat().error(&warning);
        }

        let snapshot = context.game().snapshot();
        let (handled, warnings) = self.state.apply_messages(&snapshot, batch.into_messages());
        if handled {
            if let Some(target) = self.state.targets.front() {
                context
                    .log()
                    .info(&format!("Waypoint set: {}", target.name));
            }
            context.replace_ui(self.state.render_after_command(&snapshot));
        }
        for warning in warnings {
            context.log().warning(&warning);
            context.chat().error(&warning);
        }
        if handled {
            context.chat().print("Wayfinder updated");
        }

        Ok(())
    }

    fn on_ui_event(&mut self, context: &mut Context, event: UiEvent) -> SdkResult<()> {
        if let UiEvent::CheckboxChanged { id, checked } = event {
            if id == MAP_CLICKS.key() {
                context.config().set(&MAP_CLICKS, &checked)?;
                self.state.map_clicks = checked;
            }
        }
        Ok(())
    }

    fn on_tick(&mut self, context: &mut Context, _tick: Tick) -> SdkResult<TickControl> {
        let snapshot = context.game().snapshot();
        if let Some(frame) = self.state.render_if_snapshot_changed(&snapshot) {
            context.replace_ui(frame);
        }
        Ok(TickControl::Continue)
    }

    fn deactivate(&mut self, _reason: ShutdownReason) {
        self.state = WayfinderState::default();
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Direction {
    relative_radians: f32,
    altitude_radians: f32,
    horizontal_distance_meters: f32,
}

#[derive(Clone, Debug)]
struct WaypointTarget {
    position: Vec3,
    name: String,
}

#[derive(Clone, Debug, PartialEq)]
struct WaypointSpec {
    x: f32,
    y: f32,
    name: String,
}

#[derive(Clone, Debug, PartialEq)]
enum GpsCommand {
    Hide,
    Show,
    Next,
    SetSequence(Vec<WaypointSpec>),
}

struct WayfinderState {
    visible: bool,
    /// Honors native full-map clicks and waypoint requests from other add-ons.
    map_clicks: bool,
    process_session: Option<u64>,
    targets: VecDeque<WaypointTarget>,
    last_rendered_snapshot: Option<(u64, u64)>,
}

impl Default for WayfinderState {
    fn default() -> Self {
        Self {
            visible: true,
            map_clicks: true,
            process_session: None,
            targets: VecDeque::new(),
            last_rendered_snapshot: None,
        }
    }
}

impl WayfinderState {
    fn render_if_snapshot_changed(&mut self, snapshot: &GameState) -> Option<Frame> {
        let key = (
            snapshot.observation.process_session,
            snapshot.observation.sequence,
        );
        if self.last_rendered_snapshot == Some(key) {
            return None;
        }
        self.last_rendered_snapshot = Some(key);
        Some(render_snapshot(self, snapshot))
    }

    fn render_after_command(&mut self, snapshot: &GameState) -> Frame {
        self.last_rendered_snapshot = Some((
            snapshot.observation.process_session,
            snapshot.observation.sequence,
        ));
        render_snapshot(self, snapshot)
    }

    fn apply_messages(
        &mut self,
        snapshot: &GameState,
        messages: Vec<Message>,
    ) -> (bool, Vec<String>) {
        let mut handled = false;
        let mut warnings = Vec::new();

        for message in messages {
            let from_host = message.source_addon_id == HOST_MESSAGE_SOURCE_ID;
            let native_click = message.topic == MAP_CLICK_TOPIC;
            if message.topic != GPS_TOPIC && !native_click {
                continue;
            }
            // Only the host can report native clicks. The host also forwards
            // explicit /gps commands, which bypass the map-click preference.
            if native_click && !from_host {
                continue;
            }
            let map_request = native_click || !from_host;
            if map_request && !self.map_clicks {
                warnings
                    .push("Ignored a map click: Arrows from the map is switched off".to_owned());
                continue;
            }

            match parse_gps_command(&message.payload)
                .and_then(|command| {
                    if native_click
                        && !matches!(&command, GpsCommand::SetSequence(points) if points.len() == 1)
                    {
                        return Err("a full-map click must contain one waypoint".to_owned());
                    }
                    Ok(command)
                })
                .and_then(|command| self.apply_command(snapshot, command))
            {
                Ok(()) => {
                    handled = true;
                    // A click asks for an arrow the player can see, so a hidden
                    // surface is worth saying out loud instead of silently
                    // keeping the marker off the screen.
                    if map_request && !self.visible {
                        warnings.push(
                            "Waypoint set while the arrow is hidden; run `/gps show` to display it"
                                .to_owned(),
                        );
                    }
                }
                Err(error) => warnings.push(format!(
                    "Ignored {}: {error}",
                    if !map_request {
                        "/gps command"
                    } else {
                        "waypoint request"
                    }
                )),
            }
        }

        (handled, warnings)
    }

    fn apply_command(&mut self, snapshot: &GameState, command: GpsCommand) -> Result<(), String> {
        self.sync_session(snapshot);
        match command {
            GpsCommand::Hide => self.visible = false,
            GpsCommand::Show => self.visible = true,
            GpsCommand::Next => {
                self.targets.pop_front();
            }
            GpsCommand::SetSequence(waypoints) => {
                if !snapshot.session.in_world {
                    return Err("waypoints can only be set while in the world".to_owned());
                }
                let player_position = snapshot
                    .player
                    .value
                    .as_ref()
                    .and_then(|player| player.position)
                    .ok_or_else(|| "the current player position is unavailable".to_owned())?;
                self.targets = waypoints
                    .into_iter()
                    .map(|waypoint| WaypointTarget {
                        position: Vec3 {
                            x: waypoint.x,
                            y: waypoint.y,
                            z: player_position.z,
                        },
                        name: waypoint.name,
                    })
                    .collect();
            }
        }
        Ok(())
    }

    fn sync_session(&mut self, snapshot: &GameState) {
        if self.process_session != Some(snapshot.session.process_session) {
            self.process_session = Some(snapshot.session.process_session);
            self.targets.clear();
        }

        if !snapshot.session.in_world {
            self.targets.clear();
        }
    }

    fn target_position(&mut self, snapshot: &GameState) -> Option<Vec3> {
        self.sync_session(snapshot);
        self.targets.front().map(|target| target.position)
    }

    fn clear_arrived_targets(&mut self, snapshot: &GameState) -> usize {
        let Some(player_position) = snapshot
            .player
            .value
            .as_ref()
            .and_then(|player| player.position)
        else {
            return 0;
        };

        let mut cleared = 0;
        while let Some(target) = self.targets.front() {
            let dx = target.position.x - player_position.x;
            let dy = target.position.y - player_position.y;
            let arrived =
                dx.is_finite() && dy.is_finite() && dx * dx + dy * dy < ARRIVAL_DISTANCE_SQUARED;
            if !arrived {
                break;
            }
            self.targets.pop_front();
            cleared += 1;
        }
        cleared
    }
}

fn parse_gps_command(payload: &[u8]) -> Result<GpsCommand, String> {
    let input =
        std::str::from_utf8(payload).map_err(|_| "the payload is not valid UTF-8".to_owned())?;
    let mut arguments = input.split_whitespace();
    let first = arguments
        .next()
        .ok_or_else(|| "expected `hide`, `show`, `next`, or a waypoint sequence".to_owned())?;

    match first {
        "hide" => {
            if arguments.next().is_some() {
                return Err("`hide` does not accept arguments".to_owned());
            }
            Ok(GpsCommand::Hide)
        }
        "show" => {
            if arguments.next().is_some() {
                return Err("`show` does not accept arguments".to_owned());
            }
            Ok(GpsCommand::Show)
        }
        "next" => {
            if arguments.next().is_some() {
                return Err("`next` does not accept arguments".to_owned());
            }
            Ok(GpsCommand::Next)
        }
        _ => parse_waypoint_sequence(input).map(GpsCommand::SetSequence),
    }
}

fn parse_waypoint_sequence(input: &str) -> Result<Vec<WaypointSpec>, String> {
    split_waypoint_segments(input)?
        .into_iter()
        .enumerate()
        .map(|(index, segment)| parse_waypoint(segment, index + 1))
        .collect()
}

fn split_waypoint_segments(input: &str) -> Result<Vec<&str>, String> {
    let bytes = input.as_bytes();
    let mut segments = Vec::new();
    let mut start = 0;
    let mut index = 0;
    let mut in_quotes = false;

    while index < bytes.len() {
        match bytes[index] {
            b'"' if in_quotes && bytes.get(index + 1) == Some(&b'"') => index += 2,
            b'"' => {
                in_quotes = !in_quotes;
                index += 1;
            }
            b',' if !in_quotes => {
                let segment = input[start..index].trim();
                if segment.is_empty() {
                    return Err("waypoint entries cannot be empty".to_owned());
                }
                segments.push(segment);
                start = index + 1;
                index += 1;
            }
            _ => index += 1,
        }
    }

    if in_quotes {
        return Err("waypoint name has an unterminated double quote".to_owned());
    }
    let segment = input[start..].trim();
    if segment.is_empty() {
        return Err("waypoint entries cannot be empty".to_owned());
    }
    segments.push(segment);
    Ok(segments)
}

fn parse_waypoint(segment: &str, index: usize) -> Result<WaypointSpec, String> {
    let (x_text, remainder) = take_token(segment).ok_or_else(|| {
        format!("waypoint {index} is missing its x coordinate (use `x y \"name\"`)")
    })?;
    let (y_text, name_text) = take_token(remainder).ok_or_else(|| {
        format!("waypoint {index} is missing its y coordinate (use `x y \"name\"`, commas separate waypoints)")
    })?;
    let x = parse_coordinate(&format!("waypoint {index} x"), x_text)?;
    let y = parse_coordinate(&format!("waypoint {index} y"), y_text)?;
    let name = parse_waypoint_name(name_text.trim(), index)?;
    Ok(WaypointSpec { x, y, name })
}

fn take_token(input: &str) -> Option<(&str, &str)> {
    let input = input.trim_start();
    if input.is_empty() {
        return None;
    }
    let end = input.find(char::is_whitespace).unwrap_or(input.len());
    Some((&input[..end], &input[end..]))
}

fn parse_waypoint_name(input: &str, index: usize) -> Result<String, String> {
    if input.is_empty() {
        return Ok(DEFAULT_TARGET_NAME.to_owned());
    }
    if !input.starts_with('"') {
        if input.contains('"') {
            return Err(format!(
                "waypoint {index} name must place double quotes around the entire name"
            ));
        }
        return Ok(input.to_owned());
    }
    if input.len() < 2 || !input.ends_with('"') {
        return Err(format!(
            "waypoint {index} name has an unterminated double quote"
        ));
    }

    let mut name = String::new();
    let mut characters = input[1..input.len() - 1].chars().peekable();
    while let Some(character) = characters.next() {
        if character != '"' {
            name.push(character);
            continue;
        }
        if characters.next_if_eq(&'"').is_none() {
            return Err(format!(
                "waypoint {index} name contains an unescaped double quote"
            ));
        }
        name.push('"');
    }

    Ok(if name.is_empty() {
        DEFAULT_TARGET_NAME.to_owned()
    } else {
        name
    })
}

fn parse_coordinate(axis: &str, text: &str) -> Result<f32, String> {
    let value = text
        .parse::<f32>()
        .map_err(|_| format!("{axis} coordinate `{text}` is not a number"))?;
    if !value.is_finite() {
        return Err(format!("{axis} coordinate must be finite"));
    }
    Ok(value)
}

fn render_snapshot(state: &mut WayfinderState, snapshot: &GameState) -> Frame {
    state.sync_session(snapshot);
    state.clear_arrived_targets(snapshot);
    let map_clicks = state.map_clicks;
    let mut frame = FrameBuilder::new();
    // The settings page must survive an empty target list, so it is registered
    // before any early return.
    frame.config_menu("settings", "GPS", move |ui| {
        ui.checkbox(MAP_CLICKS.key(), "Arrows from the map", map_clicks, true);
    });
    if !state.visible {
        return frame.finish();
    }

    let Some(direction) = direction_from_snapshot(state, snapshot) else {
        return frame.finish();
    };
    let target_name = state
        .targets
        .front()
        .map(|target| target.name.as_str())
        .unwrap_or(DEFAULT_TARGET_NAME);

    frame.surface(
        "wayfinder-arrow",
        "Wayfinder",
        SurfaceOptions::new(Anchor::TopCenter)
            .margin(0.0, SURFACE_TOP_MARGIN)
            .width(SURFACE_WIDTH)
            .style(SurfaceStyle::new(Color::TRANSPARENT)),
        |ui| {
            ui.vertical("wayfinder-stack", Some(STACK_SPACING), |ui| {
                ui.canvas(
                    "arrow-canvas",
                    [SURFACE_WIDTH, ARROW_CANVAS_HEIGHT],
                    |canvas| draw_arrow(canvas, direction),
                );
                centered_text(ui, "target-name", 20.0, target_name, TextStyle::Strong);
                centered_text(
                    ui,
                    "distance",
                    26.0,
                    format_distance(direction.horizontal_distance_meters),
                    TextStyle::Heading,
                );
            });
        },
    );
    frame.finish()
}

fn centered_text(
    ui: &mut farever_more_sdk::ui::SurfaceBuilder<'_>,
    id: &str,
    row_height: f32,
    value: impl Into<String>,
    style: TextStyle,
) {
    ui.table(
        format!("{id}-table"),
        vec![Column::flex().align(Alignment::Center).padding(0.0)],
        false,
        None,
        |ui| {
            ui.table_row(format!("{id}-row"), RowKind::Body, row_height, |ui| {
                ui.table_cell(format!("{id}-cell"), 0, |ui| {
                    ui.text(
                        format!("{id}-label"),
                        value,
                        Text::new(style)
                            .color(PRIMARY_TEXT)
                            .outline(TEXT_OUTLINE)
                            .no_wrap(),
                    );
                });
            });
        },
    );
}

fn direction_from_snapshot(state: &mut WayfinderState, snapshot: &GameState) -> Option<Direction> {
    let target_position = state.target_position(snapshot)?;
    if !snapshot.session.in_world {
        return None;
    }

    let player = snapshot.player.value.as_ref()?;
    let camera = snapshot.camera.value?;
    let player_position = player.position?;
    let dx = target_position.x - player_position.x;
    let dy = target_position.y - player_position.y;
    let dz = target_position.z - player_position.z;
    if !dx.is_finite() || !dy.is_finite() || !dz.is_finite() || !camera.heading_radians.is_finite()
    {
        return None;
    }

    let world_bearing = dy.atan2(dx);
    let horizontal_distance = (dx * dx + dy * dy).sqrt();
    Some(Direction {
        relative_radians: wrap_signed_radians(world_bearing - camera.heading_radians),
        altitude_radians: dz.atan2(horizontal_distance.max(1.0)),
        horizontal_distance_meters: horizontal_distance,
    })
}

fn wrap_signed_radians(value: f32) -> f32 {
    let wrapped = (value + PI).rem_euclid(TAU) - PI;
    if wrapped == -PI {
        PI
    } else {
        wrapped
    }
}

fn format_distance(horizontal_distance_meters: f32) -> String {
    if horizontal_distance_meters < 10.0 {
        format!("{horizontal_distance_meters:.1}m away")
    } else {
        format!("{horizontal_distance_meters:.0}m away")
    }
}

#[derive(Clone, Debug)]
struct ArrowPath {
    points: Vec<[f32; 2]>,
    fill: Color,
}

fn draw_arrow(canvas: &mut CanvasBuilder<'_>, direction: Direction) {
    for path in arrow_commands(direction.relative_radians, direction.altitude_radians) {
        canvas.path(path.points, true, Some(path.fill), None);
    }
}

fn arrow_commands(relative_radians: f32, altitude_radians: f32) -> Vec<ArrowPath> {
    let transformed = transform_vertices(altitude_radians);
    let mut projected = project_vertices(&transformed, relative_radians);
    align_projected_vertices(&mut projected);
    let mut commands = Vec::with_capacity(TRIANGLES.len() + 7);

    // A slightly offset copy of the underside fan creates one coherent soft
    // silhouette without requiring a concave canvas path.
    for triangle in TRIANGLES
        .iter()
        .filter(|triangle| triangle.material == Material::Underside)
    {
        commands.push(path_command(
            triangle.indices.map(|index| {
                let [x, y] = projected[index].position;
                [x + SHADOW_OFFSET[0], y + SHADOW_OFFSET[1]]
            }),
            SHADOW,
        ));
    }

    let mut visible_triangles = TRIANGLES
        .iter()
        .filter_map(|triangle| {
            let normal = triangle_normal(&transformed, triangle.indices);
            (dot(normal, VIEW_TO_CAMERA) > 0.0001).then(|| ProjectedTriangle {
                points: triangle.indices.map(|index| projected[index].position),
                depth: triangle
                    .indices
                    .iter()
                    .map(|&index| projected[index].depth)
                    .sum::<f32>()
                    / 3.0,
                fill: lit_material_color(triangle.material, normal),
            })
        })
        .collect::<Vec<_>>();
    visible_triangles.sort_by(|left, right| left.depth.total_cmp(&right.depth));

    for triangle in visible_triangles {
        commands.push(path_command(triangle.points, triangle.fill));
    }
    commands
}

#[derive(Clone, Copy, Debug)]
struct ProjectedVertex {
    position: [f32; 2],
    depth: f32,
}

#[derive(Clone, Copy, Debug)]
struct ProjectedTriangle {
    points: [[f32; 2]; 3],
    depth: f32,
    fill: Color,
}

fn transform_vertices(altitude_radians: f32) -> Vec<[f32; 3]> {
    let pitch = altitude_radians.clamp(-MAX_MESH_PITCH, MAX_MESH_PITCH);
    let (sin, cos) = pitch.sin_cos();

    VERTICES
        .iter()
        .map(|&[x, y, z]| [x, y * cos - z * sin, y * sin + z * cos])
        .collect()
}

fn project_vertices(vertices: &[[f32; 3]], relative_radians: f32) -> Vec<ProjectedVertex> {
    let (yaw_sin, yaw_cos) = relative_radians.sin_cos();

    vertices
        .iter()
        .map(|&[x, y, z]| {
            let view_y = y * VIEW_COS + z * VIEW_SIN;
            let depth = z * VIEW_COS - y * VIEW_SIN;
            let perspective = CAMERA_DISTANCE / (CAMERA_DISTANCE - depth);
            let local_x = x * perspective * MESH_SCALE;
            let local_y = -view_y * perspective * MESH_SCALE;
            ProjectedVertex {
                position: [
                    CENTER[0] + local_x * yaw_cos - local_y * yaw_sin,
                    CENTER[1] + local_x * yaw_sin + local_y * yaw_cos,
                ],
                depth,
            }
        })
        .collect()
}

fn align_projected_vertices(vertices: &mut [ProjectedVertex]) {
    let (minimum, maximum) = projected_bounds(vertices);
    let offset_x = CENTER[0] - (minimum[0] + maximum[0]) * 0.5;
    let offset_y = ARROW_CANVAS_HEIGHT - ARROW_BOTTOM_PADDING - SHADOW_OFFSET[1] - maximum[1];

    for vertex in vertices {
        vertex.position[0] += offset_x;
        vertex.position[1] += offset_y;
    }
}

fn projected_bounds(vertices: &[ProjectedVertex]) -> ([f32; 2], [f32; 2]) {
    let mut minimum = [f32::INFINITY; 2];
    let mut maximum = [f32::NEG_INFINITY; 2];
    for vertex in vertices {
        minimum[0] = minimum[0].min(vertex.position[0]);
        minimum[1] = minimum[1].min(vertex.position[1]);
        maximum[0] = maximum[0].max(vertex.position[0]);
        maximum[1] = maximum[1].max(vertex.position[1]);
    }
    (minimum, maximum)
}

fn triangle_normal(vertices: &[[f32; 3]], [a, b, c]: [usize; 3]) -> [f32; 3] {
    let ab = subtract(vertices[b], vertices[a]);
    let ac = subtract(vertices[c], vertices[a]);
    normalize(cross(ab, ac))
}

fn lit_material_color(material: Material, normal: [f32; 3]) -> Color {
    let base = match material {
        Material::Top => ARROW_TOP_LIGHT,
        Material::Bevel => ARROW_SIDE_LIGHT,
        Material::Side => ARROW_SIDE_DARK,
        Material::Underside => ARROW_UNDERSIDE,
    };
    let diffuse = dot(normal, normalize(LIGHT_DIRECTION)).max(0.0);
    let intensity = 0.52 + diffuse * 0.48;
    Color::rgba(
        (base.red * intensity).min(1.0),
        (base.green * intensity).min(1.0),
        (base.blue * intensity).min(1.0),
        base.alpha,
    )
}

fn subtract(left: [f32; 3], right: [f32; 3]) -> [f32; 3] {
    [left[0] - right[0], left[1] - right[1], left[2] - right[2]]
}

fn cross(left: [f32; 3], right: [f32; 3]) -> [f32; 3] {
    [
        left[1] * right[2] - left[2] * right[1],
        left[2] * right[0] - left[0] * right[2],
        left[0] * right[1] - left[1] * right[0],
    ]
}

fn dot(left: [f32; 3], right: [f32; 3]) -> f32 {
    left[0] * right[0] + left[1] * right[1] + left[2] * right[2]
}

fn normalize(vector: [f32; 3]) -> [f32; 3] {
    let length = dot(vector, vector).sqrt();
    if length <= f32::EPSILON {
        return [0.0; 3];
    }
    [vector[0] / length, vector[1] / length, vector[2] / length]
}

fn path_command(points: impl IntoIterator<Item = [f32; 2]>, fill: Color) -> ArrowPath {
    ArrowPath {
        points: points.into_iter().collect(),
        fill,
    }
}

farever_more_sdk::export!(WayfinderArrow);

#[cfg(test)]
mod tests {
    use super::*;

    fn status(available: bool) -> StateStatus {
        StateStatus {
            observed_at_ms: available.then_some(100),
            revision: 1,
            reason: (!available).then_some(UnavailableReason::NotYetObserved),
        }
    }

    fn snapshot_at(x: f32, y: f32, z: f32) -> GameState {
        GameState {
            observation: Observation {
                sequence: 1,
                captured_at_ms: 100,
                process_session: 1,
            },
            session: Session {
                process_session: 1,
                in_world: true,
            },
            player: Snapshot {
                status: status(true),
                value: Some(Player {
                    runtime_id: None,
                    name: None,
                    class_id: None,
                    level: None,
                    position: Some(Vec3 { x, y, z }),
                    heading_radians: None,
                    health: None,
                    max_health: None,
                }),
            },
            camera: Snapshot {
                status: status(true),
                value: Some(Camera {
                    heading_radians: 0.0,
                }),
            },
            windows: Snapshot {
                status: status(true),
                value: Some(Windows {
                    open: Vec::new(),
                    focused: None,
                }),
            },
        }
    }

    #[test]
    fn does_not_create_a_default_target() {
        let mut state = WayfinderState::default();
        let starting = snapshot_at(10.0, 20.0, 30.0);
        assert!(state.target_position(&starting).is_none());
        state
            .apply_command(&starting, GpsCommand::Show)
            .expect("show without target");
        assert!(render_snapshot(&mut state, &starting).surface_count() == 0);

        let moved = snapshot_at(40.0, 50.0, 60.0);
        assert!(state.target_position(&moved).is_none());
        assert!(render_snapshot(&mut state, &moved).surface_count() == 0);
    }

    #[test]
    fn direction_is_relative_to_camera_heading_and_wraps() {
        let mut state = WayfinderState::default();
        let mut moved = snapshot_at(0.0, 0.0, 0.0);
        moved.camera.value.as_mut().unwrap().heading_radians = PI;
        set_target(&mut state, &moved, [0.0, 1.0, 0.0], "North");

        let direction = direction_from_snapshot(&mut state, &moved).expect("direction");

        assert!((direction.relative_radians + PI * 0.5).abs() < 0.0001);
    }

    #[test]
    fn altitude_uses_vertical_delta_over_horizontal_distance() {
        let mut state = WayfinderState::default();
        let snapshot = snapshot_at(0.0, 0.0, 0.0);
        set_target(&mut state, &snapshot, [10.0, 0.0, 10.0], "Above");

        let direction = direction_from_snapshot(&mut state, &snapshot).expect("direction");

        assert!((direction.altitude_radians - PI * 0.25).abs() < 0.0001);
    }

    #[test]
    fn surface_is_hidden_until_a_target_is_set() {
        let mut state = WayfinderState::default();
        let starting = snapshot_at(0.0, 0.0, 0.0);
        assert!(render_snapshot(&mut state, &starting).surface_count() == 0);

        let moved = snapshot_at(-1.0, 0.0, 0.0);
        assert!(render_snapshot(&mut state, &moved).surface_count() == 0);
        state
            .apply_command(
                &moved,
                GpsCommand::SetSequence(vec![waypoint_spec(4.0, 0.0, "Test target")]),
            )
            .expect("set target");
        let frame = render_snapshot(&mut state, &moved);
        assert_eq!(frame.surface_count(), 1);
        let surface = frame.surface("wayfinder-arrow").unwrap();
        assert_eq!(surface.anchor(), Anchor::TopCenter);
        assert_eq!(surface.margin(), [0.0, SURFACE_TOP_MARGIN]);
        let style = surface.style().expect("transparent surface style");
        assert_eq!(style.fill().alpha, 0.0);
        assert!(style.border().is_none());
        let nodes = surface.nodes().collect::<Vec<_>>();
        assert_eq!(nodes[0].id(), "wayfinder-stack");
        assert_eq!(nodes[0].kind(), NodeKind::Container);
        assert_eq!(nodes[0].container_spacing(), Some(STACK_SPACING));
        assert_eq!(nodes[1].id(), "arrow-canvas");
        assert_eq!(nodes[1].parent(), Some("wayfinder-stack"));
        assert_eq!(text_for(&frame, "target-name-label"), Some("Test target"));
        assert_eq!(text_for(&frame, "distance-label"), Some("5.0m away"));
        for id in ["target-name-label", "distance-label"] {
            let text = surface.node(id).expect("wayfinder text widget");
            let outline = text.text_outline().expect("text outline");
            assert_eq!(outline.width, TEXT_OUTLINE.width);
            assert_eq!(outline.color.red, TEXT_OUTLINE.color.red);
            assert_eq!(
                text.text_color().map(|color| color.red),
                Some(PRIMARY_TEXT.red)
            );
        }
    }

    #[test]
    fn target_is_cleared_inside_the_arrival_radius_even_while_hidden() {
        let starting = snapshot_at(0.0, 0.0, 0.0);
        let mut state = WayfinderState::default();
        set_target(&mut state, &starting, [5.0, 0.0, 0.0], "Nearby");
        state
            .apply_command(&starting, GpsCommand::Hide)
            .expect("hide");

        let arrived = snapshot_at(2.01, 0.0, 0.0);
        assert!(render_snapshot(&mut state, &arrived).surface_count() == 0);
        assert!(state.targets.is_empty());
    }

    #[test]
    fn arrival_advances_to_the_next_waypoint() {
        let snapshot = snapshot_at(0.0, 0.0, 0.0);
        let mut state = WayfinderState::default();
        set_targets(
            &mut state,
            &snapshot,
            &[([2.0, 0.0, 0.0], "First"), ([8.0, 0.0, 0.0], "Second")],
        );

        let frame = render_snapshot(&mut state, &snapshot);
        assert_eq!(state.targets.len(), 1);
        assert_target(&state, [8.0, 0.0, 0.0], "Second");
        assert_eq!(text_for(&frame, "target-name-label"), Some("Second"));
    }

    #[test]
    fn target_is_retained_at_the_three_meter_boundary() {
        let snapshot = snapshot_at(0.0, 0.0, 0.0);
        let mut state = WayfinderState::default();
        set_target(&mut state, &snapshot, [3.0, 0.0, 0.0], "Boundary");

        let frame = render_snapshot(&mut state, &snapshot);
        assert_eq!(frame.surface_count(), 1);
        assert_eq!(text_for(&frame, "distance-label"), Some("3.0m away"));
        assert_target(&state, [3.0, 0.0, 0.0], "Boundary");
    }

    #[test]
    fn repeated_tick_on_the_same_snapshot_does_not_rebuild_ui() {
        let mut state = WayfinderState::default();
        let snapshot = snapshot_at(0.0, 0.0, 0.0);

        assert!(state.render_if_snapshot_changed(&snapshot).is_some());
        assert!(state.render_if_snapshot_changed(&snapshot).is_none());

        let mut next = snapshot;
        next.observation.sequence += 1;
        assert!(state.render_if_snapshot_changed(&next).is_some());
    }

    #[test]
    fn unavailable_camera_hides_the_surface_without_discarding_the_target() {
        let mut state = WayfinderState::default();
        let mut unavailable = snapshot_at(-1.0, 0.0, 0.0);
        set_target(&mut state, &unavailable, [5.0, 0.0, 0.0], "Target");
        unavailable.camera.status = status(false);
        unavailable.camera.value = None;
        assert!(render_snapshot(&mut state, &unavailable).surface_count() == 0);

        let recovered = snapshot_at(-1.0, 0.0, 0.0);
        assert!(direction_from_snapshot(&mut state, &recovered).is_some());
    }

    #[test]
    fn aligned_target_draws_an_upward_arrow() {
        let tip = arrow_tip(0.0, 0.0);

        assert!((tip[0] - CENTER[0]).abs() < 0.0001);
        assert!(tip[1] < CENTER[1]);
    }

    #[test]
    fn positive_angles_turn_right_in_screen_space() {
        let right = arrow_tip(PI * 0.5, 0.0);
        assert!(right[0] > CENTER[0]);
        assert!((right[1] - CENTER[1]).abs() < 0.0001);

        let behind = arrow_tip(PI, 0.0);
        assert!((behind[0] - CENTER[0]).abs() < 0.0001);
        assert!(behind[1] > CENTER[1]);

        let left = arrow_tip(-PI * 0.5, 0.0);
        assert!(left[0] < CENTER[0]);
        assert!((left[1] - CENTER[1]).abs() < 0.0001);
    }

    #[test]
    fn altitude_tilts_the_arrow_head_up_or_down() {
        let level = arrow_tip(0.0, 0.0);
        let above = arrow_tip(0.0, PI * 0.25);
        let below = arrow_tip(0.0, -PI * 0.25);

        assert!(above[1] < level[1]);
        assert!(below[1] > level[1]);
    }

    #[test]
    fn projected_mesh_uses_only_filled_faces() {
        let commands = arrow_commands(0.0, 0.0);

        assert!(commands.len() > 15);
        assert!(commands.iter().all(|command| command.points.len() == 3));
    }

    #[test]
    fn projected_mesh_is_centered_over_text_and_bottom_aligned() {
        let expected_bottom = ARROW_CANVAS_HEIGHT - ARROW_BOTTOM_PADDING - SHADOW_OFFSET[1];
        for horizontal in [0.0, PI * 0.25, PI * 0.5, PI, -PI * 0.5] {
            for altitude in [-MAX_MESH_PITCH, 0.0, MAX_MESH_PITCH] {
                let transformed = transform_vertices(altitude);
                let mut projected = project_vertices(&transformed, horizontal);
                align_projected_vertices(&mut projected);
                let (minimum, maximum) = projected_bounds(&projected);

                assert!((((minimum[0] + maximum[0]) * 0.5) - CENTER[0]).abs() < 0.0001);
                assert!((maximum[1] - expected_bottom).abs() < 0.0001);
            }
        }
    }

    #[test]
    fn projected_mesh_stays_inside_the_canvas_at_extreme_angles() {
        for horizontal in [0.0, PI * 0.5, PI, -PI * 0.5] {
            for altitude in [-PI * 0.5, 0.0, PI * 0.5] {
                for path in arrow_commands(horizontal, altitude) {
                    for point in path.points {
                        assert_canvas_point(point);
                    }
                }
            }
        }
    }

    #[test]
    fn mesh_triangle_indices_are_valid_and_materials_are_present() {
        for triangle in TRIANGLES {
            assert!(triangle
                .indices
                .into_iter()
                .all(|index| index < VERTICES.len()));
        }
        for material in [
            Material::Top,
            Material::Bevel,
            Material::Side,
            Material::Underside,
        ] {
            assert!(TRIANGLES
                .iter()
                .any(|triangle| triangle.material == material));
        }
    }

    #[test]
    fn distance_format_keeps_detail_only_at_close_range() {
        assert_eq!(format_distance(0.75), "0.8m away");
        assert_eq!(format_distance(9.94), "9.9m away");
        assert_eq!(format_distance(12.6), "13m away");
        assert_eq!(format_distance(1_234.0), "1234m away");
    }

    #[test]
    fn peer_waypoint_requests_honor_the_map_click_setting() {
        let snapshot = snapshot_at(0.0, 0.0, 0.0);
        let request = |topic: &str, source: &str| Message {
            id: 1,
            monotonic_ms: 1,
            source_addon_id: source.to_owned(),
            topic: topic.to_owned(),
            correlation_id: None,
            payload: b"10 20 \"Obelisk\"".to_vec(),
        };

        // Another add-on's request opens a target while map clicks are on.
        let mut state = WayfinderState::default();
        let (handled, warnings) =
            state.apply_messages(&snapshot, vec![request(GPS_TOPIC, "minimap")]);
        assert!(handled, "{warnings:?}");
        assert_eq!(
            state.targets.front().map(|target| target.name.as_str()),
            Some("Obelisk")
        );

        // With map clicks off the same request is ignored ...
        let mut disabled = WayfinderState {
            map_clicks: false,
            ..WayfinderState::default()
        };
        let (handled, warnings) =
            disabled.apply_messages(&snapshot, vec![request(GPS_TOPIC, "minimap")]);
        assert!(!handled, "{warnings:?}");
        assert!(disabled.targets.is_empty());

        // ... while an explicit /gps command still works.
        let (handled, warnings) =
            disabled.apply_messages(&snapshot, vec![request(GPS_TOPIC, HOST_MESSAGE_SOURCE_ID)]);
        assert!(handled, "{warnings:?}");
        assert_eq!(
            disabled.targets.front().map(|target| target.name.as_str()),
            Some("Obelisk")
        );
    }

    #[test]
    fn full_map_clicks_set_a_waypoint_and_honor_the_setting() {
        let snapshot = snapshot_at(0.0, 0.0, 12.0);
        let request = || message(MAP_CLICK_TOPIC, HOST_MESSAGE_SOURCE_ID, b"120 -45");
        let mut state = WayfinderState::default();
        let (handled, warnings) = state.apply_messages(&snapshot, vec![request()]);
        assert!(handled, "{warnings:?}");
        assert_target(&state, [120.0, -45.0, 12.0], DEFAULT_TARGET_NAME);

        state.map_clicks = false;
        let (handled, warnings) = state.apply_messages(&snapshot, vec![request()]);
        assert!(!handled);
        assert!(warnings[0].contains("Arrows from the map"));
        assert_target(&state, [120.0, -45.0, 12.0], DEFAULT_TARGET_NAME);
        let (handled, warnings) = state.apply_messages(
            &snapshot,
            vec![message(GPS_TOPIC, HOST_MESSAGE_SOURCE_ID, b"30 40 Command")],
        );
        assert!(handled, "{warnings:?}");
        assert_target(&state, [30.0, 40.0, 12.0], "Command");
    }

    #[test]
    fn full_map_clicks_require_the_host_and_one_finite_waypoint() {
        let snapshot = snapshot_at(0.0, 0.0, 0.0);
        let mut state = WayfinderState::default();
        let (handled, warnings) = state.apply_messages(
            &snapshot,
            vec![message(MAP_CLICK_TOPIC, "minimap", b"10 20")],
        );
        assert!(!handled);
        assert!(warnings.is_empty());
        assert!(state.targets.is_empty());
        for payload in [b"hide".as_slice(), b"next", b"10 20, 30 40", b"NaN 20"] {
            let (handled, warnings) = state.apply_messages(
                &snapshot,
                vec![message(MAP_CLICK_TOPIC, HOST_MESSAGE_SOURCE_ID, payload)],
            );
            assert!(!handled, "{payload:?}");
            assert_eq!(warnings.len(), 1);
            assert!(state.targets.is_empty());
            assert!(state.visible);
        }
    }

    #[test]
    fn full_map_clicks_report_hidden_arrows_and_out_of_world_requests() {
        let mut snapshot = snapshot_at(0.0, 0.0, 0.0);
        let request = || message(MAP_CLICK_TOPIC, HOST_MESSAGE_SOURCE_ID, b"120 -45");
        let mut state = WayfinderState {
            visible: false,
            ..WayfinderState::default()
        };
        let (handled, warnings) = state.apply_messages(&snapshot, vec![request()]);
        assert!(handled);
        assert!(warnings[0].contains("/gps show"));
        assert!(!state.visible);
        snapshot.session.in_world = false;
        let (handled, warnings) = state.apply_messages(&snapshot, vec![request()]);
        assert!(!handled);
        assert!(warnings[0].contains("in the world"));
    }

    #[test]
    fn peer_requests_say_why_nothing_appeared() {
        let snapshot = snapshot_at(0.0, 0.0, 0.0);
        let request = |source: &str| Message {
            id: 1,
            monotonic_ms: 1,
            source_addon_id: source.to_owned(),
            topic: GPS_TOPIC.to_owned(),
            correlation_id: None,
            payload: b"10 20 \"Obelisk\"".to_vec(),
        };

        // A click while the arrow is hidden still sets the waypoint, and the
        // player is told how to see it instead of nothing happening.
        let mut hidden = WayfinderState {
            visible: false,
            ..WayfinderState::default()
        };
        let (handled, warnings) = hidden.apply_messages(&snapshot, vec![request("minimap")]);
        assert!(handled, "{warnings:?}");
        assert!(!hidden.visible);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("/gps show"), "{warnings:?}");

        // With the setting off the click is refused out loud, not silently.
        let mut disabled = WayfinderState {
            map_clicks: false,
            ..WayfinderState::default()
        };
        let (handled, warnings) = disabled.apply_messages(&snapshot, vec![request("minimap")]);
        assert!(!handled);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("Arrows from the map"), "{warnings:?}");

        // A host command is never subject to the player's click setting and
        // stays silent when it applies.
        let (handled, warnings) =
            disabled.apply_messages(&snapshot, vec![request(HOST_MESSAGE_SOURCE_ID)]);
        assert!(handled, "{warnings:?}");
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn parses_visibility_and_waypoint_commands() {
        assert_eq!(parse_gps_command(b"hide"), Ok(GpsCommand::Hide));
        assert_eq!(parse_gps_command(b" show "), Ok(GpsCommand::Show));
        assert_eq!(parse_gps_command(b"next"), Ok(GpsCommand::Next));
        assert_eq!(
            parse_gps_command(b"-12.5 42"),
            Ok(GpsCommand::SetSequence(vec![waypoint_spec(
                -12.5,
                42.0,
                DEFAULT_TARGET_NAME,
            )]))
        );
        assert_eq!(
            parse_gps_command(b"10 20 Navelin market"),
            Ok(GpsCommand::SetSequence(vec![waypoint_spec(
                10.0,
                20.0,
                "Navelin market",
            )]))
        );
        assert_eq!(
            parse_gps_command(b"10 20 \"First, stop\", 30 40 \"Second \"\"quoted\"\" stop\""),
            Ok(GpsCommand::SetSequence(vec![
                waypoint_spec(10.0, 20.0, "First, stop"),
                waypoint_spec(30.0, 40.0, "Second \"quoted\" stop"),
            ]))
        );
    }

    #[test]
    fn rejects_malformed_or_non_finite_waypoint_commands() {
        for payload in [
            b"".as_slice(),
            b"hide now".as_slice(),
            b"show now".as_slice(),
            b"next now".as_slice(),
            b"12".as_slice(),
            b"east 12".as_slice(),
            b"NaN 12".as_slice(),
            b"12 inf".as_slice(),
            b"1 2 First,".as_slice(),
            b"1 2 First,, 3 4 Third".as_slice(),
            b"1 2 \"Unterminated".as_slice(),
            b"1 2 \"Quoted\" suffix".as_slice(),
        ] {
            assert!(parse_gps_command(payload).is_err(), "payload: {payload:?}");
        }
    }

    #[test]
    fn hide_and_show_preserve_the_target() {
        let snapshot = snapshot_at(10.0, 20.0, 7.0);
        let mut state = WayfinderState::default();
        state
            .apply_command(
                &snapshot,
                GpsCommand::SetSequence(vec![
                    waypoint_spec(13.0, 24.0, "Camp"),
                    waypoint_spec(30.0, 40.0, "Town"),
                ]),
            )
            .expect("set target");

        state
            .apply_command(&snapshot, GpsCommand::Hide)
            .expect("hide");
        assert!(!state.visible);
        assert!(render_snapshot(&mut state, &snapshot).surface_count() == 0);
        assert_eq!(state.targets.len(), 2);
        assert_target(&state, [13.0, 24.0, 7.0], "Camp");

        state
            .apply_command(&snapshot, GpsCommand::Show)
            .expect("show");
        let frame = render_snapshot(&mut state, &snapshot);
        assert_eq!(frame.surface_count(), 1);
        assert_eq!(text_for(&frame, "target-name-label"), Some("Camp"));
        assert_target(&state, [13.0, 24.0, 7.0], "Camp");
        assert_eq!(state.targets.len(), 2);
    }

    #[test]
    fn next_command_advances_the_waypoint_sequence() {
        let snapshot = snapshot_at(0.0, 0.0, 2.0);
        let mut state = WayfinderState::default();
        state
            .apply_command(
                &snapshot,
                GpsCommand::SetSequence(vec![
                    waypoint_spec(5.0, 0.0, "First"),
                    waypoint_spec(10.0, 0.0, "Second"),
                ]),
            )
            .expect("set sequence");
        state
            .apply_command(&snapshot, GpsCommand::Next)
            .expect("advance sequence");

        let frame = render_snapshot(&mut state, &snapshot);
        assert_eq!(state.targets.len(), 1);
        assert_target(&state, [10.0, 0.0, 2.0], "Second");
        assert_eq!(text_for(&frame, "target-name-label"), Some("Second"));
    }

    #[test]
    fn setting_a_target_while_hidden_does_not_show_it() {
        let snapshot = snapshot_at(1.0, 2.0, 3.0);
        let mut state = WayfinderState::default();
        state
            .apply_command(&snapshot, GpsCommand::Hide)
            .expect("hide");
        state
            .apply_command(
                &snapshot,
                GpsCommand::SetSequence(vec![waypoint_spec(4.0, 6.0, "Hidden target")]),
            )
            .expect("set target");

        assert!(render_snapshot(&mut state, &snapshot).surface_count() == 0);
        assert_target(&state, [4.0, 6.0, 3.0], "Hidden target");
    }

    #[test]
    fn gps_topic_messages_are_routed_by_source() {
        let snapshot = snapshot_at(0.0, 0.0, 0.0);
        let mut state = WayfinderState::default();
        let (handled, warnings) = state.apply_messages(
            &snapshot,
            vec![
                // Another topic is dropped whatever the source.
                message("other", HOST_MESSAGE_SOURCE_ID, b"hide"),
                // The host speaks for the player: applied, and malformed
                // arguments are reported.
                message(GPS_TOPIC, HOST_MESSAGE_SOURCE_ID, b"show extra"),
                message(GPS_TOPIC, HOST_MESSAGE_SOURCE_ID, b"5 6 Test target"),
                // A peer add-on asks for a waypoint on the same topic.
                message(GPS_TOPIC, "minimap", b"7 8 Marker"),
            ],
        );

        assert!(handled);
        assert_eq!(warnings.len(), 1);
        assert!(state.visible);
        // Each request replaces the queue, so the last one is the target.
        assert_target(&state, [7.0, 8.0, 0.0], "Marker");
    }

    fn arrow_tip(relative_radians: f32, altitude_radians: f32) -> [f32; 2] {
        let transformed = transform_vertices(altitude_radians);
        project_vertices(&transformed, relative_radians)[18].position
    }

    fn assert_canvas_point(point: [f32; 2]) {
        assert!((0.0..=SURFACE_WIDTH).contains(&point[0]));
        assert!((0.0..=ARROW_CANVAS_HEIGHT).contains(&point[1]));
    }

    fn assert_target(state: &WayfinderState, expected: [f32; 3], name: &str) {
        let target = state.targets.front().expect("target");
        assert_eq!(
            [target.position.x, target.position.y, target.position.z],
            expected
        );
        assert_eq!(target.name, name);
    }

    fn set_target(
        state: &mut WayfinderState,
        snapshot: &GameState,
        position: [f32; 3],
        name: &str,
    ) {
        set_targets(state, snapshot, &[(position, name)]);
    }

    fn set_targets(state: &mut WayfinderState, snapshot: &GameState, targets: &[([f32; 3], &str)]) {
        state.sync_session(snapshot);
        state.targets = targets
            .iter()
            .map(|(position, name)| WaypointTarget {
                position: Vec3 {
                    x: position[0],
                    y: position[1],
                    z: position[2],
                },
                name: (*name).to_owned(),
            })
            .collect();
    }

    fn waypoint_spec(x: f32, y: f32, name: &str) -> WaypointSpec {
        WaypointSpec {
            x,
            y,
            name: name.to_owned(),
        }
    }

    fn message(topic: &str, source_addon_id: &str, payload: &[u8]) -> Message {
        Message {
            id: 1,
            monotonic_ms: 100,
            source_addon_id: source_addon_id.to_owned(),
            topic: topic.to_owned(),
            correlation_id: None,
            payload: payload.to_vec(),
        }
    }

    fn text_for<'a>(frame: &'a Frame, id: &str) -> Option<&'a str> {
        frame.surface("wayfinder-arrow")?.node(id)?.text()
    }
}

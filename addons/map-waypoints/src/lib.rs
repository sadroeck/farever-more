//! Permanent demon POIs with passive full-map annotations.
use farever_more_sdk::prelude::*;
use farever_poi_protocol::{Bounds, Client as PoiClient, Poi, PoiKind, QueryRegion};
use farever_waypoint_protocol::{Client as WaypointClient, Waypoint};

mod portraits {
    // Share the already extracted inventory artwork with the minimap.
    include!("../../minimap/assets/soulstones/icons_generated.rs");
}

#[path = "../../shared/soulstone_marker.rs"]
mod soulstone_marker;

const WORLD: &str = "World/W1_Siagarta";

struct MapWaypoints {
    gps: Option<WaypointClient>,
    pois: Vec<Poi>,
    icons: Vec<(&'static str, Image)>,
}

impl Addon for MapWaypoints {
    fn activate(context: &mut ActivateContext) -> SdkResult<Self> {
        let dependencies = context.dependencies();
        let gps = WaypointClient::open(&dependencies).ok();
        let provider = PoiClient::open(&dependencies, "poi-database")
            .map_err(|error| format!("open typed POI service: {error}"))?;
        let page = provider
            .query_region(
                &QueryRegion::new(
                    WORLD,
                    Bounds {
                        min_x: -4096.0,
                        min_y: -4096.0,
                        max_x: 4096.0,
                        max_y: 4096.0,
                    },
                    64,
                )
                .with_kinds([PoiKind::Soulstone]),
            )
            .map_err(|error| format!("query demon POIs: {error}"))?;
        if page.truncated {
            return Err("Demon POI query exceeded its bounded page".into());
        }
        if page.pois.len() != portraits::SOURCES.len()
            || portraits::SOURCES
                .iter()
                .any(|(item, _)| page.pois.iter().filter(|poi| poi.id == *item).count() != 1)
        {
            return Err(
                "POI Database is missing the eight reviewed demon sites; update the provider"
                    .into(),
            );
        }
        let icons = portraits::SOURCES
            .iter()
            .map(|(item, bytes)| {
                context
                    .assets()
                    .register_image(&format!("soulstone-{item}"), bytes)
                    .map(|image| (*item, image))
            })
            .collect::<SdkResult<Vec<_>>>()?;
        context.timer().schedule(Duration::from_millis(16));
        context.log().info("Permanent full-map demon POIs ready");
        Ok(Self {
            gps,
            pois: page.pois,
            icons,
        })
    }

    fn on_tick(&mut self, context: &mut Context, _tick: Tick) -> SdkResult<TickControl> {
        let game = context.game();
        let session = game.snapshot().session;
        let map = game.map().value;
        let waypoint = self
            .gps
            .as_ref()
            .and_then(|gps| gps.current().ok().flatten());
        context.replace_ui(render(
            session,
            map.as_ref(),
            waypoint.as_ref(),
            &self.pois,
            &self.icons,
        ));
        Ok(TickControl::Continue)
    }
}

/// Map visibility controls the base POIs; GPS only controls the highlight.
fn render(
    session: Session,
    map: Option<&VisibleMap>,
    waypoint: Option<&Waypoint>,
    pois: &[Poi],
    icons: &[(&str, Image)],
) -> Frame {
    let Some(map) = map else {
        return Frame::empty();
    };
    if !session.in_world {
        return Frame::empty();
    }
    let selected = waypoint
        .filter(|point| {
            point.process_session == session.process_session && point.world == map.world
        })
        .and_then(|point| point.item_id.as_deref());
    let mut markers: Vec<_> = pois
        .iter()
        .filter(|poi| poi.world == map.world && poi.kind.known() == Some(PoiKind::Soulstone))
        .filter_map(|poi| {
            let image = icons.iter().find(|(id, _)| *id == poi.id)?.1.clone();
            let center = marker_position(map, [poi.x, poi.y, poi.z.unwrap_or_default()])?;
            Some((center, image, selected == Some(poi.id.as_str())))
        })
        .collect();
    // The prominent selected marker stays above nearby unselected POIs.
    markers.sort_by_key(|(_, _, selected)| *selected);
    if markers.is_empty() {
        return Frame::empty();
    }
    let scale = map.pixels_per_point;
    let Some(bounds) = visible_bounds(map) else {
        return Frame::empty();
    };
    let mut frame = FrameBuilder::new();
    frame.surface(
        "full-map-waypoint",
        "Map Waypoint",
        SurfaceOptions::new(Anchor::TopLeft)
            .margin(bounds.left / scale, bounds.top / scale)
            .style(SurfaceStyle::new(Color::TRANSPARENT)),
        |ui| {
            ui.passive_canvas(
                "map-portrait",
                [bounds.width / scale, bounds.height / scale],
                |canvas| {
                    for (center, image, selected) in &markers {
                        soulstone_marker::draw(canvas, *center, image, *selected);
                    }
                },
            );
        },
    );
    frame.finish()
}

fn marker_position(map: &VisibleMap, position: [f32; 3]) -> Option<[f32; 2]> {
    let scale = map.pixels_per_point;
    let b = visible_bounds(map)?;
    if !scale.is_finite()
        || scale <= 0.0
        || b.width <= 0.0
        || b.height <= 0.0
        || ![b.left, b.top, b.width, b.height]
            .iter()
            .chain(position.iter())
            .all(|v| v.is_finite())
    {
        return None;
    }
    let projected = map.world_to_client.project([position[0], position[1]]);
    if !projected.iter().all(|v| v.is_finite()) {
        return None;
    }
    let local = [
        (projected[0] - b.left) / scale,
        (projected[1] - b.top) / scale,
    ];
    let half = soulstone_marker::MAX_REACH;
    // Never clamp a location to the edge. Partially visible portraits are
    // clipped by the canvas to the native map rectangle.
    (local[0] + half > 0.0
        && local[1] + half > 0.0
        && local[0] - half < b.width / scale
        && local[1] - half < b.height / scale)
        .then_some(local)
}

/// Corner-anchored surfaces use nonnegative insets. Clip a native map that
/// extends past the client origin before converting to canvas coordinates,
/// so host margin normalization cannot move the portrait.
fn visible_bounds(map: &VisibleMap) -> Option<MapBounds> {
    let b = map.bounds;
    let left = b.left.max(0.0);
    let top = b.top.max(0.0);
    let width = b.left + b.width - left;
    let height = b.top + b.height - top;
    ([b.left, b.top, b.width, b.height, width, height]
        .iter()
        .all(|v| v.is_finite())
        && b.width > 0.0
        && b.height > 0.0
        && width > 0.0
        && height > 0.0)
        .then_some(MapBounds {
            left,
            top,
            width,
            height,
        })
}

export!(MapWaypoints);

#[cfg(test)]
mod tests {
    use super::*;

    fn map() -> VisibleMap {
        VisibleMap {
            world: "World/W1_Siagarta".into(),
            bounds: MapBounds {
                left: 100.0,
                top: 200.0,
                width: 800.0,
                height: 600.0,
            },
            world_to_client: MapTransform {
                a: 2.0,
                b: 0.0,
                c: 0.0,
                d: 2.0,
                tx: 120.0,
                ty: 240.0,
            },
            pixels_per_point: 2.0,
        }
    }
    fn waypoint() -> Waypoint {
        Waypoint {
            process_session: 7,
            world: map().world,
            position: [10.0, 20.0, 0.0],
            name: "Asmodax".into(),
            item_id: Some("Soulstone_Z1_1".into()),
        }
    }
    fn icons() -> Vec<(&'static str, Image)> {
        vec![(
            "Soulstone_Z1_1",
            Image {
                id: "portrait".into(),
            },
        )]
    }
    fn pois() -> Vec<Poi> {
        let point = waypoint();
        vec![Poi::new(
            PoiKind::Soulstone,
            "Soulstone_Z1_1",
            point.name,
            point.world,
            point.position[0],
            point.position[1],
            Some(point.position[2]),
        )]
    }
    fn paths(frame: &Frame) -> usize {
        frame
            .surfaces()
            .flat_map(|surface| surface.canvas())
            .filter(|command| matches!(command.primitive(), PrimitiveRef::Path(_)))
            .count()
    }

    #[test]
    fn follows_pan_zoom_and_display_scale() {
        let mut map = map();
        assert_eq!(marker_position(&map, [10.0, 20.0, 0.0]), Some([20.0, 40.0]));
        map.world_to_client.a = 4.0;
        map.world_to_client.d = 4.0;
        map.world_to_client.tx += 80.0;
        assert_eq!(marker_position(&map, [10.0, 20.0, 0.0]), Some([70.0, 60.0]));
    }

    #[test]
    fn clips_map_at_client_origin_without_moving_world_position() {
        let mut map = map();
        map.bounds.left = -100.0;
        assert_eq!(marker_position(&map, [10.0, 20.0, 0.0]), Some([70.0, 40.0]));
        let frame = render(
            Session {
                in_world: true,
                process_session: 7,
            },
            Some(&map),
            Some(&waypoint()),
            &pois(),
            &icons(),
        );
        assert_eq!(frame.surfaces().next().unwrap().margin(), [0.0, 100.0]);
        map.bounds.left = -900.0;
        assert!(marker_position(&map, [10.0, 20.0, 0.0]).is_none());
    }
    #[test]
    fn keeps_partial_icons_at_true_position_and_hides_offscreen_targets() {
        let map = map();
        assert_eq!(
            marker_position(&map, [-20.0, 0.0, 0.0]),
            Some([-10.0, 20.0])
        );
        assert!(marker_position(&map, [-40.0, 0.0, 0.0]).is_none());
        assert!(marker_position(&map, [500.0, 0.0, 0.0]).is_none());
        assert!(marker_position(&map, [f32::NAN, 0.0, 0.0]).is_none());
    }
    #[test]
    fn renders_inventory_portrait_as_passive_canvas_at_map_origin() {
        let frame = render(
            Session {
                in_world: true,
                process_session: 7,
            },
            Some(&map()),
            Some(&waypoint()),
            &pois(),
            &icons(),
        );
        let surface = frame.surfaces().next().expect("portrait surface");
        assert_eq!(surface.margin(), [50.0, 100.0]);
        assert_eq!(surface.nodes().next().unwrap().kind(), NodeKind::Canvas);
    }
    #[test]
    fn arrival_or_stale_gps_clears_only_highlight_and_closed_map_clears_everything() {
        let session = Session {
            in_world: true,
            process_session: 7,
        };
        let map = map();
        let mut point = waypoint();
        assert_eq!(
            render(session, None, Some(&point), &pois(), &icons())
                .surfaces()
                .count(),
            0
        );
        assert_eq!(
            render(session, Some(&map), None, &pois(), &icons())
                .surfaces()
                .count(),
            1
        );
        assert_eq!(
            paths(&render(
                session,
                Some(&map),
                Some(&point),
                &pois(),
                &icons()
            )),
            3
        );
        assert_eq!(
            paths(&render(session, Some(&map), None, &pois(), &icons())),
            2
        );
        point.process_session = 8;
        assert_eq!(
            paths(&render(
                session,
                Some(&map),
                Some(&point),
                &pois(),
                &icons()
            )),
            2
        );
        point.process_session = 7;
        point.world = "World/W2".into();
        assert_eq!(
            paths(&render(
                session,
                Some(&map),
                Some(&point),
                &pois(),
                &icons()
            )),
            2
        );
        let mut other_world = map;
        other_world.world = "World/W2".into();
        assert_eq!(
            render(session, Some(&other_world), None, &pois(), &icons())
                .surfaces()
                .count(),
            0
        );
        assert_eq!(
            render(
                Session {
                    in_world: false,
                    ..session
                },
                Some(&other_world),
                None,
                &pois(),
                &icons()
            )
            .surfaces()
            .count(),
            0
        );
    }
}

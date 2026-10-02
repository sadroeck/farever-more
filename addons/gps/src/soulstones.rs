use farever_more_sdk::prelude::{Vec3, VisibleMap};

mod data {
    include!("../assets/soulstones_generated.rs");
}

pub struct Soulstone {
    pub item: &'static str,
    pub name: &'static str,
    pub world: &'static str,
    pub position: [f32; 3],
}

pub fn find(item: &str) -> Option<&'static Soulstone> {
    data::SITES.iter().find(|site| site.item == item)
}

/// Native clicks use the displayed marker's 18-point hit radius at the current
/// zoom/DPI. Minimap requests have exact coordinates rounded to two decimals.
pub fn find_map_site(
    world: &str,
    position: [f32; 2],
    map: Option<&VisibleMap>,
) -> Option<&'static Soulstone> {
    if !position.iter().all(|value| value.is_finite()) {
        return None;
    }
    let map = map.filter(|map| {
        map.world == world && map.pixels_per_point.is_finite() && map.pixels_per_point > 0.0
    });
    let mut nearest = None;
    let mut best = f32::INFINITY;
    for site in data::SITES.iter().filter(|site| site.world == world) {
        let (distance, limit) = if let Some(map) = map {
            let click = map.world_to_client.project(position);
            let target = map
                .world_to_client
                .project([site.position[0], site.position[1]]);
            if click[0] < map.bounds.left
                || click[0] > map.bounds.left + map.bounds.width
                || click[1] < map.bounds.top
                || click[1] > map.bounds.top + map.bounds.height
            {
                continue;
            }
            (
                ((click[0] - target[0]).hypot(click[1] - target[1])) / map.pixels_per_point,
                18.0,
            )
        } else {
            (
                (position[0] - site.position[0]).hypot(position[1] - site.position[1]),
                0.02,
            )
        };
        if distance.is_finite() && distance <= limit && distance < best {
            best = distance;
            nearest = Some(site);
        }
    }
    nearest
}

impl Soulstone {
    pub fn position(&self) -> Vec3 {
        Vec3 {
            x: self.position[0],
            y: self.position[1],
            z: self.position[2],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use farever_more_sdk::prelude::{MapBounds, MapTransform};

    #[test]
    fn every_rounded_minimap_request_resolves_the_inventory_site() {
        for site in data::SITES {
            let rounded = [
                format!("{:.2}", site.position[0]).parse().unwrap(),
                format!("{:.2}", site.position[1]).parse().unwrap(),
            ];
            assert_eq!(
                find_map_site(site.world, rounded, None).unwrap().item,
                site.item
            );
            assert!(find_map_site("World/W2", rounded, None).is_none());
        }
        assert!(find_map_site(data::SITES[0].world, [f32::NAN, 0.0], None).is_none());
    }

    #[test]
    fn native_hit_radius_tracks_zoom_dpi_and_nearest_site_without_snapping_open_space() {
        let site = &data::SITES[5];
        for zoom in [0.1, 0.5, 2.0] {
            for dpi in [1.0, 2.0] {
                let map = VisibleMap {
                    world: site.world.into(),
                    bounds: MapBounds {
                        left: 100.0,
                        top: 100.0,
                        width: 800.0,
                        height: 600.0,
                    },
                    world_to_client: MapTransform {
                        a: zoom,
                        b: 0.0,
                        c: 0.0,
                        d: zoom,
                        tx: 500.0 - site.position[0] * zoom,
                        ty: 400.0 - site.position[1] * zoom,
                    },
                    pixels_per_point: dpi,
                };
                let inside = [site.position[0] + 17.0 * dpi / zoom, site.position[1]];
                assert_eq!(
                    find_map_site(site.world, inside, Some(&map)).unwrap().item,
                    site.item
                );
                let outside = [site.position[0] + 19.0 * dpi / zoom, site.position[1]];
                assert!(find_map_site(site.world, outside, Some(&map)).is_none());
                assert!(find_map_site(site.world, [4000.0, 4000.0], Some(&map)).is_none());
            }
        }
    }
}

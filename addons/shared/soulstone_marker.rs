//! Shared POI presentation for the full map and minimap.
use farever_more_sdk::prelude::*;

pub const HALF_SIZE: f32 = 12.0;
pub const HIGHLIGHT_HALF_SIZE: f32 = 20.0;
/// Includes the contrasting halo's outer stroke, for plate clipping.
pub const MAX_REACH: f32 = 27.0;

pub fn draw(canvas: &mut CanvasBuilder<'_>, center: [f32; 2], icon: &Image, selected: bool) {
    let [x, y] = center;
    if selected {
        canvas.circle(
            center,
            24.0,
            Some(Color::rgba8(255, 213, 126, 48)),
            Some(Stroke::new(6.0, Color::rgba8(20, 14, 6, 245))),
        );
        canvas.circle(
            center,
            24.0,
            None,
            Some(Stroke::new(3.5, Color::rgba8(255, 213, 126, 255))),
        );
    }
    let half = if selected { 16.0 } else { HALF_SIZE };
    let icon_half = if selected { 12.0 } else { 8.0 };
    let diamond = |radius: f32, offset: f32| {
        [
            [x, y - radius + offset],
            [x + radius, y + offset],
            [x, y + radius + offset],
            [x - radius, y + offset],
        ]
    };
    canvas.path(
        diamond(half + 1.0, 1.5),
        true,
        Some(Color::rgba8(12, 10, 10, 210)),
        None,
    );
    canvas.path(
        diamond(half, 0.0),
        true,
        Some(Color::rgba8(64, 38, 42, 245)),
        Some(Stroke::new(1.0, Color::rgba8(180, 147, 110, 255))),
    );
    canvas.image(
        icon.clone(),
        [x - icon_half, y - icon_half],
        [x + icon_half, y + icon_half],
        [0.0, 0.0],
        [1.0, 1.0],
        0.0,
        None,
        0.0,
    );
    if selected {
        canvas.path(
            diamond(HIGHLIGHT_HALF_SIZE, 0.0),
            true,
            None,
            Some(Stroke::new(3.0, Color::rgba8(255, 213, 126, 255))),
        );
    }
}

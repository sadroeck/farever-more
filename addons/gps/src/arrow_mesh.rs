#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Material {
    Top,
    Bevel,
    Side,
    Underside,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Triangle {
    pub(crate) indices: [usize; 3],
    pub(crate) material: Material,
}

const fn triangle(a: usize, b: usize, c: usize, material: Material) -> Triangle {
    Triangle {
        indices: [a, b, c],
        material,
    }
}

/// A compact beveled arrow pointing toward local +Y.
///
/// Rings 0..7 and 7..14 form the vertical body, 14..21 is the inset
/// top bevel, 21..24 is the raised center ridge, and 24 is the underside
/// fan center. Keeping this as ordinary mesh data makes it straightforward to
/// replace with an offline-authored asset after the rendering experiment.
pub(crate) const VERTICES: [[f32; 3]; 25] = [
    [-0.18, -0.75, -0.18],
    [0.18, -0.75, -0.18],
    [0.18, 0.25, -0.18],
    [0.55, 0.25, -0.18],
    [0.0, 1.0, -0.18],
    [-0.55, 0.25, -0.18],
    [-0.18, 0.25, -0.18],
    [-0.18, -0.75, 0.08],
    [0.18, -0.75, 0.08],
    [0.18, 0.25, 0.08],
    [0.55, 0.25, 0.08],
    [0.0, 1.0, 0.08],
    [-0.55, 0.25, 0.08],
    [-0.18, 0.25, 0.08],
    [-0.15, -0.68, 0.18],
    [0.15, -0.68, 0.18],
    [0.15, 0.22, 0.18],
    [0.47, 0.22, 0.18],
    [0.0, 0.90, 0.18],
    [-0.47, 0.22, 0.18],
    [-0.15, 0.22, 0.18],
    [0.0, -0.68, 0.25],
    [0.0, 0.22, 0.28],
    [0.0, 0.86, 0.24],
    [0.0, 0.05, -0.18],
];

pub(crate) const TRIANGLES: [Triangle; 43] = [
    // Vertical sides.
    triangle(0, 1, 8, Material::Side),
    triangle(0, 8, 7, Material::Side),
    triangle(1, 2, 9, Material::Side),
    triangle(1, 9, 8, Material::Side),
    triangle(2, 3, 10, Material::Side),
    triangle(2, 10, 9, Material::Side),
    triangle(3, 4, 11, Material::Side),
    triangle(3, 11, 10, Material::Side),
    triangle(4, 5, 12, Material::Side),
    triangle(4, 12, 11, Material::Side),
    triangle(5, 6, 13, Material::Side),
    triangle(5, 13, 12, Material::Side),
    triangle(6, 0, 7, Material::Side),
    triangle(6, 7, 13, Material::Side),
    // Sloped bevel from the outer upper ring to the inset top ring.
    triangle(7, 8, 15, Material::Bevel),
    triangle(7, 15, 14, Material::Bevel),
    triangle(8, 9, 16, Material::Bevel),
    triangle(8, 16, 15, Material::Bevel),
    triangle(9, 10, 17, Material::Bevel),
    triangle(9, 17, 16, Material::Bevel),
    triangle(10, 11, 18, Material::Bevel),
    triangle(10, 18, 17, Material::Bevel),
    triangle(11, 12, 19, Material::Bevel),
    triangle(11, 19, 18, Material::Bevel),
    triangle(12, 13, 20, Material::Bevel),
    triangle(12, 20, 19, Material::Bevel),
    triangle(13, 7, 14, Material::Bevel),
    triangle(13, 14, 20, Material::Bevel),
    // Raised top facets and center ridge.
    triangle(14, 21, 22, Material::Top),
    triangle(14, 22, 20, Material::Top),
    triangle(21, 15, 16, Material::Top),
    triangle(21, 16, 22, Material::Top),
    triangle(22, 17, 23, Material::Top),
    triangle(17, 18, 23, Material::Top),
    triangle(22, 23, 19, Material::Top),
    triangle(19, 23, 18, Material::Top),
    // Reversed underside fan.
    triangle(24, 1, 0, Material::Underside),
    triangle(24, 2, 1, Material::Underside),
    triangle(24, 3, 2, Material::Underside),
    triangle(24, 4, 3, Material::Underside),
    triangle(24, 5, 4, Material::Underside),
    triangle(24, 6, 5, Material::Underside),
    triangle(24, 0, 6, Material::Underside),
];

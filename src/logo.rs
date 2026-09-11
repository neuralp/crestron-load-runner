// The application mark: three triangles arranged around an empty fourth.
//
// It is described rather than stored, so it is sharp at any size and there is
// no image file to keep beside the source. `build.rs` includes this file
// directly to raster the Windows executable icon, which is why the module has
// no inner documentation and may not depend on the rest of the crate.

/// Height of an equilateral triangle whose side is 1.
const HEIGHT: f32 = 0.866_025_4;
/// Where that triangle starts, so it sits centred in a square.
const TOP: f32 = (1.0 - HEIGHT) / 2.0;
const BOTTOM: f32 = TOP + HEIGHT;
const MIDDLE: f32 = TOP + HEIGHT / 2.0;

/// The three filled triangles, each as its corners in a unit square. Y grows
/// downward, as it does both on screen and in an icon.
pub const TRIANGLES: [[(f32, f32); 3]; 3] = [
    // Above the gap.
    [(0.5, TOP), (0.25, MIDDLE), (0.75, MIDDLE)],
    // Left and right of it; the inverted triangle between them stays empty.
    [(0.25, MIDDLE), (0.0, BOTTOM), (0.5, BOTTOM)],
    [(0.75, MIDDLE), (0.5, BOTTOM), (1.0, BOTTOM)],
];

pub const GOLD: [u8; 3] = [0xf0, 0xc4, 0x20];

/// Space left around the mark in a raster icon, as a fraction of its side.
/// Windows crops nothing, so the icon has to hold its own margin.
pub const MARGIN: f32 = 0.06;

/// The mark as straight (not premultiplied) RGBA, `size` pixels square.
pub fn rasterize(size: usize) -> Vec<u8> {
    /// Samples per axis. Sixteen coverage levels are enough for edges this
    /// straight, and keep the smallest icons from looking ragged.
    const SAMPLES: usize = 4;

    let mut pixels = vec![0; size * size * 4];
    let span = 1.0 - 2.0 * MARGIN;
    for y in 0..size {
        for x in 0..size {
            let mut covered = 0;
            for sample in 0..SAMPLES * SAMPLES {
                let offset = |index: usize| (index as f32 + 0.5) / SAMPLES as f32;
                // Back from the pixel into the unit square the mark is drawn in.
                let point = (
                    ((x as f32 + offset(sample % SAMPLES)) / size as f32 - MARGIN) / span,
                    ((y as f32 + offset(sample / SAMPLES)) / size as f32 - MARGIN) / span,
                );
                if TRIANGLES.iter().any(|triangle| contains(triangle, point)) {
                    covered += 1;
                }
            }
            let pixel = (y * size + x) * 4;
            pixels[pixel..pixel + 3].copy_from_slice(&GOLD);
            pixels[pixel + 3] = (covered * 255 / (SAMPLES * SAMPLES)) as u8;
        }
    }
    pixels
}

/// Whether a point is inside a triangle: it is on the same side of all three
/// edges. Corner-sharing triangles must not both claim a point, so one edge
/// orientation is taken as inside and the other as outside.
fn contains(triangle: &[(f32, f32); 3], (x, y): (f32, f32)) -> bool {
    let side =
        |(ax, ay): (f32, f32), (bx, by): (f32, f32)| (bx - ax) * (y - ay) - (by - ay) * (x - ax);
    let sides = [
        side(triangle[0], triangle[1]),
        side(triangle[1], triangle[2]),
        side(triangle[2], triangle[0]),
    ];
    sides.iter().all(|side| *side >= 0.0) || sides.iter().all(|side| *side <= 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Alpha at a point of the unit square the mark is drawn in.
    fn alpha(pixels: &[u8], size: usize, (x, y): (f32, f32)) -> u8 {
        let span = 1.0 - 2.0 * MARGIN;
        let pixel = |value: f32| ((MARGIN + value * span) * size as f32) as usize;
        pixels[(pixel(y) * size + pixel(x)) * 4 + 3]
    }

    #[test]
    fn the_mark_is_three_solid_triangles_around_an_empty_one() {
        let size = 128;
        let pixels = rasterize(size);
        assert_eq!(pixels.len(), size * size * 4);

        // The middle of each triangle is solid.
        for centre in TRIANGLES.map(|triangle| {
            let sum = triangle.iter().fold((0.0, 0.0), |sum, corner| {
                (sum.0 + corner.0, sum.1 + corner.1)
            });
            (sum.0 / 3.0, sum.1 / 3.0)
        }) {
            assert_eq!(alpha(&pixels, size, centre), 255, "{centre:?}");
        }
        // The triangle between them is not drawn, and neither is the ground
        // around the mark.
        assert_eq!(alpha(&pixels, size, (0.5, 0.65)), 0);
        for outside in [(0.02, 0.02), (0.98, 0.02), (0.5, 0.0), (0.02, 0.98)] {
            assert_eq!(alpha(&pixels, size, outside), 0, "{outside:?}");
        }
        // Every pixel carries the same colour; only coverage varies.
        assert!(
            pixels.chunks_exact(4).all(|pixel| pixel[..3] == GOLD),
            "the mark is one colour"
        );
    }

    #[test]
    fn the_smallest_icon_still_shows_the_gap() {
        let size = 16;
        let pixels = rasterize(size);
        assert_eq!(alpha(&pixels, size, (0.5, 0.35)), 255, "the top triangle");
        assert_eq!(alpha(&pixels, size, (0.5, 0.65)), 0, "the gap");
    }
}

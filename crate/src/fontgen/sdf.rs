//! Glyph outlines to TextMeshPro's `SDFAA` distance field.
//!
//! The field is the exact Euclidean distance from each texel centre to the outline, sampled at
//! the font asset's point size. The encoding is the one TextCore's `SDFAA` render mode writes
//! into an Alpha8 atlas: `127.5` on the edge, one gradient-scale's worth of texels either side
//! mapped onto the full byte range, positive inside. It was fitted against atlases Unity
//! 6000.5 baked itself and matches them along the edge, which is what normal text samples.
//! Further out TextCore runs an approximate transform over the anti-aliased bitmap, which
//! steps past corners and jitters along curves; this field is the exact distance there, so
//! outline and underlay effects come out smoother than on the glyphs FreeType adds later.

use ttf_parser::OutlineBuilder;

/// FreeType's 16.16 scale for a face at `ppem` pixels per em: `FT_DivFix(ppem << 6, upem)`.
pub fn freetype_scale(ppem: u32, units_per_em: u16) -> i64 {
    let upem = units_per_em as i64;
    (((ppem as i64) << 6 << 16) + upem / 2) / upem
}

/// `FT_MulFix`: scales a font-unit coordinate to 26.6 pixels, rounding half away from zero.
/// Outline points are snapped this way before anything measures them, which is why TextCore's
/// glyph metrics are all multiples of 1/64 px.
fn scale_26_6(v: f64, scale: i64) -> f64 {
    let units = v.round() as i64;
    let scaled = (units.abs() * scale + 0x8000) >> 16;
    let fixed = if units < 0 { -scaled } else { scaled };
    fixed as f64 / 64.0
}

/// Flattening tolerance in pixels. Far below what an 8-bit field can show.
const FLATTEN_TOLERANCE: f64 = 1.0 / 32.0;

const MAX_CURVE_SEGMENTS: usize = 64;

/// A glyph outline at the sampling point size, flattened into line segments.
#[derive(Default)]
pub struct Outline {
    scale: i64,
    segments: Vec<[f64; 4]>,
    start: [f64; 2],
    last: [f64; 2],
    open: bool,
    /// FreeType's control box: the extent of every point, on and off the curve.
    pub x_min: f64,
    pub y_min: f64,
    pub x_max: f64,
    pub y_max: f64,
}

impl Outline {
    /// `scale` is [`freetype_scale`]'s 16.16 fixed-point factor.
    pub fn new(scale: i64) -> Self {
        Outline {
            scale,
            x_min: f64::INFINITY,
            y_min: f64::INFINITY,
            x_max: f64::NEG_INFINITY,
            y_max: f64::NEG_INFINITY,
            ..Outline::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    fn point(&mut self, x: f32, y: f32) -> [f64; 2] {
        let p = [
            scale_26_6(x as f64, self.scale),
            scale_26_6(y as f64, self.scale),
        ];
        self.x_min = self.x_min.min(p[0]);
        self.y_min = self.y_min.min(p[1]);
        self.x_max = self.x_max.max(p[0]);
        self.y_max = self.y_max.max(p[1]);
        p
    }

    fn line(&mut self, to: [f64; 2]) {
        if to != self.last {
            self.segments
                .push([self.last[0], self.last[1], to[0], to[1]]);
        }
        self.last = to;
    }

    fn close_contour(&mut self) {
        if self.open {
            let start = self.start;
            self.line(start);
            self.open = false;
        }
    }

    fn curve(&mut self, deviation: f64, eval: impl Fn(f64) -> [f64; 2]) {
        let n =
            ((deviation / FLATTEN_TOLERANCE).sqrt().ceil() as usize).clamp(1, MAX_CURVE_SEGMENTS);
        for i in 1..=n {
            self.line(eval(i as f64 / n as f64));
        }
    }

    /// Signed distance from `(px, py)` to the outline: positive inside under the nonzero
    /// winding rule both TrueType and CFF outlines use.
    fn signed_distance(&self, px: f64, py: f64) -> f64 {
        let mut best = f64::INFINITY;
        let mut winding = 0i32;
        for &[x0, y0, x1, y1] in &self.segments {
            let (dx, dy) = (x1 - x0, y1 - y0);
            let len2 = dx * dx + dy * dy;
            let t = if len2 > 0.0 {
                (((px - x0) * dx + (py - y0) * dy) / len2).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let (ex, ey) = (x0 + t * dx - px, y0 + t * dy - py);
            best = best.min(ex * ex + ey * ey);

            let cross = dx * (py - y0) - (px - x0) * dy;
            if y0 <= py {
                if y1 > py && cross > 0.0 {
                    winding += 1;
                }
            } else if y1 <= py && cross < 0.0 {
                winding -= 1;
            }
        }
        let d = best.sqrt();
        if winding != 0 {
            d
        } else {
            -d
        }
    }
}

impl OutlineBuilder for Outline {
    fn move_to(&mut self, x: f32, y: f32) {
        self.close_contour();
        let p = self.point(x, y);
        self.start = p;
        self.last = p;
        self.open = true;
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let p = self.point(x, y);
        self.line(p);
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let p0 = self.last;
        let c = self.point(x1, y1);
        let p = self.point(x, y);
        let deviation = ((p0[0] - 2.0 * c[0] + p[0]).hypot(p0[1] - 2.0 * c[1] + p[1])) / 4.0;
        self.curve(deviation, |t| {
            let u = 1.0 - t;
            [
                u * u * p0[0] + 2.0 * u * t * c[0] + t * t * p[0],
                u * u * p0[1] + 2.0 * u * t * c[1] + t * t * p[1],
            ]
        });
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let p0 = self.last;
        let c1 = self.point(x1, y1);
        let c2 = self.point(x2, y2);
        let p = self.point(x, y);
        let d1 = (p0[0] - 2.0 * c1[0] + c2[0]).hypot(p0[1] - 2.0 * c1[1] + c2[1]);
        let d2 = (c1[0] - 2.0 * c2[0] + p[0]).hypot(c1[1] - 2.0 * c2[1] + p[1]);
        self.curve(d1.max(d2) * 0.75, |t| {
            let u = 1.0 - t;
            let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
            [
                a * p0[0] + b * c1[0] + c * c2[0] + d * p[0],
                a * p0[1] + b * c1[1] + c * c2[1] + d * p[1],
            ]
        });
    }

    fn close(&mut self) {
        self.close_contour();
    }
}

/// The pixel box FreeType renders a glyph into: its control box snapped outward to whole
/// pixels. `(left, bottom, width, height)`.
pub fn bitmap_box(o: &Outline) -> (i32, i32, u32, u32) {
    let left = o.x_min.floor() as i32;
    let bottom = o.y_min.floor() as i32;
    let right = o.x_max.ceil() as i32;
    let top = o.y_max.ceil() as i32;
    (left, bottom, (right - left) as u32, (top - bottom) as u32)
}

/// Renders the glyph's field over its bitmap box grown by `padding` on every side. Rows run
/// bottom-up, like the Alpha8 atlas the field is copied into.
pub fn render(o: &Outline, padding: u32, gradient_scale: f64) -> (Vec<u8>, u32, u32) {
    let (left, bottom, w, h) = bitmap_box(o);
    let fw = w + 2 * padding;
    let fh = h + 2 * padding;
    let step = 255.0 / (2.0 * gradient_scale);
    let mut field = vec![0u8; (fw * fh) as usize];
    for j in 0..fh {
        let py = bottom as f64 - padding as f64 + j as f64 + 0.5;
        for i in 0..fw {
            let px = left as f64 - padding as f64 + i as f64 + 0.5;
            let d = o.signed_distance(px, py);
            field[(j * fw + i) as usize] = (127.5 + d * step).round().clamp(0.0, 255.0) as u8;
        }
    }
    (field, fw, fh)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One font unit to one pixel.
    const UNIT: i64 = 64 << 16;

    fn square(size: f32) -> Outline {
        let mut o = Outline::new(UNIT);
        o.move_to(0.0, 0.0);
        o.line_to(size, 0.0);
        o.line_to(size, size);
        o.line_to(0.0, size);
        o.close();
        o
    }

    #[test]
    fn distance_is_positive_inside_and_negative_outside() {
        let o = square(10.0);
        assert!((o.signed_distance(5.0, 5.0) - 5.0).abs() < 1e-9);
        assert!((o.signed_distance(-2.0, 5.0) + 2.0).abs() < 1e-9);
    }

    #[test]
    fn edge_texels_straddle_the_midpoint_by_half_a_texel() {
        let o = square(20.0);
        let (field, fw, _) = render(&o, 9, 10.0);
        // Row 8 of the padded box is half a texel below the square, row 9 half a texel in.
        let x = 9 + 10;
        assert_eq!(field[(8 * fw + x) as usize], 121);
        assert_eq!(field[(9 * fw + x) as usize], 134);
    }

    #[test]
    fn bitmap_box_snaps_outward() {
        // 90 px per em on a 1000-unit em: 253 units land at 22.765625 px.
        let mut o = Outline::new(freetype_scale(90, 1000));
        o.move_to(253.0, 0.0);
        o.line_to(396.0, 0.0);
        o.line_to(396.0, 698.0);
        o.close();
        assert_eq!(bitmap_box(&o), (22, 0, 14, 63));
    }

    #[test]
    fn scaling_rounds_like_freetype() {
        let s = freetype_scale(90, 1000);
        assert_eq!(s, 377487);
        assert_eq!(scale_26_6(253.0, s), 22.765625);
        assert_eq!(scale_26_6(-253.0, s), -22.765625);
    }
}

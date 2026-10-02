//! A scene font rewritten from validated values, so no byte of the original file reaches a client.
//!
//! A font bundle carries its font for FreeType, which draws any character outside the
//! pre-filled set at runtime, and a scene controls its own text, so it can always make that
//! happen. Embedding the uploaded file would hand FreeType untrusted bytes. Instead every table
//! is regenerated from what was parsed and the limits here accepted:
//!
//! - `glyf`/`loca`: each glyph's outline re-encoded as a simple glyph (composites flattened)
//!   with no hinting instructions;
//! - `cmap`: the Unicode mappings, as format 4 and format 12;
//! - `head`, `hhea`, `hmtx`, `maxp`, `OS/2`, `post`: the metrics FreeType and TextCore read,
//!   with the source's values, so the face measures the same;
//! - `name`: family and style only;
//! - `GPOS`: one `kern` lookup holding the pairs [`super::kerning`] read.
//!
//! Hinting programs, variation data, colour and bitmap glyphs, layout rules other than kerning,
//! and any table this file does not name are dropped. Glyph ids are kept, so the pre-filled
//! assets and the rebuilt font agree on them.
//!
//! The two tables a hostile font can make expensive, `glyf` and `cmap`, are read raw here
//! rather than through `ttf-parser`, so the walk that is bounded is the walk that runs: every
//! composite is sized from the component graph (nodes, points and nesting) before any outline
//! is expanded, and every `cmap` range is clamped and charged against a budget before any code
//! point is visited. Glyph parsing follows the specification's layout and `ttf-parser` 0.25's
//! semantics where they are a choice (no grid rounding or scaled offsets, a one-point glyph is
//! empty, a malformed glyph is empty, a component outside the font is skipped), which is what
//! the fidelity test compares against; anchored components, which `ttf-parser` misreads, are
//! placed by their matched points. A font past a bound is [`Refused`](super::Refused).

use super::kerning::KernPair;
use super::{MAX_FONT_BYTES, MAX_GLYPH_POINTS};
use anyhow::Result;
use std::collections::BTreeMap;
use ttf_parser::{name_id, Face, GlyphId, Tag};

/// Outline points across every glyph of the font, composites expanded. Bounds the rebuild the
/// way [`MAX_GLYPH_POINTS`] bounds one glyph.
pub const MAX_FONT_POINTS: usize = 4_000_000;

/// Glyphs visited while flattening one composite, itself included. A glyph of no points still
/// costs a visit, so this is what bounds a fan-out of empty leaves.
pub const MAX_GLYPH_NODES: usize = 1024;

/// Glyph visits across every glyph of the font.
pub const MAX_FONT_NODES: usize = 1_000_000;

/// Components one composite glyph may reference directly. Real composites reference a handful.
pub const MAX_GLYPH_COMPONENTS: usize = 64;

/// Composite nesting the rebuild follows: `ttf-parser`'s own ceiling.
pub const MAX_COMPOSITE_DEPTH: usize = 32;

/// Encoding records a `cmap` may hold. Real fonts have two to four.
pub const MAX_CMAP_SUBTABLES: usize = 32;

/// Work the `cmap` walk may do across every subtable: one unit per group or segment record read
/// and one per code point visited. Every Unicode scalar twice covers a full-repertoire font
/// carrying both a BMP and a full-range subtable.
pub const MAX_CMAP_WORK: usize = 2 * 0x11_0000;

/// Coordinates are re-encoded as 16-bit deltas, so they must stay within half the range.
const MAX_COORDINATE: f32 = 16383.0;

/// Characters a name record keeps.
const MAX_NAME_CHARS: usize = 63;

/// A GPOS subtable addresses its parts with 16-bit offsets.
const MAX_GPOS_BYTES: usize = 0xFFFF;

const ON_CURVE: u8 = 0x01;
const X_SHORT: u8 = 0x02;
const Y_SHORT: u8 = 0x04;
const REPEAT: u8 = 0x08;
const X_SAME_OR_POSITIVE: u8 = 0x10;
const Y_SAME_OR_POSITIVE: u8 = 0x20;

const ARG_1_AND_2_ARE_WORDS: u16 = 0x0001;
const ARGS_ARE_XY_VALUES: u16 = 0x0002;
const WE_HAVE_A_SCALE: u16 = 0x0008;
const MORE_COMPONENTS: u16 = 0x0020;
const WE_HAVE_AN_X_AND_Y_SCALE: u16 = 0x0040;
const WE_HAVE_A_TWO_BY_TWO: u16 = 0x0080;

#[derive(Default)]
struct W(Vec<u8>);

impl W {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn i16(&mut self, v: i16) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn i32(&mut self, v: i32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn i64(&mut self, v: i64) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }
    fn pad4(&mut self) {
        while !self.0.len().is_multiple_of(4) {
            self.0.push(0);
        }
    }
    fn len(&self) -> usize {
        self.0.len()
    }
    fn set_u16(&mut self, at: usize, v: u16) {
        self.0[at..at + 2].copy_from_slice(&v.to_be_bytes());
    }
}

fn be_u8(data: &[u8], at: usize) -> Option<u8> {
    data.get(at).copied()
}

fn be_u16(data: &[u8], at: usize) -> Option<u16> {
    data.get(at..at + 2)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
}

fn be_i16(data: &[u8], at: usize) -> Option<i16> {
    be_u16(data, at).map(|v| v as i16)
}

fn be_u32(data: &[u8], at: usize) -> Option<u32> {
    data.get(at..at + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// F2Dot14 to float, as `ttf-parser` converts it.
fn f2dot14(v: i16) -> f32 {
    v as f32 / 16384.0
}

/// A composite component's affine transform, `ttf-parser`'s `Transform`: `x' = ax + cy + e`,
/// `y' = bx + dy + f`. The offset is added unscaled, as `ttf-parser` and FreeType's default do.
#[derive(Clone, Copy)]
struct Transform {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
}

impl Transform {
    const IDENTITY: Transform = Transform {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };
}

struct Component {
    glyph: u16,
    transform: Transform,
    /// `(parent point, child point)` when the component is placed by matching points rather
    /// than by an offset (`ARGS_ARE_XY_VALUES` clear).
    anchor: Option<(u16, u16)>,
}

/// The source's `glyf` and `loca` tables, read raw.
struct GlyfTables<'a> {
    glyf: &'a [u8],
    loca: &'a [u8],
    long_offsets: bool,
    /// Glyphs the font has: `maxp`'s count, or fewer when `loca` holds fewer entries. The one
    /// bound every walk here uses, so no glyph is flattened that was not sized.
    glyphs: usize,
}

impl<'a> GlyfTables<'a> {
    fn of(face: &Face<'a>) -> Option<Self> {
        let raw = face.raw_face();
        let glyf = raw.table(Tag::from_bytes(b"glyf"))?;
        let loca = raw.table(Tag::from_bytes(b"loca"))?;
        let long_offsets = matches!(
            face.tables().head.index_to_location_format,
            ttf_parser::head::IndexToLocationFormat::Long
        );
        let entry = if long_offsets { 4 } else { 2 };
        let glyphs = (loca.len() / entry)
            .saturating_sub(1)
            .min(face.number_of_glyphs() as usize);
        Some(GlyfTables {
            glyf,
            loca,
            long_offsets,
            glyphs,
        })
    }

    fn glyph(&self, gid: usize) -> &'a [u8] {
        if gid >= self.glyphs {
            return &[];
        }
        let range = if self.long_offsets {
            be_u32(self.loca, 4 * gid).zip(be_u32(self.loca, 4 * gid + 4))
        } else {
            be_u16(self.loca, 2 * gid)
                .zip(be_u16(self.loca, 2 * gid + 2))
                .map(|(s, e)| (s as u32 * 2, e as u32 * 2))
        };
        range
            .filter(|(s, e)| s <= e)
            .and_then(|(s, e)| self.glyf.get(s as usize..e as usize))
            .unwrap_or(&[])
    }

    /// A composite's components, laid out as the specification stores them: two arguments are
    /// always present (bytes or words), read as an offset with `ARGS_ARE_XY_VALUES` and as a
    /// pair of point numbers without it. No grid rounding, no scaled offsets.
    fn components(data: &[u8]) -> Result<Vec<Component>> {
        if be_i16(data, 0).is_none_or(|n| n >= 0) {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let mut at = 10;
        while let (Some(flags), Some(glyph)) = (be_u16(data, at), be_u16(data, at + 2)) {
            at += 4;
            let (arg1, arg2) = if flags & ARG_1_AND_2_ARE_WORDS != 0 {
                let (Some(a), Some(b)) = (be_u16(data, at), be_u16(data, at + 2)) else {
                    break;
                };
                at += 4;
                (a, b)
            } else {
                let (Some(a), Some(b)) = (be_u8(data, at), be_u8(data, at + 1)) else {
                    break;
                };
                at += 2;
                (a as u16, b as u16)
            };
            let mut t = Transform::IDENTITY;
            let anchor = if flags & ARGS_ARE_XY_VALUES != 0 {
                if flags & ARG_1_AND_2_ARE_WORDS != 0 {
                    (t.e, t.f) = (arg1 as i16 as f32, arg2 as i16 as f32);
                } else {
                    (t.e, t.f) = (arg1 as u8 as i8 as f32, arg2 as u8 as i8 as f32);
                }
                None
            } else {
                Some((arg1, arg2))
            };
            if flags & WE_HAVE_A_TWO_BY_TWO != 0 {
                let Some(v) = (0..4)
                    .map(|i| be_i16(data, at + 2 * i))
                    .collect::<Option<Vec<_>>>()
                else {
                    break;
                };
                (t.a, t.b, t.c, t.d) = (f2dot14(v[0]), f2dot14(v[1]), f2dot14(v[2]), f2dot14(v[3]));
                at += 8;
            } else if flags & WE_HAVE_AN_X_AND_Y_SCALE != 0 {
                let (Some(a), Some(d)) = (be_i16(data, at), be_i16(data, at + 2)) else {
                    break;
                };
                (t.a, t.d) = (f2dot14(a), f2dot14(d));
                at += 4;
            } else if flags & WE_HAVE_A_SCALE != 0 {
                let Some(a) = be_i16(data, at) else {
                    break;
                };
                (t.a, t.d) = (f2dot14(a), f2dot14(a));
                at += 2;
            }
            out.push(Component {
                glyph,
                transform: t,
                anchor,
            });
            if out.len() > MAX_GLYPH_COMPONENTS {
                refuse!("a composite glyph references over {MAX_GLYPH_COMPONENTS} components");
            }
            if flags & MORE_COMPONENTS == 0 {
                break;
            }
        }
        Ok(out)
    }

    /// Points a simple glyph stores: one past its last contour end. A glyph of one point is
    /// empty, as `ttf-parser` treats it.
    fn simple_points(data: &[u8]) -> usize {
        match be_i16(data, 0) {
            Some(n) if n > 0 => {
                let total = be_u16(data, 10 + 2 * (n as usize - 1)).map_or(0, |e| e as usize + 1);
                if total == 1 {
                    0
                } else {
                    total
                }
            }
            _ => 0,
        }
    }

    /// A simple glyph's contours in font units. `None` for a glyph whose data does not hold what
    /// its header declares, which `ttf-parser` cannot outline either.
    fn simple_contours(data: &[u8]) -> Option<Vec<Vec<(f32, f32, bool)>>> {
        let Some(n) = be_i16(data, 0).filter(|n| *n > 0) else {
            return Some(Vec::new());
        };
        let n = n as usize;
        let ends: Vec<u16> = (0..n)
            .map(|i| be_u16(data, 10 + 2 * i))
            .collect::<Option<_>>()?;
        let total = ends.last().map_or(0, |e| *e as usize + 1);
        if total <= 1 {
            return Some(Vec::new());
        }
        let mut at = 10 + 2 * n;
        let instructions = be_u16(data, at)? as usize;
        at += 2 + instructions;

        let mut flags = Vec::with_capacity(total);
        while flags.len() < total {
            let flag = be_u8(data, at)?;
            at += 1;
            let repeats = if flag & REPEAT != 0 {
                let r = be_u8(data, at)?;
                at += 1;
                r as usize + 1
            } else {
                1
            };
            if flags.len() + repeats > total {
                return None;
            }
            flags.extend(std::iter::repeat_n(flag, repeats));
        }

        let mut read_axis = |short: u8, same_or_positive: u8| -> Option<Vec<f32>> {
            let mut out = Vec::with_capacity(total);
            let mut v = 0i32;
            for &flag in &flags {
                let delta = if flag & short != 0 {
                    let d = be_u8(data, at)? as i32;
                    at += 1;
                    if flag & same_or_positive != 0 {
                        d
                    } else {
                        -d
                    }
                } else if flag & same_or_positive != 0 {
                    0
                } else {
                    let d = be_i16(data, at)? as i32;
                    at += 2;
                    d
                };
                v += delta;
                out.push(v as f32);
            }
            Some(out)
        };
        let xs = read_axis(X_SHORT, X_SAME_OR_POSITIVE)?;
        let ys = read_axis(Y_SHORT, Y_SAME_OR_POSITIVE)?;

        let mut contours = Vec::with_capacity(n);
        let mut start = 0usize;
        for end in ends {
            let end = end as usize + 1;
            if end <= start || end > total {
                return None;
            }
            contours.push(
                (start..end)
                    .map(|i| (xs[i], ys[i], flags[i] & ON_CURVE != 0))
                    .collect(),
            );
            start = end;
        }
        Some(contours)
    }
}

/// What flattening a glyph costs, from the component graph alone.
#[derive(Clone, Copy)]
struct Sizing {
    nodes: usize,
    points: usize,
    /// Composite levels below this glyph.
    height: usize,
}

/// Sizes every glyph before any outline is built. Counts saturate just past their caps, and a
/// glyph over a cap, a cycle, or nesting past `ttf-parser`'s depth is refused here. The depth
/// is checked on the way down, so the sizing itself never recurses past it.
fn size_glyphs(tables: &GlyfTables<'_>) -> Result<Vec<Sizing>> {
    const UNVISITED: usize = usize::MAX;
    const IN_PROGRESS: usize = usize::MAX - 1;
    let n = tables.glyphs;
    let mut memo = vec![
        Sizing {
            nodes: UNVISITED,
            points: 0,
            height: 0
        };
        n
    ];

    fn visit(
        tables: &GlyfTables<'_>,
        gid: usize,
        depth: usize,
        memo: &mut [Sizing],
    ) -> Result<Sizing> {
        // `ttf-parser` gives up at this depth; refusing before recursing is what bounds the
        // stack here.
        if depth >= MAX_COMPOSITE_DEPTH {
            refuse!("composite glyphs nest {MAX_COMPOSITE_DEPTH} deep or more");
        }
        let Some(entry) = memo.get(gid).copied() else {
            // A component outside the font: `ttf-parser` skips it.
            return Ok(Sizing {
                nodes: 1,
                points: 0,
                height: 0,
            });
        };
        match entry.nodes {
            IN_PROGRESS => refuse!("glyph {gid} is a composite of itself"),
            UNVISITED => {}
            _ => {
                if depth + entry.height >= MAX_COMPOSITE_DEPTH {
                    refuse!("composite glyphs nest {MAX_COMPOSITE_DEPTH} deep or more");
                }
                return Ok(entry);
            }
        }
        memo[gid].nodes = IN_PROGRESS;
        let data = tables.glyph(gid);
        let components = GlyfTables::components(data)?;
        let mut sizing = Sizing {
            nodes: 1,
            points: if components.is_empty() {
                GlyfTables::simple_points(data)
            } else {
                0
            },
            height: 0,
        };
        for c in components {
            let child = visit(tables, c.glyph as usize, depth + 1, memo)?;
            sizing.nodes = sizing
                .nodes
                .saturating_add(child.nodes)
                .min(MAX_GLYPH_NODES + 1);
            sizing.points = sizing
                .points
                .saturating_add(child.points)
                .min(MAX_GLYPH_POINTS + 1);
            sizing.height = sizing.height.max(child.height + 1);
        }
        memo[gid] = sizing;
        Ok(sizing)
    }

    let (mut nodes, mut points) = (0usize, 0usize);
    for gid in 0..n {
        let s = visit(tables, gid, 0, &mut memo)?;
        if s.nodes > MAX_GLYPH_NODES {
            refuse!("glyph {gid} flattens through over {MAX_GLYPH_NODES} glyphs");
        }
        if s.points > MAX_GLYPH_POINTS {
            refuse!("glyph {gid} expands to over {MAX_GLYPH_POINTS} outline points");
        }
        nodes += s.nodes;
        points += s.points;
        if nodes > MAX_FONT_NODES {
            refuse!("font composites flatten through over {MAX_FONT_NODES} glyphs");
        }
        if points > MAX_FONT_POINTS {
            refuse!("font outlines expand to over {MAX_FONT_POINTS} points");
        }
    }
    Ok(memo)
}

/// A glyph's contours in its own space, composites flattened: each component's points go
/// through its transform, with an anchored component moved so its matched point lands on the
/// parent's. `None` when a glyph on the way is malformed, which empties the whole glyph as it
/// does in `ttf-parser`. Bounded by [`size_glyphs`] having passed the glyph.
fn flatten(tables: &GlyfTables<'_>, gid: usize) -> Result<Option<Vec<Vec<(f32, f32, bool)>>>> {
    let data = tables.glyph(gid);
    let components = GlyfTables::components(data)?;
    if components.is_empty() {
        return Ok(GlyfTables::simple_contours(data));
    }
    let mut out: Vec<Vec<(f32, f32, bool)>> = Vec::new();
    for c in components {
        if c.glyph as usize >= tables.glyphs {
            continue;
        }
        let Some(child) = flatten(tables, c.glyph as usize)? else {
            return Ok(None);
        };
        let t = c.transform;
        let scaled: Vec<Vec<(f32, f32, bool)>> = child
            .into_iter()
            .map(|contour| {
                contour
                    .into_iter()
                    .map(|(x, y, on)| (t.a * x + t.c * y, t.b * x + t.d * y, on))
                    .collect()
            })
            .collect();
        let (e, f) = match c.anchor {
            None => (t.e, t.f),
            Some((parent, child)) => {
                let parent = out.iter().flatten().nth(parent as usize);
                let child = scaled.iter().flatten().nth(child as usize);
                match (parent, child) {
                    (Some(p), Some(q)) => (p.0 - q.0, p.1 - q.1),
                    _ => (0.0, 0.0),
                }
            }
        };
        out.extend(scaled.into_iter().map(|contour| {
            contour
                .into_iter()
                .map(|(x, y, on)| (x + e, y + f, on))
                .collect()
        }));
    }
    Ok(Some(out))
}

/// Integer contours as TrueType stores them, range-checked as floats first: nested composite
/// scales can push a value past what the integer cast would saturate to.
fn to_points(contours: &[Vec<(f32, f32, bool)>]) -> Result<Vec<Vec<(i32, i32, bool)>>> {
    let mut out = Vec::with_capacity(contours.len());
    for c in contours {
        let mut contour = Vec::with_capacity(c.len());
        for &(x, y, on) in c {
            if !x.is_finite()
                || !y.is_finite()
                || x.abs() > MAX_COORDINATE
                || y.abs() > MAX_COORDINATE
            {
                refuse!("glyph coordinate out of range");
            }
            contour.push((x.round() as i32, y.round() as i32, on));
        }
        if !contour.is_empty() {
            out.push(contour);
        }
    }
    Ok(out)
}

fn bbox(contours: &[Vec<(i32, i32, bool)>]) -> Option<(i32, i32, i32, i32)> {
    let mut it = contours.iter().flatten();
    let &(x, y, _) = it.next()?;
    Some(it.fold((x, y, x, y), |(a, b, c, d), &(x, y, _)| {
        (a.min(x), b.min(y), c.max(x), d.max(y))
    }))
}

struct GlyphRecord {
    data: Vec<u8>,
    bbox: Option<(i32, i32, i32, i32)>,
    advance: u16,
    points: usize,
    contours: usize,
}

fn encode_glyph(contours: &[Vec<(i32, i32, bool)>]) -> (Vec<u8>, Option<(i32, i32, i32, i32)>) {
    let Some((x_min, y_min, x_max, y_max)) = bbox(contours) else {
        return (Vec::new(), None);
    };
    let mut w = W::default();
    w.i16(contours.len() as i16);
    for v in [x_min, y_min, x_max, y_max] {
        w.i16(v as i16);
    }
    let mut end = 0usize;
    for contour in contours {
        end += contour.len();
        w.u16((end - 1) as u16);
    }
    w.u16(0); // no instructions

    let points: Vec<_> = contours.iter().flatten().copied().collect();
    let (mut flags, mut xs, mut ys) = (Vec::new(), W::default(), W::default());
    let (mut px, mut py) = (0i32, 0i32);
    for &(x, y, on) in &points {
        let (dx, dy) = (x - px, y - py);
        (px, py) = (x, y);
        let mut flag = if on { ON_CURVE } else { 0 };
        if dx == 0 {
            flag |= X_SAME_OR_POSITIVE;
        } else if dx.abs() < 256 {
            flag |= X_SHORT | if dx > 0 { X_SAME_OR_POSITIVE } else { 0 };
            xs.u8(dx.unsigned_abs() as u8);
        } else {
            xs.i16(dx as i16);
        }
        if dy == 0 {
            flag |= Y_SAME_OR_POSITIVE;
        } else if dy.abs() < 256 {
            flag |= Y_SHORT | if dy > 0 { Y_SAME_OR_POSITIVE } else { 0 };
            ys.u8(dy.unsigned_abs() as u8);
        } else {
            ys.i16(dy as i16);
        }
        flags.push(flag);
    }
    w.bytes(&flags);
    w.bytes(&xs.0);
    w.bytes(&ys.0);
    w.pad4();
    (w.0, Some((x_min, y_min, x_max, y_max)))
}

fn glyph_records(face: &Face<'_>) -> Result<Vec<GlyphRecord>> {
    let n = face.number_of_glyphs() as usize;
    let advance = |gid: usize| face.glyph_hor_advance(GlyphId(gid as u16)).unwrap_or(0);
    let empty = |gid: usize| GlyphRecord {
        data: Vec::new(),
        bbox: None,
        advance: advance(gid),
        points: 0,
        contours: 0,
    };
    let Some(tables) = GlyfTables::of(face) else {
        // No outlines at all: every glyph is empty. The bake refuses the font later if nothing
        // it pre-fills can be drawn.
        return Ok((0..n).map(empty).collect());
    };
    size_glyphs(&tables)?;
    (0..n)
        .map(|gid| {
            let Some(raw) = flatten(&tables, gid)? else {
                return Ok(empty(gid));
            };
            let contours = to_points(&raw)?;
            let (data, bbox) = encode_glyph(&contours);
            Ok(GlyphRecord {
                data,
                bbox,
                advance: advance(gid),
                points: contours.iter().map(Vec::len).sum(),
                contours: contours.len(),
            })
        })
        .collect()
}

/// A `cmap` encoding record for Unicode, as `ttf-parser`'s `is_unicode`: platform 0, or
/// Windows with the BMP or full-range encoding.
fn is_unicode_record(platform: u16, encoding: u16) -> bool {
    platform == 0 || (platform == 3 && (encoding == 1 || encoding == 10))
}

/// Walks one `cmap` subtable raw, calling `map(code point, glyph)` for every mapping it declares,
/// each range clamped to Unicode and every record and code point charged to `budget` first.
fn walk_subtable(data: &[u8], budget: &mut usize, mut map: impl FnMut(u32, u16)) -> Result<()> {
    fn charge(budget: &mut usize, units: usize) -> Result<()> {
        if units > *budget {
            refuse!("cmap needs over {MAX_CMAP_WORK} units of work to read");
        }
        *budget -= units;
        Ok(())
    }
    /// `lo..=hi` clamped to Unicode; `None` when empty.
    fn clamp(lo: u32, hi: u32) -> Option<(u32, u32)> {
        let hi = hi.min(0x10_FFFF);
        (lo <= hi).then_some((lo, hi))
    }
    match be_u16(data, 0) {
        Some(0) => {
            charge(budget, 256)?;
            for cp in 0..256u32 {
                if let Some(g) = be_u8(data, 6 + cp as usize) {
                    map(cp, g as u16);
                }
            }
        }
        Some(4) => {
            let seg_count = be_u16(data, 6).unwrap_or(0) as usize / 2;
            charge(budget, seg_count)?;
            let ends = 14;
            let starts = ends + 2 * seg_count + 2;
            let deltas = starts + 2 * seg_count;
            let range_offsets = deltas + 2 * seg_count;
            for i in 0..seg_count {
                let (Some(start), Some(end), Some(delta), Some(range_offset)) = (
                    be_u16(data, starts + 2 * i),
                    be_u16(data, ends + 2 * i),
                    be_u16(data, deltas + 2 * i),
                    be_u16(data, range_offsets + 2 * i),
                ) else {
                    break;
                };
                let Some((lo, hi)) = clamp(start as u32, end as u32) else {
                    continue;
                };
                charge(budget, (hi - lo) as usize + 1)?;
                for cp in lo..=hi {
                    let g = if range_offset == 0 {
                        (cp as u16).wrapping_add(delta)
                    } else {
                        let at = range_offsets
                            + 2 * i
                            + range_offset as usize
                            + 2 * (cp - start as u32) as usize;
                        match be_u16(data, at) {
                            Some(0) | None => 0,
                            Some(g) => g.wrapping_add(delta),
                        }
                    };
                    map(cp, g);
                }
            }
        }
        Some(6) => {
            if let (Some(first), Some(count)) = (be_u16(data, 6), be_u16(data, 8)) {
                charge(budget, count as usize)?;
                for i in 0..count as usize {
                    let cp = first as u32 + i as u32;
                    if cp > 0xFFFF {
                        break;
                    }
                    if let Some(g) = be_u16(data, 10 + 2 * i) {
                        map(cp, g);
                    }
                }
            }
        }
        Some(10) => {
            if let (Some(start), Some(count)) = (be_u32(data, 12), be_u32(data, 16)) {
                let Some((lo, hi)) = clamp(start, start.saturating_add(count.saturating_sub(1)))
                else {
                    return Ok(());
                };
                charge(budget, (hi - lo) as usize + 1)?;
                for cp in lo..=hi {
                    if let Some(g) = be_u16(data, 20 + 2 * (cp - start) as usize) {
                        map(cp, g);
                    }
                }
            }
        }
        Some(format @ (12 | 13)) => {
            let groups = be_u32(data, 12).unwrap_or(0) as usize;
            charge(budget, groups)?;
            for i in 0..groups {
                let at = 16 + 12 * i;
                let (Some(start), Some(end), Some(glyph)) =
                    (be_u32(data, at), be_u32(data, at + 4), be_u32(data, at + 8))
                else {
                    break;
                };
                let Some((lo, hi)) = clamp(start, end) else {
                    continue;
                };
                charge(budget, (hi - lo) as usize + 1)?;
                for cp in lo..=hi {
                    let g = if format == 12 {
                        glyph.checked_add(cp - start)
                    } else {
                        Some(glyph)
                    };
                    if let Some(g) = g.filter(|g| *g <= u16::MAX as u32) {
                        map(cp, g as u16);
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// The Unicode mappings the font declares: the first unicode subtable mapping a code point
/// wins, as in `ttf-parser`'s `Face::glyph_index`. Read raw and bounded by
/// [`MAX_CMAP_SUBTABLES`] records and [`MAX_CMAP_WORK`] units of work for the whole font, so a
/// `cmap` is never a place a hostile font can make the lane spend time.
pub(super) fn unicode_map(face: &Face<'_>) -> Result<BTreeMap<u32, u16>> {
    let mut map = BTreeMap::new();
    let Some(raw) = face.raw_face().table(Tag::from_bytes(b"cmap")) else {
        return Ok(map);
    };
    let records = be_u16(raw, 2).unwrap_or(0) as usize;
    if records > MAX_CMAP_SUBTABLES {
        refuse!("cmap has {records} encoding records, over {MAX_CMAP_SUBTABLES}");
    }
    let glyphs = face.number_of_glyphs();
    let mut budget = MAX_CMAP_WORK;
    let mut seen = Vec::new();
    for i in 0..records {
        let at = 4 + 8 * i;
        let (Some(platform), Some(encoding), Some(offset)) =
            (be_u16(raw, at), be_u16(raw, at + 2), be_u32(raw, at + 4))
        else {
            break;
        };
        if !is_unicode_record(platform, encoding) || seen.contains(&offset) {
            continue;
        }
        seen.push(offset);
        let Some(data) = raw.get(offset as usize..) else {
            continue;
        };
        // As `ttf-parser`: a Windows full-range record only counts with a full-range format.
        if platform == 3 && encoding == 10 && !matches!(be_u16(data, 0), Some(12) | Some(13)) {
            continue;
        }
        walk_subtable(data, &mut budget, |cp, g| {
            if g != 0 && g < glyphs && char::from_u32(cp).is_some() {
                map.entry(cp).or_insert(g);
            }
        })?;
    }
    Ok(map)
}

/// Runs of consecutive code points mapped to consecutive glyphs: `(first cp, last cp, first gid)`.
fn runs(map: impl Iterator<Item = (u32, u16)>) -> Vec<(u32, u32, u16)> {
    let mut out: Vec<(u32, u32, u16)> = Vec::new();
    for (cp, gid) in map {
        match out.last_mut() {
            Some((start, end, g)) if *end + 1 == cp && *g as u32 + (cp - *start) == gid as u32 => {
                *end = cp
            }
            _ => out.push((cp, cp, gid)),
        }
    }
    out
}

fn cmap_table(map: &BTreeMap<u32, u16>) -> Vec<u8> {
    let groups = runs(map.iter().map(|(&c, &g)| (c, g)));
    let mut f12 = W::default();
    f12.u16(12);
    f12.u16(0);
    f12.u32((16 + 12 * groups.len()) as u32);
    f12.u32(0);
    f12.u32(groups.len() as u32);
    for &(start, end, gid) in &groups {
        f12.u32(start);
        f12.u32(end);
        f12.u32(gid as u32);
    }

    // Format 4 for clients that only read the BMP subtable, when its 16-bit length allows.
    let mut segments = runs(map.range(..0xFFFF).map(|(&c, &g)| (c, g)));
    segments.push((0xFFFF, 0xFFFF, 0));
    let seg_count = segments.len();
    let f4_len = 16 + 8 * seg_count;
    let f4 = (f4_len <= 0xFFFF).then(|| {
        let mut f4 = W::default();
        let search = 2 * (1u16 << (seg_count as f64).log2().floor() as u16);
        f4.u16(4);
        f4.u16(f4_len as u16);
        f4.u16(0);
        f4.u16((seg_count * 2) as u16);
        f4.u16(search);
        f4.u16((search / 2).trailing_zeros() as u16);
        f4.u16((seg_count * 2) as u16 - search);
        for s in &segments {
            f4.u16(s.1 as u16);
        }
        f4.u16(0);
        for s in &segments {
            f4.u16(s.0 as u16);
        }
        for s in &segments {
            // The terminating segment maps 0xFFFF to glyph 0.
            let delta = if s.0 == 0xFFFF {
                1
            } else {
                s.2.wrapping_sub(s.0 as u16)
            };
            f4.u16(delta);
        }
        for _ in &segments {
            f4.u16(0);
        }
        f4.0
    });

    let subtables: Vec<(u16, Vec<u8>)> = f4
        .map(|b| (1u16, b))
        .into_iter()
        .chain([(10u16, f12.0)])
        .collect();
    let mut w = W::default();
    w.u16(0);
    w.u16(subtables.len() as u16);
    let mut offset = 4 + 8 * subtables.len();
    for (encoding, data) in &subtables {
        w.u16(3);
        w.u16(*encoding);
        w.u32(offset as u32);
        offset += data.len();
    }
    for (_, data) in subtables {
        w.bytes(&data);
    }
    w.0
}

fn clean_name(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME_CHARS)
        .collect()
}

fn read_name(face: &Face<'_>, id: u16) -> Option<String> {
    let names: Vec<_> = face
        .names()
        .into_iter()
        .filter(|n| n.name_id == id && n.is_unicode())
        .collect();
    names
        .iter()
        .find(|n| n.language_id == 0x0409)
        .or_else(|| names.first())
        .and_then(|n| n.to_string())
        .map(|s| clean_name(&s))
        .filter(|s| !s.is_empty())
}

fn name_table(face: &Face<'_>) -> Vec<u8> {
    let family = read_name(face, name_id::FAMILY).unwrap_or_else(|| "Scene Font".into());
    let style = read_name(face, name_id::SUBFAMILY).unwrap_or_else(|| "Regular".into());
    let postscript: String = format!("{family}-{style}")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(MAX_NAME_CHARS)
        .collect();
    let mut records: Vec<(u16, String)> = vec![
        (name_id::FAMILY, family.clone()),
        (name_id::SUBFAMILY, style.clone()),
        (name_id::FULL_NAME, format!("{family} {style}")),
        (name_id::POST_SCRIPT_NAME, postscript),
    ];
    for id in [name_id::TYPOGRAPHIC_FAMILY, name_id::TYPOGRAPHIC_SUBFAMILY] {
        if let Some(v) = read_name(face, id) {
            records.push((id, v));
        }
    }
    records.sort_by_key(|r| r.0);

    let strings: Vec<Vec<u8>> = records
        .iter()
        .map(|(_, s)| s.encode_utf16().flat_map(u16::to_be_bytes).collect())
        .collect();
    let mut w = W::default();
    w.u16(0);
    w.u16(records.len() as u16);
    w.u16((6 + 12 * records.len()) as u16);
    let mut offset = 0usize;
    for ((id, _), s) in records.iter().zip(&strings) {
        w.u16(3);
        w.u16(1);
        w.u16(0x0409);
        w.u16(*id);
        w.u16(s.len() as u16);
        w.u16(offset as u16);
        offset += s.len();
    }
    for s in strings {
        w.bytes(&s);
    }
    w.0
}

/// One `kern` feature over a single pair-positioning lookup, for both the default and Latin
/// scripts, with the pairs it holds. Pairs that do not fit the subtable's 16-bit offsets are
/// dropped smallest first, so the caller writes the same pairs into the font assets.
fn gpos_table(pairs: &[KernPair]) -> Option<(Vec<u8>, Vec<KernPair>)> {
    let mut kept: Vec<KernPair> = pairs.to_vec();
    kept.sort_by_key(|p| std::cmp::Reverse(p.x_advance.unsigned_abs()));
    let header = 10 + 2 + 2 * 26 + 2 + 6 + 2 + 2 + 6 + 8;
    let size = |pairs: &[KernPair]| {
        let firsts = {
            let mut f: Vec<u16> = pairs.iter().map(|p| p.first).collect();
            f.sort_unstable();
            f.dedup();
            f.len()
        };
        header + 10 + 2 * firsts + 4 + 2 * firsts + 2 * firsts + 4 * pairs.len()
    };
    while !kept.is_empty() && size(&kept) > MAX_GPOS_BYTES {
        let cut = kept.len() - (kept.len() / 16).max(1);
        kept.truncate(cut);
    }
    if kept.is_empty() {
        return None;
    }
    kept.sort_by_key(|p| (p.first, p.second));

    let mut by_first: BTreeMap<u16, Vec<&KernPair>> = BTreeMap::new();
    for p in &kept {
        by_first.entry(p.first).or_default().push(p);
    }

    // PairPosFormat1: coverage of first glyphs, one pair set each, first glyph's XAdvance only.
    let mut pp = W::default();
    let n = by_first.len();
    pp.u16(1);
    let coverage_at = pp.len();
    pp.u16(0);
    pp.u16(0x0004);
    pp.u16(0);
    pp.u16(n as u16);
    let offsets_at = pp.len();
    for _ in 0..n {
        pp.u16(0);
    }
    for (i, set) in by_first.values().enumerate() {
        let at = pp.len() as u16;
        pp.set_u16(offsets_at + 2 * i, at);
        pp.u16(set.len() as u16);
        for p in set {
            pp.u16(p.second);
            pp.i16(p.x_advance);
        }
    }
    let coverage = pp.len() as u16;
    pp.set_u16(coverage_at, coverage);
    pp.u16(1);
    pp.u16(n as u16);
    for first in by_first.keys() {
        pp.u16(*first);
    }

    let mut w = W::default();
    w.u16(1);
    w.u16(0);
    w.u16(10); // ScriptList
    w.u16(0); // FeatureList, patched
    w.u16(0); // LookupList, patched

    // ScriptList: DFLT and latn share one script whose default language runs feature 0.
    let script_list = w.len();
    w.u16(2);
    for tag in [b"DFLT", b"latn"] {
        w.bytes(tag);
        w.u16(2 + 6 * 2);
    }
    w.u16(4); // Script: default LangSys right after
    w.u16(0);
    w.u16(0); // LangSys: no lookup order
    w.u16(0xFFFF);
    w.u16(1);
    w.u16(0);
    debug_assert_eq!(script_list, 10);

    let feature_list = w.len();
    w.set_u16(6, feature_list as u16);
    w.u16(1);
    w.bytes(b"kern");
    w.u16(8);
    w.u16(0); // Feature: no params
    w.u16(1);
    w.u16(0);

    let lookup_list = w.len();
    w.set_u16(8, lookup_list as u16);
    w.u16(1);
    w.u16(4);
    w.u16(2); // Lookup: pair adjustment
    w.u16(0);
    w.u16(1);
    w.u16(8);
    w.bytes(&pp.0);
    (w.len() <= MAX_GPOS_BYTES).then_some((w.0, kept))
}

fn checksum(data: &[u8]) -> u32 {
    data.chunks(4).fold(0u32, |sum, c| {
        let mut b = [0u8; 4];
        b[..c.len()].copy_from_slice(c);
        sum.wrapping_add(u32::from_be_bytes(b))
    })
}

/// An sfnt file over `tables`: the sorted directory, each table padded to four bytes, and
/// `head`'s checksum adjustment set so the whole file sums to the magic.
fn assemble(mut tables: Vec<([u8; 4], Vec<u8>)>) -> Vec<u8> {
    tables.sort_by_key(|t| t.0);
    let n = tables.len() as u16;
    let search = 16 * (1u16 << (n as f64).log2().floor() as u16);
    let mut font = W::default();
    font.u32(0x0001_0000);
    font.u16(n);
    font.u16(search);
    font.u16((search / 16).trailing_zeros() as u16);
    font.u16(n * 16 - search);
    let mut offset = 12 + 16 * tables.len();
    let mut head_at = None;
    for (tag, data) in &tables {
        font.bytes(tag);
        font.u32(checksum(data));
        font.u32(offset as u32);
        font.u32(data.len() as u32);
        if tag == b"head" {
            head_at = Some(offset);
        }
        offset += data.len().div_ceil(4) * 4;
    }
    for (_, data) in &tables {
        font.bytes(data);
        font.pad4();
    }
    if let Some(head_at) = head_at {
        let adjustment = 0xB1B0_AFBAu32.wrapping_sub(checksum(&font.0));
        font.0[head_at + 8..head_at + 12].copy_from_slice(&adjustment.to_be_bytes());
    }
    font.0
}

/// Rewrites `face` as a TrueType file of regenerated tables, kerned by as many of `pairs` as
/// the GPOS can hold; returns the file and the pairs it kept.
pub fn rebuild(face: &Face<'_>, pairs: &[KernPair]) -> Result<(Vec<u8>, Vec<KernPair>)> {
    let glyphs = glyph_records(face)?;
    let num_glyphs = glyphs.len();
    if num_glyphs == 0 {
        refuse!("font has no glyphs");
    }
    let upem = face.units_per_em();
    let map = unicode_map(face)?;

    let mut glyf = W::default();
    let mut loca = W::default();
    for g in &glyphs {
        loca.u32(glyf.len() as u32);
        glyf.bytes(&g.data);
    }
    loca.u32(glyf.len() as u32);

    let mut hmtx = W::default();
    let (mut adv_max, mut min_lsb, mut min_rsb, mut max_extent) =
        (0u16, i32::MAX, i32::MAX, i32::MIN);
    let (mut bbox, mut adv_sum, mut adv_count) = (None::<(i32, i32, i32, i32)>, 0u64, 0u64);
    for g in &glyphs {
        let lsb = g.bbox.map_or(0, |b| b.0);
        hmtx.u16(g.advance);
        hmtx.i16(lsb as i16);
        adv_max = adv_max.max(g.advance);
        if g.advance > 0 {
            adv_sum += g.advance as u64;
            adv_count += 1;
        }
        if let Some(b) = g.bbox {
            min_lsb = min_lsb.min(b.0);
            min_rsb = min_rsb.min(g.advance as i32 - b.2);
            max_extent = max_extent.max(b.2);
            bbox = Some(match bbox {
                None => b,
                Some(a) => (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)),
            });
        }
    }
    let bbox = bbox.unwrap_or((0, 0, 0, 0));
    let (min_lsb, min_rsb, max_extent) = if max_extent == i32::MIN {
        (0, 0, 0)
    } else {
        (min_lsb, min_rsb, max_extent)
    };

    let tables = face.tables();
    let os2 = tables.os2;
    let bold = face.is_bold();
    let italic = face.is_italic();

    let mut head = W::default();
    head.u32(0x0001_0000);
    head.u32(0x0001_0000);
    head.u32(0); // checkSumAdjustment, set by `assemble`
    head.u32(0x5F0F_3CF5);
    head.u16(0x0009); // baseline at y=0, integer ppem
    head.u16(upem);
    head.i64(0);
    head.i64(0);
    for v in [bbox.0, bbox.1, bbox.2, bbox.3] {
        head.i16(v as i16);
    }
    head.u16(bold as u16 | (italic as u16) << 1);
    head.u16(8);
    head.i16(2);
    head.i16(1);
    head.i16(0);

    let hhea_src = tables.hhea;
    let mut hhea = W::default();
    hhea.u32(0x0001_0000);
    hhea.i16(hhea_src.ascender);
    hhea.i16(hhea_src.descender);
    hhea.i16(hhea_src.line_gap);
    hhea.u16(adv_max);
    hhea.i16(min_lsb as i16);
    hhea.i16(min_rsb.clamp(i16::MIN as i32, i16::MAX as i32) as i16);
    hhea.i16(max_extent as i16);
    hhea.i16(1);
    hhea.i16(0);
    hhea.i16(0);
    for _ in 0..4 {
        hhea.i16(0);
    }
    hhea.i16(0);
    hhea.u16(num_glyphs as u16);

    let mut maxp = W::default();
    maxp.u32(0x0001_0000);
    maxp.u16(num_glyphs as u16);
    maxp.u16(glyphs.iter().map(|g| g.points).max().unwrap_or(0) as u16);
    maxp.u16(glyphs.iter().map(|g| g.contours).max().unwrap_or(0) as u16);
    maxp.u16(0);
    maxp.u16(0);
    maxp.u16(1);
    for _ in 0..8 {
        maxp.u16(0);
    }

    let mut os2_w = W::default();
    let sub = os2.map(|t| t.subscript_metrics());
    let sup = os2.map(|t| t.superscript_metrics());
    let strike = os2.map(|t| t.strikeout_metrics());
    let typo = os2.map_or(
        (hhea_src.ascender, hhea_src.descender, hhea_src.line_gap),
        |t| {
            (
                t.typographic_ascender(),
                t.typographic_descender(),
                t.typographic_line_gap(),
            )
        },
    );
    let win = os2.map_or(
        (
            hhea_src.ascender.max(0),
            hhea_src.descender.min(0).saturating_neg(),
        ),
        // `ttf-parser` negates usWinDescent; the table stores it positive.
        |t| (t.windows_ascender(), t.windows_descender().saturating_neg()),
    );
    let mut fs_selection = 0u16;
    if italic {
        fs_selection |= 1;
    }
    if bold {
        fs_selection |= 1 << 5;
    }
    if !italic && !bold {
        fs_selection |= 1 << 6;
    }
    if os2.is_some_and(|t| t.use_typographic_metrics()) {
        fs_selection |= 1 << 7;
    }
    if face.is_oblique() {
        fs_selection |= 1 << 9;
    }
    let bmp: Vec<u32> = map.keys().copied().filter(|&c| c <= 0xFFFF).collect();
    os2_w.u16(4);
    os2_w.i16(
        adv_sum
            .checked_div(adv_count)
            .unwrap_or(0)
            .min(i16::MAX as u64) as i16,
    );
    os2_w.u16(face.weight().to_number());
    os2_w.u16(face.width().to_number());
    os2_w.u16(0);
    for m in [sub, sup] {
        let m = m.unwrap_or(ttf_parser::os2::ScriptMetrics {
            x_size: 0,
            y_size: 0,
            x_offset: 0,
            y_offset: 0,
        });
        os2_w.i16(m.x_size);
        os2_w.i16(m.y_size);
        os2_w.i16(m.x_offset);
        os2_w.i16(m.y_offset);
    }
    os2_w.i16(strike.map_or(0, |s| s.thickness));
    os2_w.i16(strike.map_or(0, |s| s.position));
    os2_w.i16(0);
    os2_w.bytes(&[0u8; 10]);
    for _ in 0..4 {
        os2_w.u32(0);
    }
    os2_w.bytes(b"    ");
    os2_w.u16(fs_selection);
    os2_w.u16(bmp.first().copied().unwrap_or(0) as u16);
    os2_w.u16(bmp.last().copied().unwrap_or(0) as u16);
    os2_w.i16(typo.0);
    os2_w.i16(typo.1);
    os2_w.i16(typo.2);
    os2_w.u16(win.0 as u16);
    os2_w.u16(win.1 as u16);
    os2_w.u32(0);
    os2_w.u32(0);
    os2_w.i16(os2.and_then(|t| t.x_height()).unwrap_or(0));
    os2_w.i16(os2.and_then(|t| t.capital_height()).unwrap_or(0));
    os2_w.u16(0);
    os2_w.u16(0x20);
    os2_w.u16(2);

    let mut post = W::default();
    let post_src = tables.post;
    post.u32(0x0003_0000);
    post.i32(post_src.map_or(0, |p| (p.italic_angle as f64 * 65536.0).round() as i32));
    post.i16(post_src.map_or(0, |p| p.underline_metrics.position));
    post.i16(post_src.map_or(0, |p| p.underline_metrics.thickness));
    post.u32(post_src.is_some_and(|p| p.is_monospaced) as u32);
    for _ in 0..4 {
        post.u32(0);
    }

    let mut out_tables: Vec<([u8; 4], Vec<u8>)> = vec![
        (*b"OS/2", os2_w.0),
        (*b"cmap", cmap_table(&map)),
        (*b"glyf", glyf.0),
        (*b"head", head.0),
        (*b"hhea", hhea.0),
        (*b"hmtx", hmtx.0),
        (*b"loca", loca.0),
        (*b"maxp", maxp.0),
        (*b"name", name_table(face)),
        (*b"post", post.0),
    ];
    let kept = match gpos_table(pairs) {
        Some((gpos, kept)) => {
            out_tables.push((*b"GPOS", gpos));
            kept
        }
        None => Vec::new(),
    };
    let font = assemble(out_tables);
    if font.len() > MAX_FONT_BYTES {
        // Deterministic for the input, so a refusal: it is cached and never rebuilt.
        refuse!(
            "rebuilt font is {} bytes, over the {MAX_FONT_BYTES}-byte cap",
            font.len()
        );
    }
    Ok((font, kept))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn runs_join_only_consecutive_mappings() {
        let map = [(65, 10), (66, 11), (67, 13), (69, 14)];
        assert_eq!(
            runs(map.into_iter()),
            vec![(65, 66, 10), (67, 67, 13), (69, 69, 14)]
        );
    }

    #[test]
    fn checksum_pads_the_last_word() {
        assert_eq!(checksum(&[0, 0, 0, 1, 0, 0, 1]), 1 + 256);
    }

    /// A simple glyph of `n` points on one contour, with small deltas (one byte each).
    pub fn simple_glyph(n: usize) -> Vec<u8> {
        let contour: Vec<(i32, i32, bool)> = (0..n as i32)
            .map(|i| (i * 7 % 600, i * 13 % 700, true))
            .collect();
        encode_glyph(&[contour]).0
    }

    /// A simple glyph of `n` points whose deltas all take two bytes.
    pub fn wide_glyph(n: usize) -> Vec<u8> {
        let contour: Vec<(i32, i32, bool)> = (0..n as i32)
            .map(|i| ((i % 2) * 1000, ((i + 1) % 2) * 1000, true))
            .collect();
        encode_glyph(&[contour]).0
    }

    /// A glyph with no contours and no points.
    pub fn empty_glyph() -> Vec<u8> {
        let mut w = W::default();
        for _ in 0..5 {
            w.i16(0);
        }
        w.0
    }

    /// A composite glyph of `components`, each placed at the origin.
    pub fn composite_glyph(components: &[u16]) -> Vec<u8> {
        composite_glyph_with(components, ARGS_ARE_XY_VALUES)
    }

    /// A composite glyph with the given argument `flags` on every component.
    pub fn composite_glyph_with(components: &[u16], flags: u16) -> Vec<u8> {
        let mut w = W::default();
        w.i16(-1);
        for _ in 0..4 {
            w.i16(0);
        }
        for (i, &gid) in components.iter().enumerate() {
            let more = if i + 1 < components.len() {
                MORE_COMPONENTS
            } else {
                0
            };
            w.u16(flags | more);
            w.u16(gid);
            w.u8(0);
            w.u8(0);
        }
        w.pad4();
        w.0
    }

    /// A `cmap` with one subtable of `format` 12 or 13 over the given groups
    /// `(start, end, glyph)`.
    pub fn cmap_groups(format: u16, groups: &[(u32, u32, u32)]) -> Vec<u8> {
        let mut w = W::default();
        w.u16(0);
        w.u16(1);
        w.u16(3);
        w.u16(10);
        w.u32(12);
        w.u16(format);
        w.u16(0);
        w.u32((16 + 12 * groups.len()) as u32);
        w.u32(0);
        w.u32(groups.len() as u32);
        for &(s, e, g) in groups {
            w.u32(s);
            w.u32(e);
            w.u32(g);
        }
        w.0
    }

    pub fn cmap_format12(groups: &[(u32, u32, u32)]) -> Vec<u8> {
        cmap_groups(12, groups)
    }

    /// A TrueType file of `glyphs` with 1000 units per em, 600-unit advances, the given `cmap`
    /// and any `extra` tables.
    pub fn fixture(glyphs: &[Vec<u8>], cmap: Vec<u8>, extra: Vec<([u8; 4], Vec<u8>)>) -> Vec<u8> {
        fixture_declaring(glyphs, glyphs.len(), cmap, extra)
    }

    /// [`fixture`] with `maxp` and `hhea` declaring `declared` glyphs, however many `glyphs`
    /// the `loca` holds.
    pub fn fixture_declaring(
        glyphs: &[Vec<u8>],
        declared: usize,
        cmap: Vec<u8>,
        extra: Vec<([u8; 4], Vec<u8>)>,
    ) -> Vec<u8> {
        let n = declared;
        let mut head = W::default();
        head.u32(0x0001_0000);
        head.u32(0x0001_0000);
        head.u32(0);
        head.u32(0x5F0F_3CF5);
        head.u16(0x0009);
        head.u16(1000);
        head.i64(0);
        head.i64(0);
        for _ in 0..4 {
            head.i16(0);
        }
        head.u16(0);
        head.u16(8);
        head.i16(2);
        head.i16(1); // long loca offsets
        head.i16(0);

        let mut hhea = W::default();
        hhea.u32(0x0001_0000);
        hhea.i16(800);
        hhea.i16(-200);
        hhea.i16(0);
        hhea.u16(600);
        hhea.i16(0);
        hhea.i16(0);
        hhea.i16(600);
        hhea.i16(1);
        for _ in 0..7 {
            hhea.i16(0);
        }
        hhea.u16(n as u16);

        let mut maxp = W::default();
        maxp.u32(0x0001_0000);
        maxp.u16(n as u16);
        maxp.u16(u16::MAX);
        maxp.u16(1);
        maxp.u16(u16::MAX);
        maxp.u16(1);
        maxp.u16(1);
        for _ in 0..8 {
            maxp.u16(0);
        }

        let mut hmtx = W::default();
        for _ in 0..n {
            hmtx.u16(600);
            hmtx.i16(0);
        }

        let (mut glyf, mut loca) = (W::default(), W::default());
        for g in glyphs {
            loca.u32(glyf.len() as u32);
            glyf.bytes(g);
            glyf.pad4();
        }
        loca.u32(glyf.len() as u32);

        let mut tables = vec![
            (*b"cmap", cmap),
            (*b"glyf", glyf.0),
            (*b"head", head.0),
            (*b"hhea", hhea.0),
            (*b"hmtx", hmtx.0),
            (*b"loca", loca.0),
            (*b"maxp", maxp.0),
        ];
        tables.extend(extra);
        assemble(tables)
    }

    pub fn is_refused(e: &anyhow::Error) -> bool {
        e.downcast_ref::<super::super::Refused>().is_some()
    }

    fn refusal(font: &[u8]) -> anyhow::Error {
        let face = Face::parse(font, 0).unwrap();
        let err = rebuild(&face, &[]).unwrap_err();
        assert!(is_refused(&err), "{err:#}");
        err
    }

    #[derive(Default)]
    struct PointLog(Vec<(char, f32, f32)>);

    impl ttf_parser::OutlineBuilder for PointLog {
        fn move_to(&mut self, x: f32, y: f32) {
            self.0.push(('M', x, y));
        }
        fn line_to(&mut self, x: f32, y: f32) {
            self.0.push(('L', x, y));
        }
        fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
            self.0.push(('q', x1, y1));
            self.0.push(('Q', x, y));
        }
        fn curve_to(&mut self, _: f32, _: f32, _: f32, _: f32, x: f32, y: f32) {
            self.0.push(('C', x, y));
        }
        fn close(&mut self) {
            self.0.push(('Z', 0.0, 0.0));
        }
    }

    /// `ttf-parser`'s outline of glyph 0 in `font` and in its rebuild.
    fn outline_pair(font: &[u8]) -> (Vec<(char, f32, f32)>, Vec<(char, f32, f32)>) {
        let face = Face::parse(font, 0).unwrap();
        let (rebuilt, _) = rebuild(&face, &[]).unwrap();
        let rebuilt = Face::parse(&rebuilt, 0).unwrap();
        let (mut a, mut b) = (PointLog::default(), PointLog::default());
        face.outline_glyph(GlyphId(0), &mut a);
        rebuilt.outline_glyph(GlyphId(0), &mut b);
        (a.0, b.0)
    }

    #[test]
    fn a_composite_bomb_is_refused_before_it_expands() {
        // Glyph i is two copies of glyph i+1; the last is 100 points. Flattened, glyph 0 would
        // hold 100 << 30 points.
        let mut glyphs: Vec<Vec<u8>> = (0..30u16)
            .map(|i| composite_glyph(&[i + 1, i + 1]))
            .collect();
        glyphs.push(simple_glyph(100));
        let font = fixture(&glyphs, cmap_format12(&[(0x41, 0x41, 0)]), Vec::new());
        let err = refusal(&font);
        assert!(err.to_string().contains("over"), "{err:#}");
    }

    #[test]
    fn a_fan_out_of_empty_leaves_is_refused_by_its_visits() {
        // No glyph has a point, so only the visit count can stop this: glyph i references
        // glyph i+1 sixty-four times, thirty levels deep.
        let mut glyphs: Vec<Vec<u8>> = (0..30u16).map(|i| composite_glyph(&[i + 1; 64])).collect();
        glyphs.push(empty_glyph());
        let font = fixture(&glyphs, cmap_format12(&[(0x41, 0x41, 0)]), Vec::new());
        let err = refusal(&font);
        assert!(err.to_string().contains("flattens through"), "{err:#}");
    }

    #[test]
    fn a_composite_cycle_is_refused() {
        let glyphs = vec![composite_glyph(&[1]), composite_glyph(&[0])];
        let font = fixture(&glyphs, cmap_format12(&[(0x41, 0x41, 0)]), Vec::new());
        refusal(&font);
    }

    #[test]
    fn a_loca_longer_than_maxp_is_bounded_by_maxp() {
        // maxp declares two glyphs; loca holds three, and the third is a composite of itself
        // that glyph 1 references. Sized or not, nothing past maxp may be flattened.
        let glyphs = vec![
            simple_glyph(4),
            composite_glyph(&[2]),
            composite_glyph(&[2]),
        ];
        let font = fixture_declaring(&glyphs, 2, cmap_format12(&[(0x41, 0x42, 0)]), Vec::new());
        let face = Face::parse(&font, 0).unwrap();
        assert_eq!(face.number_of_glyphs(), 2);
        let (rebuilt, _) = rebuild(&face, &[]).unwrap();
        let rebuilt = Face::parse(&rebuilt, 0).unwrap();
        assert_eq!(rebuilt.number_of_glyphs(), 2);
        let mut log = PointLog::default();
        // Glyph 1's only component is outside the font: skipped, so the glyph is empty.
        assert!(rebuilt.outline_glyph(GlyphId(1), &mut log).is_none());
    }

    #[test]
    fn a_long_chain_is_refused_before_the_stack_runs_out() {
        // 65535 composites, each referencing the next: refused at depth 32, not followed down.
        let mut glyphs: Vec<Vec<u8>> = (1..65535u16).map(|i| composite_glyph(&[i])).collect();
        glyphs.push(empty_glyph());
        let font = fixture(&glyphs, cmap_format12(&[(0x41, 0x41, 0)]), Vec::new());
        let err = refusal(&font);
        assert!(err.to_string().contains("nest"), "{err:#}");
    }

    #[test]
    fn a_chain_of_exactly_the_depth_limit_is_refused_as_ttf_parser_gives_up() {
        // 33 glyphs: glyph 0 flattens through 32 composite levels. ttf-parser stops at a depth
        // of 32, so this is the shortest chain it would draw empty.
        let mut glyphs: Vec<Vec<u8>> = (1..=32u16).map(|i| composite_glyph(&[i])).collect();
        glyphs.push(simple_glyph(4));
        let font = fixture(&glyphs, cmap_format12(&[(0x41, 0x41, 0)]), Vec::new());
        let face = Face::parse(&font, 0).unwrap();
        let mut log = PointLog::default();
        assert!(face.outline_glyph(GlyphId(0), &mut log).is_none());
        refusal(&font);
        // One level less draws, in both.
        let mut glyphs: Vec<Vec<u8>> = (1..=31u16).map(|i| composite_glyph(&[i])).collect();
        glyphs.push(simple_glyph(4));
        let font = fixture(&glyphs, cmap_format12(&[(0x41, 0x41, 0)]), Vec::new());
        let (a, b) = outline_pair(&font);
        assert!(!a.is_empty());
        assert_eq!(a, b);
    }

    #[test]
    fn the_point_cap_holds_without_the_node_cap() {
        // Fifty copies of a 200-point glyph: 51 visits, 10,000 points.
        let glyphs = vec![composite_glyph(&[1; 50]), simple_glyph(200)];
        let font = fixture(&glyphs, cmap_format12(&[(0x41, 0x41, 0)]), Vec::new());
        let err = refusal(&font);
        assert!(err.to_string().contains("expands to over"), "{err:#}");
    }

    #[test]
    fn the_font_point_cap_is_reached_before_the_output() {
        // 505 composites of one 8000-point glyph: 4.04M points, two visits each, 12 MB out.
        let mut glyphs = vec![simple_glyph(8000)];
        glyphs.extend((0..505).map(|_| composite_glyph(&[0])));
        let font = fixture(&glyphs, cmap_format12(&[(0x41, 0x41, 0)]), Vec::new());
        let err = refusal(&font);
        assert!(err.to_string().contains("font outlines expand"), "{err:#}");
    }

    #[test]
    fn the_font_node_cap_is_reached_with_no_points_at_all() {
        // 60,000 composites of seventeen empty leaves: 1.08M visits, zero points.
        let mut glyphs: Vec<Vec<u8>> = (0..60_000)
            .map(|_| composite_glyph(&[60_000; 17]))
            .collect();
        glyphs.push(empty_glyph());
        let font = fixture(&glyphs, cmap_format12(&[(0x41, 0x41, 0)]), Vec::new());
        let err = refusal(&font);
        assert!(
            err.to_string().contains("font composites flatten"),
            "{err:#}"
        );
    }

    #[test]
    fn anchored_components_consume_their_arguments_and_match_points() {
        // Component 1 is anchored (ARGS_ARE_XY_VALUES clear): its two argument bytes name the
        // parent's point 2 and its own point 0. Component 2 is offset by (10, 20). Glyph 2's
        // point 0 must land on glyph 1's point 2, and glyph 3 must still be read after it.
        let mut g = W::default();
        g.i16(-1);
        for _ in 0..4 {
            g.i16(0);
        }
        g.u16(ARGS_ARE_XY_VALUES | MORE_COMPONENTS);
        g.u16(1);
        g.u8(0);
        g.u8(0);
        g.u16(MORE_COMPONENTS); // anchored
        g.u16(2);
        g.u8(2);
        g.u8(0);
        g.u16(ARGS_ARE_XY_VALUES);
        g.u16(3);
        g.u8(10);
        g.u8(20);
        g.pad4();
        let glyphs = vec![g.0, simple_glyph(5), simple_glyph(4), simple_glyph(3)];
        let font = fixture(&glyphs, cmap_format12(&[(0x41, 0x41, 0)]), Vec::new());
        let face = Face::parse(&font, 0).unwrap();
        let (rebuilt, _) = rebuild(&face, &[]).unwrap();
        let rebuilt = Face::parse(&rebuilt, 0).unwrap();
        let mut log = PointLog::default();
        rebuilt.outline_glyph(GlyphId(0), &mut log);
        let moves: Vec<(f32, f32)> = log
            .0
            .iter()
            .filter(|p| p.0 == 'M')
            .map(|p| (p.1, p.2))
            .collect();
        // simple_glyph(n) point i is (7i mod 600, 13i mod 700): glyph 1's point 2 is (14, 26).
        assert_eq!(moves, vec![(0.0, 0.0), (14.0, 26.0), (10.0, 20.0)]);
    }

    #[test]
    fn nested_transforms_compose_like_ttf_parser() {
        // Three levels: a 2x2 at the top, an x/y scale with a negative byte offset below it,
        // a uniform scale with word offsets at the bottom. Compared against ttf-parser's own
        // walk of the original, so the order of composition is pinned.
        let mut top = W::default();
        top.i16(-1);
        for _ in 0..4 {
            top.i16(0);
        }
        top.u16(ARGS_ARE_XY_VALUES | ARG_1_AND_2_ARE_WORDS | WE_HAVE_A_TWO_BY_TWO);
        top.u16(1);
        top.i16(100);
        top.i16(-40);
        top.i16(0); // a = 0
        top.i16(16384); // b = 1
        top.i16(-16384); // c = -1
        top.i16(0); // d = 0
        top.pad4();
        let mut mid = W::default();
        mid.i16(-1);
        for _ in 0..4 {
            mid.i16(0);
        }
        mid.u16(ARGS_ARE_XY_VALUES | WE_HAVE_AN_X_AND_Y_SCALE);
        mid.u16(2);
        mid.u8((-30i8) as u8);
        mid.u8(50);
        mid.i16(8192); // 0.5
        mid.i16(-8192);
        mid.pad4();
        let mut low = W::default();
        low.i16(-1);
        for _ in 0..4 {
            low.i16(0);
        }
        low.u16(ARGS_ARE_XY_VALUES | ARG_1_AND_2_ARE_WORDS | WE_HAVE_A_SCALE);
        low.u16(3);
        low.i16(7);
        low.i16(-9);
        low.i16(24576); // 1.5
        low.pad4();
        let glyphs = vec![top.0, mid.0, low.0, simple_glyph(7)];
        let font = fixture(&glyphs, cmap_format12(&[(0x41, 0x41, 0)]), Vec::new());
        let (a, b) = outline_pair(&font);
        assert_eq!(a.len(), b.len());
        assert!(a.len() > 7);
        for (p, q) in a.iter().zip(&b) {
            assert_eq!(p.0, q.0);
            assert!(
                (p.1 - q.1).abs() <= 0.5 && (p.2 - q.2).abs() <= 0.5,
                "{p:?} vs {q:?}"
            );
        }
    }

    #[test]
    fn a_malformed_glyph_is_written_empty_not_refused() {
        // Flags that run past the glyph's data: ttf-parser cannot outline it and skips it. A
        // parent that uses it is empty too, as in ttf-parser.
        let mut bad = W::default();
        bad.i16(1);
        for _ in 0..4 {
            bad.i16(0);
        }
        bad.u16(9); // ten points
        bad.u16(0);
        bad.u8(ON_CURVE); // one flag, then nothing
        bad.pad4();
        let glyphs = vec![simple_glyph(4), bad.0, composite_glyph(&[1])];
        let font = fixture(&glyphs, cmap_format12(&[(0x41, 0x43, 0)]), Vec::new());
        let face = Face::parse(&font, 0).unwrap();
        let (rebuilt, _) = rebuild(&face, &[]).unwrap();
        let rebuilt = Face::parse(&rebuilt, 0).unwrap();
        let mut log = PointLog::default();
        assert!(rebuilt.outline_glyph(GlyphId(0), &mut log).is_some());
        assert!(rebuilt.outline_glyph(GlyphId(1), &mut log).is_none());
        assert!(rebuilt.outline_glyph(GlyphId(2), &mut log).is_none());
    }

    #[test]
    fn a_cmap_group_over_all_code_points_is_clamped() {
        let glyphs = vec![Vec::new(), simple_glyph(4)];
        let font = fixture(
            &glyphs,
            cmap_format12(&[(0x41, 0xFFFF_FFFF, 1)]),
            Vec::new(),
        );
        let face = Face::parse(&font, 0).unwrap();
        let (rebuilt, _) = rebuild(&face, &[]).unwrap();
        let rebuilt = Face::parse(&rebuilt, 0).unwrap();
        assert_eq!(rebuilt.glyph_index('A'), Some(GlyphId(1)));
        // 'B' would be glyph 2, which the font does not have.
        assert_eq!(rebuilt.glyph_index('B'), None);
    }

    #[test]
    fn cmap_ranges_past_the_budget_are_refused() {
        let glyphs = vec![Vec::new(), simple_glyph(4)];
        // Three full-range groups in one subtable: 3 × 0x110000 visits, over the budget.
        let groups: Vec<(u32, u32, u32)> = (0..3).map(|_| (0, 0x10_FFFF, 1)).collect();
        refusal(&fixture(&glyphs, cmap_format12(&groups), Vec::new()));
    }

    #[test]
    fn a_format_13_subtable_of_many_groups_is_charged_per_group() {
        // 1.3M groups that clamp to nothing cost one unit each; with one full-range group they
        // pass the budget. Under 16 MB, as an upload could be.
        let glyphs = vec![Vec::new(), simple_glyph(4)];
        let mut groups: Vec<(u32, u32, u32)> = vec![(0, 0x10_FFFF, 1)];
        groups.extend(std::iter::repeat_n((0x20_0000, 0x20_0000, 1), 1_300_000));
        let font = fixture(&glyphs, cmap_groups(13, &groups), Vec::new());
        assert!(font.len() <= MAX_FONT_BYTES);
        refusal(&font);
    }

    #[test]
    fn format_13_maps_every_code_point_of_a_group_to_one_glyph() {
        let glyphs = vec![Vec::new(), simple_glyph(4), simple_glyph(5)];
        let font = fixture(&glyphs, cmap_groups(13, &[(0x41, 0x43, 2)]), Vec::new());
        let face = Face::parse(&font, 0).unwrap();
        let map = unicode_map(&face).unwrap();
        assert_eq!(map.get(&0x41), Some(&2));
        assert_eq!(map.get(&0x43), Some(&2));
        assert_eq!(map.get(&0x44), None);
    }

    #[test]
    fn too_many_cmap_records_are_refused() {
        let glyphs = vec![Vec::new(), simple_glyph(4)];
        let mut cmap = W::default();
        cmap.u16(0);
        cmap.u16(40);
        for _ in 0..40 {
            cmap.u16(3);
            cmap.u16(10);
            cmap.u32(4 + 8 * 40);
        }
        cmap.u16(12);
        cmap.u16(0);
        cmap.u32(16);
        cmap.u32(0);
        cmap.u32(0);
        refusal(&fixture(&glyphs, cmap.0, Vec::new()));
    }

    #[test]
    fn an_oversized_rebuild_is_refused() {
        // 480 composites of one 8192-point glyph with two-byte deltas: under 100 KB in, past
        // 16 MB out.
        let mut glyphs = vec![wide_glyph(8192)];
        glyphs.extend((0..480).map(|_| composite_glyph(&[0])));
        let font = fixture(&glyphs, cmap_format12(&[(0x41, 0x41, 0)]), Vec::new());
        assert!(font.len() < 100 * 1024);
        let err = refusal(&font);
        assert!(err.to_string().contains("rebuilt font is"), "{err:#}");
    }

    #[test]
    fn the_rebuilt_file_checksums_like_an_sfnt() {
        let glyphs = vec![Vec::new(), simple_glyph(12), simple_glyph(5)];
        let font = fixture(&glyphs, cmap_format12(&[(0x41, 0x42, 1)]), Vec::new());
        let face = Face::parse(&font, 0).unwrap();
        let (rebuilt, _) = rebuild(&face, &[]).unwrap();
        assert_eq!(checksum(&rebuilt), 0xB1B0_AFBA);
        let count = be_u16(&rebuilt, 4).unwrap() as usize;
        for i in 0..count {
            let at = 12 + 16 * i;
            let tag = &rebuilt[at..at + 4];
            let sum = be_u32(&rebuilt, at + 4).unwrap();
            let off = be_u32(&rebuilt, at + 8).unwrap() as usize;
            let len = be_u32(&rebuilt, at + 12).unwrap() as usize;
            let mut data = rebuilt[off..off + len].to_vec();
            if tag == b"head" {
                data[8..12].copy_from_slice(&[0; 4]);
            }
            assert_eq!(checksum(&data), sum, "{}", String::from_utf8_lossy(tag));
        }
    }
}

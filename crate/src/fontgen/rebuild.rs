//! A scene font rewritten from validated values, so no byte of the original file reaches a client.
//!
//! A font bundle carries its font for FreeType, which draws any character outside the
//! pre-filled set at runtime, and a scene controls its own text, so it can always make that
//! happen. Embedding the uploaded file would hand FreeType untrusted bytes. Instead every table
//! is regenerated from what `ttf-parser` parsed and the limits here accepted:
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

use super::kerning::KernPair;
use super::{MAX_FONT_BYTES, MAX_GLYPH_POINTS};
use anyhow::{anyhow, bail, Result};
use std::collections::BTreeMap;
use ttf_parser::{name_id, Face, GlyphId, OutlineBuilder};

/// Outline points across every glyph of the font, composites expanded. Bounds the rebuild the
/// way [`MAX_GLYPH_POINTS`] bounds one glyph.
pub const MAX_FONT_POINTS: usize = 4_000_000;

/// Coordinates are re-encoded as 16-bit deltas, so they must stay within half the range.
const MAX_COORDINATE: i32 = 16383;

/// Characters a name record keeps.
const MAX_NAME_CHARS: usize = 63;

/// A GPOS subtable addresses its parts with 16-bit offsets.
const MAX_GPOS_BYTES: usize = 0xFFFF;

const ON_CURVE: u8 = 0x01;
const X_SHORT: u8 = 0x02;
const Y_SHORT: u8 = 0x04;
const X_SAME_OR_POSITIVE: u8 = 0x10;
const Y_SAME_OR_POSITIVE: u8 = 0x20;

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

/// A glyph's contours in font units, as `ttf-parser` walks them: `(x, y, on_curve)`.
#[derive(Default)]
struct Contours {
    contours: Vec<Vec<(f32, f32, bool)>>,
    points: usize,
    error: Option<&'static str>,
}

impl Contours {
    fn push(&mut self, x: f32, y: f32, on: bool) {
        self.points += 1;
        match self.contours.last_mut() {
            Some(c) => c.push((x, y, on)),
            None => self.error = Some("outline does not start with a move"),
        }
    }

    /// The contours as TrueType stores them: integer points. An on-curve point between two
    /// off-curve ones that is their exact midpoint and not on whole units can only have been
    /// implied in the source, so it is left implied again rather than rounded; one on whole
    /// units may have been stored, and stays.
    fn to_points(&self) -> Result<Vec<Vec<(i32, i32, bool)>>, &'static str> {
        let mut out = Vec::with_capacity(self.contours.len());
        for c in &self.contours {
            let n = c.len();
            let implied = |i: usize| {
                let (prev, cur, next) = (c[(i + n - 1) % n], c[i], c[(i + 1) % n]);
                n > 2
                    && cur.2
                    && !prev.2
                    && !next.2
                    && cur.0 * 2.0 == prev.0 + next.0
                    && cur.1 * 2.0 == prev.1 + next.1
                    && (cur.0.fract() != 0.0 || cur.1.fract() != 0.0)
            };
            let mut contour = Vec::with_capacity(n);
            for i in 0..n {
                if implied(i) {
                    continue;
                }
                let (x, y, on) = c[i];
                let (x, y) = (x.round() as i32, y.round() as i32);
                if x.abs() > MAX_COORDINATE || y.abs() > MAX_COORDINATE {
                    return Err("coordinate out of range");
                }
                contour.push((x, y, on));
            }
            if !contour.is_empty() {
                out.push(contour);
            }
        }
        Ok(out)
    }
}

impl OutlineBuilder for Contours {
    fn move_to(&mut self, x: f32, y: f32) {
        self.contours.push(Vec::new());
        self.push(x, y, true);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.push(x, y, true);
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.push(x1, y1, false);
        self.push(x, y, true);
    }
    fn curve_to(&mut self, _: f32, _: f32, _: f32, _: f32, _: f32, _: f32) {
        self.error = Some("cubic curve in a TrueType outline");
    }
    fn close(&mut self) {
        // TrueType contours close on their own: drop the point that repeats the start.
        if let Some(c) = self.contours.last_mut() {
            if c.len() > 1 && c.first().map(|p| (p.0, p.1)) == c.last().map(|p| (p.0, p.1)) {
                c.pop();
            }
        }
    }
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
    let mut total = 0usize;
    (0..face.number_of_glyphs())
        .map(|gid| {
            let gid = GlyphId(gid);
            let mut c = Contours::default();
            face.outline_glyph(gid, &mut c);
            if c.points > MAX_GLYPH_POINTS {
                bail!(
                    "glyph {} has {} outline points, over {MAX_GLYPH_POINTS}",
                    gid.0,
                    c.points
                );
            }
            total += c.points;
            if total > MAX_FONT_POINTS {
                bail!("font outlines exceed {MAX_FONT_POINTS} points");
            }
            if let Some(e) = c.error {
                bail!("glyph {}: {e}", gid.0);
            }
            let contours = c.to_points().map_err(|e| anyhow!("glyph {}: {e}", gid.0))?;
            let (data, bbox) = encode_glyph(&contours);
            Ok(GlyphRecord {
                data,
                bbox,
                advance: face.glyph_hor_advance(gid).unwrap_or(0),
                points: contours.iter().map(Vec::len).sum(),
                contours: contours.len(),
            })
        })
        .collect()
}

/// The Unicode mappings the face resolves, as `ttf-parser` picks among its subtables.
fn unicode_map(face: &Face<'_>) -> BTreeMap<u32, u16> {
    let mut codepoints = Vec::new();
    if let Some(cmap) = face.tables().cmap {
        for subtable in cmap.subtables.into_iter().filter(|s| s.is_unicode()) {
            subtable.codepoints(|cp| codepoints.push(cp));
        }
    }
    let glyphs = face.number_of_glyphs();
    codepoints
        .into_iter()
        .filter_map(|cp| {
            let gid = face.glyph_index(char::from_u32(cp)?)?;
            (gid.0 != 0 && gid.0 < glyphs).then_some((cp, gid.0))
        })
        .collect()
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
/// scripts. Pairs that do not fit the subtable's 16-bit offsets are dropped smallest first.
fn gpos_table(pairs: &[KernPair]) -> Option<Vec<u8>> {
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
    (w.len() <= MAX_GPOS_BYTES).then_some(w.0)
}

fn checksum(data: &[u8]) -> u32 {
    data.chunks(4).fold(0u32, |sum, c| {
        let mut b = [0u8; 4];
        b[..c.len()].copy_from_slice(c);
        sum.wrapping_add(u32::from_be_bytes(b))
    })
}

/// Rewrites `face` as a TrueType file of regenerated tables, kerned by `pairs`.
pub fn rebuild(face: &Face<'_>, pairs: &[KernPair]) -> Result<Vec<u8>> {
    let glyphs = glyph_records(face)?;
    let num_glyphs = glyphs.len();
    if num_glyphs == 0 {
        bail!("font has no glyphs");
    }
    let upem = face.units_per_em();
    let map = unicode_map(face);

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
    head.u32(0); // checkSumAdjustment, patched
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
    if let Some(gpos) = gpos_table(pairs) {
        out_tables.push((*b"GPOS", gpos));
    }
    out_tables.sort_by_key(|t| t.0);

    let n = out_tables.len() as u16;
    let search = 16 * (1u16 << (n as f64).log2().floor() as u16);
    let mut font = W::default();
    font.u32(0x0001_0000);
    font.u16(n);
    font.u16(search);
    font.u16((search / 16).trailing_zeros() as u16);
    font.u16(n * 16 - search);
    let mut offset = 12 + 16 * out_tables.len();
    let mut head_at = 0usize;
    for (tag, data) in &out_tables {
        font.bytes(tag);
        font.u32(checksum(data));
        font.u32(offset as u32);
        font.u32(data.len() as u32);
        if tag == b"head" {
            head_at = offset;
        }
        offset += data.len().div_ceil(4) * 4;
    }
    for (_, data) in &out_tables {
        font.bytes(data);
        font.pad4();
    }
    let adjustment = 0xB1B0_AFBAu32.wrapping_sub(checksum(&font.0));
    font.0[head_at + 8..head_at + 12].copy_from_slice(&adjustment.to_be_bytes());

    if font.len() > MAX_FONT_BYTES {
        return Err(anyhow!(
            "rebuilt font is {} bytes, over the {MAX_FONT_BYTES}-byte cap",
            font.len()
        ));
    }
    Ok(font.0)
}

#[cfg(test)]
mod tests {
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
}

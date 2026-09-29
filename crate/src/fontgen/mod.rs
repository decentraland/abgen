//! Scene fonts baked into TextMeshPro / UI Toolkit dynamic font assets.
//!
//! The explorer's `font_src` loader builds its runtime font assets with
//! `CreateFontAsset(path, 0, 90, 9, SDFAA, 1024, 1024)` and lets FreeType draw each glyph on the
//! main thread the first time a string needs it. A font bundle ships the same asset already
//! built, with its atlas pre-filled with the characters scene text uses most, so common text
//! renders on the first frame. The asset stays dynamic and carries the source font, so any
//! character outside the pre-filled set is still added at runtime exactly as before.
//!
//! Everything here mirrors what TextCore writes for those settings, fitted against assets Unity
//! 6000.5 created itself: [`FaceInfo`] follows FreeType's face metrics, glyph metrics are the
//! 26.6 control box, glyph rects and packing slots use TextCore's padding convention, and the
//! atlas holds the [`sdf`] field TextCore's `SDFAA` mode renders.

pub mod sdf;

use anyhow::{anyhow, bail, Result};
use ttf_parser::{name_id, Face, GlyphId};

/// The explorer's `RuntimeFontAssetFactory` settings; a bundle must match them or its assets
/// would not render like the runtime-created ones they replace.
pub const SAMPLING_POINT_SIZE: u32 = 90;
pub const ATLAS_PADDING: u32 = 9;
pub const ATLAS_SIZE: u32 = 1024;
/// `GlyphRenderMode.SDFAA`.
pub const RENDER_MODE: i64 = 4165;

/// TextCore keeps one texel between SDF glyph slots and off the atlas' far edges.
const PACKING_MODIFIER: u32 = 1;
/// The material's `_GradientScale` for an SDF atlas.
pub const GRADIENT_SCALE: u32 = ATLAS_PADDING + PACKING_MODIFIER;

/// Unity's legacy `Font` object reports its metrics at this size.
const LEGACY_FONT_SIZE: f64 = 16.0;

/// Pre-filled in this order until the list ends or the atlas is full. ASCII first, then the
/// punctuation word processors substitute into ASCII text, then the Latin-1 letters and marks
/// of the languages Decentraland scenes are written in. Anything else is added at runtime.
const PRIORITY_EXTRA: &str = "’‘“”–—…•€\
áéíóúñÁÉÍÓÚÑüÜ¿¡\
ãõçâêôàÃÕÇÂÊÔÀ\
èìòùëïîûÈÌÒÙËÏÎÛ\
äöÄÖß\
«»°©®™·×÷";

pub fn priority_characters() -> Vec<char> {
    (0x20u8..=0x7e)
        .map(char::from)
        .chain(PRIORITY_EXTRA.chars())
        .collect()
}

/// `UnityEngine.TextCore.FaceInfo`, at [`SAMPLING_POINT_SIZE`].
#[derive(Clone, Debug, PartialEq)]
pub struct FaceInfo {
    pub family_name: String,
    pub style_name: String,
    pub units_per_em: i64,
    pub line_height: f64,
    pub ascent_line: f64,
    pub cap_line: f64,
    pub mean_line: f64,
    pub descent_line: f64,
    pub underline_offset: f64,
    pub underline_thickness: f64,
    pub strikethrough_offset: f64,
    pub tab_width: f64,
}

/// The metrics Unity's legacy `Font` object carries next to the font data.
#[derive(Clone, Debug, PartialEq)]
pub struct LegacyMetrics {
    pub ascent: f64,
    pub descent: f64,
    pub line_spacing: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GlyphRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Glyph {
    pub index: u32,
    pub width: f64,
    pub height: f64,
    pub bearing_x: f64,
    pub bearing_y: f64,
    pub advance: f64,
    pub rect: GlyphRect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Character {
    pub unicode: u32,
    pub glyph_index: u32,
}

/// A font asset's dynamic data after pre-filling its first atlas.
pub struct BakedFont {
    pub face: FaceInfo,
    pub legacy: LegacyMetrics,
    pub glyphs: Vec<Glyph>,
    pub characters: Vec<Character>,
    pub used_rects: Vec<GlyphRect>,
    pub free_rects: Vec<GlyphRect>,
    /// `ATLAS_SIZE`² Alpha8 texels, bottom row first.
    pub atlas: Vec<u8>,
}

/// An sfnt container FreeType reads: TrueType, OpenType/CFF, or Apple's `true`. Collections
/// and web fonts are left to the runtime loader, which rejects them the same way.
pub fn is_font_file(bytes: &[u8]) -> bool {
    matches!(
        bytes.get(..4),
        Some([0x00, 0x01, 0x00, 0x00]) | Some(b"OTTO") | Some(b"true")
    )
}

/// `.ttf` / `.otf`, the extensions a scene's `font_src` may name.
pub fn is_font_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".ttf") || lower.ends_with(".otf")
}

/// Whether [`bake`] can take this font: an sfnt that parses and maps at least one of the
/// pre-filled characters. Cheap enough to gate a conversion on.
pub fn is_supported(bytes: &[u8]) -> bool {
    is_font_file(bytes)
        && Face::parse(bytes, 0).is_ok_and(|face| {
            face.units_per_em() > 0
                && priority_characters()
                    .into_iter()
                    .any(|c| face.glyph_index(c).is_some())
        })
}

struct Prepared {
    glyph: Glyph,
    outline: Option<sdf::Outline>,
}

pub fn bake(bytes: &[u8]) -> Result<BakedFont> {
    if !is_font_file(bytes) {
        bail!("not a TrueType or OpenType font");
    }
    let face = Face::parse(bytes, 0).map_err(|e| anyhow!("font does not parse: {e}"))?;
    let upem = face.units_per_em() as f64;
    if upem <= 0.0 {
        bail!("font declares no units per em");
    }
    let scale = SAMPLING_POINT_SIZE as f64 / upem;
    let outline_scale = sdf::freetype_scale(SAMPLING_POINT_SIZE, face.units_per_em());

    let face_info = face_info(&face, scale, outline_scale);
    let legacy = LegacyMetrics {
        ascent: face.ascender() as f64 * LEGACY_FONT_SIZE / upem,
        descent: face.descender() as f64 * LEGACY_FONT_SIZE / upem,
        line_spacing: (face.ascender() as f64 - face.descender() as f64 + face.line_gap() as f64)
            * LEGACY_FONT_SIZE
            / upem,
    };

    let mut characters = Vec::new();
    let mut prepared: Vec<Prepared> = Vec::new();
    for c in priority_characters() {
        let Some(gid) = face.glyph_index(c) else {
            continue;
        };
        characters.push(Character {
            unicode: c as u32,
            glyph_index: gid.0 as u32,
        });
        if prepared.iter().any(|p| p.glyph.index == gid.0 as u32) {
            continue;
        }
        prepared.push(prepare_glyph(&face, gid, scale, outline_scale));
    }
    if characters.is_empty() {
        bail!("font maps none of the pre-filled characters");
    }

    let slots = pack_prefix(&prepared);
    let kept = slots.len();
    let mut atlas = vec![0u8; (ATLAS_SIZE * ATLAS_SIZE) as usize];
    let mut glyphs = Vec::with_capacity(kept);
    let mut used_rects = Vec::new();
    let fields: Vec<Option<(Vec<u8>, u32, u32)>> = {
        use rayon::prelude::*;
        prepared[..kept]
            .par_iter()
            .map(|p| {
                p.outline
                    .as_ref()
                    .map(|o| sdf::render(o, ATLAS_PADDING, GRADIENT_SCALE as f64))
            })
            .collect()
    };
    for ((p, slot), field) in prepared[..kept].iter().zip(&slots).zip(fields) {
        let mut glyph = p.glyph.clone();
        if let (Some(slot), Some((field, fw, fh))) = (slot, field) {
            glyph.rect = GlyphRect {
                x: slot.x + GRADIENT_SCALE as i32,
                y: slot.y + GRADIENT_SCALE as i32,
                width: glyph.rect.width,
                height: glyph.rect.height,
            };
            blit(
                &mut atlas,
                &field,
                fw,
                fh,
                slot.x as u32 + 1,
                slot.y as u32 + 1,
            );
            used_rects.push(*slot);
        }
        glyphs.push(glyph);
    }

    let kept_indices: Vec<u32> = glyphs.iter().map(|g| g.index).collect();
    characters.retain(|c| kept_indices.contains(&c.glyph_index));
    let free_rects = free_rects(&used_rects);

    Ok(BakedFont {
        face: face_info,
        legacy,
        glyphs,
        characters,
        used_rects,
        free_rects,
        atlas,
    })
}

fn face_info(face: &Face<'_>, scale: f64, outline_scale: i64) -> FaceInfo {
    let ascent = face.ascender() as f64;
    let descent = face.descender() as f64;
    let line_gap = face.line_gap() as f64;
    let underline = face.underline_metrics();
    let (ul_pos, ul_thick) = underline
        .map(|m| (m.position as f64, m.thickness as f64))
        .unwrap_or((0.0, 0.0));

    let glyph_top = |c: char, fallback: Option<i16>| -> f64 {
        face.glyph_index(c)
            .and_then(|g| {
                let mut o = sdf::Outline::new(outline_scale);
                face.outline_glyph(g, &mut o)
                    .filter(|_| !o.is_empty())
                    .map(|_| o.y_max)
            })
            .or_else(|| fallback.map(|v| v as f64 * scale))
            .unwrap_or(0.0)
            .round()
    };
    let cap_line = glyph_top('H', face.capital_height());
    let mean_line = glyph_top('x', face.x_height());

    let space_advance = face
        .glyph_index(' ')
        .and_then(|g| face.glyph_hor_advance(g))
        .map(|a| a as f64 * scale)
        .unwrap_or(0.0);

    FaceInfo {
        family_name: best_name(face, name_id::TYPOGRAPHIC_FAMILY, name_id::FAMILY),
        style_name: best_name(face, name_id::TYPOGRAPHIC_SUBFAMILY, name_id::SUBFAMILY),
        units_per_em: face.units_per_em() as i64,
        line_height: (ascent - descent + line_gap) * scale,
        ascent_line: ascent * scale,
        cap_line,
        mean_line,
        descent_line: descent * scale,
        // FreeType reports the underline's top edge; `post` stores its centre.
        underline_offset: (ul_pos - ul_thick / 2.0) * scale,
        underline_thickness: ul_thick * scale,
        strikethrough_offset: mean_line / 2.5,
        tab_width: space_advance.round(),
    }
}

/// The typographic name when the font has one, else the legacy family/subfamily pair.
fn best_name(face: &Face<'_>, preferred: u16, fallback: u16) -> String {
    let read = |id: u16| {
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
            .filter(|s| !s.is_empty())
    };
    read(preferred)
        .or_else(|| read(fallback))
        .unwrap_or_default()
}

fn prepare_glyph(face: &Face<'_>, gid: GlyphId, scale: f64, outline_scale: i64) -> Prepared {
    let advance = face.glyph_hor_advance(gid).unwrap_or(0) as f64 * scale;
    let mut outline = sdf::Outline::new(outline_scale);
    let drawn = face.outline_glyph(gid, &mut outline).is_some() && !outline.is_empty();
    if !drawn {
        return Prepared {
            glyph: Glyph {
                index: gid.0 as u32,
                width: 0.0,
                height: 0.0,
                bearing_x: 0.0,
                bearing_y: 0.0,
                advance,
                rect: GlyphRect::default(),
            },
            outline: None,
        };
    }
    let (_, _, w, h) = sdf::bitmap_box(&outline);
    Prepared {
        glyph: Glyph {
            index: gid.0 as u32,
            width: outline.x_max - outline.x_min,
            height: outline.y_max - outline.y_min,
            bearing_x: outline.x_min,
            bearing_y: outline.y_max,
            advance,
            rect: GlyphRect {
                x: 0,
                y: 0,
                width: w as i32,
                height: h as i32,
            },
        },
        outline: Some(outline),
    }
}

/// A glyph's slot: its bitmap plus the padding on both sides and TextCore's one-texel gap.
fn slot_size(g: &Glyph) -> (i32, i32) {
    let extra = (2 * ATLAS_PADDING + PACKING_MODIFIER) as i32;
    (g.rect.width + extra, g.rect.height + extra)
}

/// TextCore never packs into the last row or column.
const PACK_LIMIT: i32 = (ATLAS_SIZE - PACKING_MODIFIER) as i32;

/// Shelf-packs `glyphs` tallest first. `None` when they do not all fit.
fn shelf_pack(glyphs: &[&Glyph]) -> Option<Vec<Option<GlyphRect>>> {
    let mut order: Vec<usize> = (0..glyphs.len())
        .filter(|&i| glyphs[i].rect.width > 0)
        .collect();
    order.sort_by_key(|&i| (std::cmp::Reverse(glyphs[i].rect.height), glyphs[i].index));
    let mut slots = vec![None; glyphs.len()];
    let (mut x, mut y, mut shelf_h) = (0i32, 0i32, 0i32);
    for i in order {
        let (w, h) = slot_size(glyphs[i]);
        if w > PACK_LIMIT {
            return None;
        }
        if x + w > PACK_LIMIT {
            y += shelf_h;
            x = 0;
            shelf_h = 0;
        }
        if y + h > PACK_LIMIT {
            return None;
        }
        slots[i] = Some(GlyphRect {
            x,
            y,
            width: w,
            height: h,
        });
        x += w;
        shelf_h = shelf_h.max(h);
    }
    Some(slots)
}

/// Packs the longest prefix of the priority-ordered glyphs that fits in one atlas.
fn pack_prefix(prepared: &[Prepared]) -> Vec<Option<GlyphRect>> {
    let glyphs: Vec<&Glyph> = prepared.iter().map(|p| &p.glyph).collect();
    if let Some(all) = shelf_pack(&glyphs) {
        return all;
    }
    let (mut lo, mut hi) = (0usize, glyphs.len());
    while lo + 1 < hi {
        let mid = (lo + hi) / 2;
        if shelf_pack(&glyphs[..mid]).is_some() {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    shelf_pack(&glyphs[..lo]).unwrap_or_default()
}

/// The atlas area no glyph slot touches, as rectangles TextCore's packer can keep filling at
/// runtime: the gap above each slot shorter than its shelf, the rest of each shelf, and
/// everything above the last shelf.
fn free_rects(used: &[GlyphRect]) -> Vec<GlyphRect> {
    let mut shelves: Vec<(i32, i32, i32)> = Vec::new();
    for r in used {
        match shelves.iter_mut().find(|s| s.0 == r.y) {
            Some(s) => {
                s.1 = s.1.max(r.height);
                s.2 = s.2.max(r.x + r.width);
            }
            None => shelves.push((r.y, r.height, r.x + r.width)),
        }
    }
    let mut free = Vec::new();
    for r in used {
        let shelf_h = shelves
            .iter()
            .find(|s| s.0 == r.y)
            .map_or(r.height, |s| s.1);
        if r.height < shelf_h {
            free.push(GlyphRect {
                x: r.x,
                y: r.y + r.height,
                width: r.width,
                height: shelf_h - r.height,
            });
        }
    }
    let mut top = 0;
    for &(y, h, right) in &shelves {
        if right < PACK_LIMIT {
            free.push(GlyphRect {
                x: right,
                y,
                width: PACK_LIMIT - right,
                height: h,
            });
        }
        top = top.max(y + h);
    }
    if top < PACK_LIMIT {
        free.push(GlyphRect {
            x: 0,
            y: top,
            width: PACK_LIMIT,
            height: PACK_LIMIT - top,
        });
    }
    free
}

fn blit(atlas: &mut [u8], field: &[u8], fw: u32, fh: u32, x0: u32, y0: u32) {
    for j in 0..fh {
        let dst = ((y0 + j) * ATLAS_SIZE + x0) as usize;
        let src = (j * fw) as usize;
        atlas[dst..dst + fw as usize].copy_from_slice(&field[src..src + fw as usize]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> GlyphRect {
        GlyphRect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    fn overlaps(a: &GlyphRect, b: &GlyphRect) -> bool {
        a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
    }

    #[test]
    fn free_rects_never_overlap_used_slots() {
        let used = vec![rect(0, 0, 40, 90), rect(40, 0, 30, 70), rect(0, 90, 50, 60)];
        let free = free_rects(&used);
        for f in &free {
            assert!(f.width > 0 && f.height > 0);
            assert!(f.x + f.width <= PACK_LIMIT && f.y + f.height <= PACK_LIMIT);
            for u in &used {
                assert!(!overlaps(f, u), "{f:?} overlaps {u:?}");
            }
        }
        assert!(free.contains(&rect(40, 70, 30, 20)));
        assert!(free.contains(&rect(0, 150, PACK_LIMIT, PACK_LIMIT - 150)));
    }

    #[test]
    fn font_magic() {
        assert!(is_font_file(&[0, 1, 0, 0, 9]));
        assert!(is_font_file(b"OTTO...."));
        assert!(!is_font_file(b"wOF2...."));
        assert!(!is_font_file(b"glTF"));
    }

    #[test]
    fn priority_starts_with_ascii_and_has_no_duplicates() {
        let chars = priority_characters();
        assert_eq!(chars[0], ' ');
        assert_eq!(chars[94], '~');
        let mut seen = std::collections::HashSet::new();
        assert!(chars.iter().all(|c| seen.insert(*c)));
    }
}

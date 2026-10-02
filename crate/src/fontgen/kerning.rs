//! Kerning pairs read from a scene font, for the glyphs a bundle pre-fills.
//!
//! TextMeshPro kerns from the font's GPOS `kern` feature: it reads pair adjustments for every
//! glyph it adds at runtime, and a pre-filled asset has to carry the records for the glyphs it
//! ships with. The rebuilt font keeps only the pairs read here (see `rebuild`), so the original
//! GPOS table, one of the most intricate structures a font parser walks, never reaches a client.

use ttf_parser::gpos::{PairAdjustment, PositioningSubtable};
use ttf_parser::{Face, GlyphId, Tag};

/// Lookup subtables a font may make the reader visit. Every pair query walks each one, so this
/// bounds the work a hostile font can ask for; a font past it ships without kerning.
const MAX_KERN_SUBTABLES: usize = 256;

/// A horizontal kerning pair: `x_advance` font units added after `first` when `second` follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KernPair {
    pub first: u16,
    pub second: u16,
    pub x_advance: i16,
}

/// The pairs among `glyphs` the font kerns, sorted by glyph. GPOS `kern` lookups win; a font
/// without them falls back to the legacy `kern` table. Pairs whose adjustment sums to zero are
/// dropped.
pub fn pairs(face: &Face<'_>, glyphs: &[GlyphId]) -> Vec<KernPair> {
    let mut glyphs = glyphs.to_vec();
    glyphs.sort_by_key(|g| g.0);
    glyphs.dedup();

    let mut out = gpos_pairs(face, &glyphs).unwrap_or_else(|| kern_table_pairs(face, &glyphs));
    out.retain(|p| p.x_advance != 0);
    out
}

/// `None` when the font has no GPOS `kern` lookups to read, or more of them than the reader
/// will visit.
fn gpos_pairs(face: &Face<'_>, glyphs: &[GlyphId]) -> Option<Vec<KernPair>> {
    let gpos = face.tables().gpos?;
    let kern = Tag::from_bytes(b"kern");
    let mut lookups: Vec<u16> = gpos
        .features
        .into_iter()
        .filter(|f| f.tag == kern)
        .flat_map(|f| f.lookup_indices.into_iter())
        .collect();
    lookups.sort_unstable();
    lookups.dedup();
    if lookups.is_empty() {
        return None;
    }

    // Lookups apply in order and add up; within one, the first subtable covering the pair
    // decides it.
    let mut subtables: Vec<Vec<PairAdjustment<'_>>> = Vec::new();
    let mut visited = 0usize;
    for index in lookups {
        let lookup = gpos.lookups.get(index)?;
        let mut pairs = Vec::new();
        for subtable in lookup.subtables.into_iter::<PositioningSubtable>() {
            visited += 1;
            if visited > MAX_KERN_SUBTABLES {
                return None;
            }
            if let PositioningSubtable::Pair(p) = subtable {
                pairs.push(p);
            }
        }
        subtables.push(pairs);
    }

    let mut out = Vec::new();
    for &first in glyphs {
        for &second in glyphs {
            let mut x_advance = 0i32;
            for lookup in &subtables {
                if let Some(v) = lookup.iter().find_map(|p| pair_value(p, first, second)) {
                    x_advance += v as i32;
                }
            }
            out.push(KernPair {
                first: first.0,
                second: second.0,
                x_advance: x_advance.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            });
        }
    }
    Some(out)
}

/// The first glyph's advance adjustment when this subtable covers the pair. A class-based
/// subtable covers every pair whose first glyph it covers, even with a zero value.
fn pair_value(p: &PairAdjustment<'_>, first: GlyphId, second: GlyphId) -> Option<i16> {
    match p {
        PairAdjustment::Format1 { coverage, sets } => {
            let set = sets.get(coverage.get(first)?)?;
            set.get(second).map(|(a, _)| a.x_advance)
        }
        PairAdjustment::Format2 {
            coverage,
            classes,
            matrix,
        } => {
            coverage.get(first)?;
            let value = matrix.get((classes.0.get(first), classes.1.get(second)));
            Some(value.map_or(0, |(a, _)| a.x_advance))
        }
    }
}

fn kern_table_pairs(face: &Face<'_>, glyphs: &[GlyphId]) -> Vec<KernPair> {
    let Some(kern) = face.tables().kern else {
        return Vec::new();
    };
    let subtables: Vec<_> = kern
        .subtables
        .into_iter()
        .filter(|s| s.horizontal && !s.variable && !s.has_cross_stream && !s.has_state_machine)
        .take(MAX_KERN_SUBTABLES)
        .collect();
    let mut out = Vec::new();
    for &first in glyphs {
        for &second in glyphs {
            let x_advance: i32 = subtables
                .iter()
                .filter_map(|s| s.glyphs_kerning(first, second))
                .map(i32::from)
                .sum();
            out.push(KernPair {
                first: first.0,
                second: second.0,
                x_advance: x_advance.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            });
        }
    }
    out
}

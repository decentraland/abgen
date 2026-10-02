//! Kerning pairs read from a scene font, for the glyphs a bundle pre-fills.
//!
//! TextMeshPro kerns from the font's GPOS `kern` feature: it reads pair adjustments for every
//! glyph it adds at runtime, and a pre-filled asset has to carry the records for the glyphs it
//! ships with. The rebuilt font keeps only the pairs read here (see `rebuild`), so the original
//! GPOS table, one of the most intricate structures a font parser walks, never reaches a client.
//!
//! The reader is bounded before it reads: a font whose `kern` feature points at more lookup
//! indices, lookups or subtables than the caps allow ships without kerning rather than being
//! walked.

use ttf_parser::gpos::{PairAdjustment, PositioningSubtable};
use ttf_parser::{Face, GlyphId, Tag};

/// Lookups plus subtables the reader visits. Every pair query walks each subtable, so this bounds
/// the work a hostile font can ask for.
pub const MAX_KERN_TABLES: usize = 256;

/// Lookup indices the `kern` feature records may name in total. Feature records are six bytes
/// and may all point at one feature of 65535 indices, so this is counted, not collected.
pub const MAX_KERN_FEATURE_INDICES: usize = 65536;

/// A horizontal kerning pair: `x_advance` font units added after `first` when `second` follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KernPair {
    pub first: u16,
    pub second: u16,
    pub x_advance: i16,
}

/// The pairs among `glyphs` the font kerns, sorted by glyph. A GPOS `kern` feature wins; a font
/// without one falls back to the legacy `kern` table. A GPOS past the caps, or one `ttf-parser`
/// cannot follow, yields no pairs at all. Pairs whose adjustment sums to zero are dropped.
pub fn pairs(face: &Face<'_>, glyphs: &[GlyphId]) -> Vec<KernPair> {
    let mut glyphs = glyphs.to_vec();
    glyphs.sort_by_key(|g| g.0);
    glyphs.dedup();

    let mut out = gpos_pairs(face, &glyphs).unwrap_or_else(|| kern_table_pairs(face, &glyphs));
    out.retain(|p| p.x_advance != 0);
    out
}

/// `None` when the font has no GPOS `kern` feature to read. An empty list when it has one the
/// reader will not walk: over a cap, or with a lookup index the table does not resolve.
fn gpos_pairs(face: &Face<'_>, glyphs: &[GlyphId]) -> Option<Vec<KernPair>> {
    let gpos = face.tables().gpos?;
    let kern = Tag::from_bytes(b"kern");
    let mut wanted = [0u64; 1024];
    let mut any = false;
    let mut named = 0usize;
    for feature in gpos.features.into_iter().filter(|f| f.tag == kern) {
        any = true;
        // Indexed, not iterated: ttf-parser's array iterator keeps a u16 position that
        // overflows when a 65535-entry list is walked to its end.
        for i in 0..feature.lookup_indices.len() {
            let Some(index) = feature.lookup_indices.get(i) else {
                break;
            };
            named += 1;
            if named > MAX_KERN_FEATURE_INDICES {
                return Some(Vec::new());
            }
            wanted[index as usize / 64] |= 1 << (index % 64);
        }
    }
    if !any {
        return None;
    }

    // Lookups apply in order and add up; within one, the first subtable covering the pair
    // decides it.
    let mut subtables: Vec<Vec<PairAdjustment<'_>>> = Vec::new();
    let mut visited = 0usize;
    for index in (0..=u16::MAX).filter(|&i| wanted[i as usize / 64] & (1 << (i % 64)) != 0) {
        visited += 1;
        if visited > MAX_KERN_TABLES {
            return Some(Vec::new());
        }
        let Some(lookup) = gpos.lookups.get(index) else {
            return Some(Vec::new());
        };
        let mut pairs = Vec::new();
        for subtable in lookup.subtables.into_iter::<PositioningSubtable>() {
            visited += 1;
            if visited > MAX_KERN_TABLES {
                return Some(Vec::new());
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
        .take(MAX_KERN_TABLES)
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

#[cfg(test)]
mod tests {
    use super::super::rebuild::tests::{cmap_format12, fixture, simple_glyph};
    use super::*;

    /// A GPOS whose `kern` feature list holds `records` entries all pointing at one feature
    /// naming lookup 0 65535 times, over one pair lookup kerning glyph 1 before glyph 2 by -50.
    fn gpos_feature_overlay(records: usize) -> Vec<u8> {
        let mut w: Vec<u8> = Vec::new();
        let u16 = |w: &mut Vec<u8>, v: u16| w.extend_from_slice(&v.to_be_bytes());
        // Header: ScriptList at 10, LookupList at 30, FeatureList after it.
        u16(&mut w, 1);
        u16(&mut w, 0);
        u16(&mut w, 10);
        u16(&mut w, 0); // FeatureList, patched
        u16(&mut w, 30);
        // ScriptList: DFLT -> Script -> default LangSys running feature 0.
        u16(&mut w, 1);
        w.extend_from_slice(b"DFLT");
        u16(&mut w, 8);
        u16(&mut w, 4);
        u16(&mut w, 0);
        u16(&mut w, 0);
        u16(&mut w, 0xFFFF);
        u16(&mut w, 1);
        u16(&mut w, 0);
        assert_eq!(w.len(), 30);
        // LookupList: one pair lookup with a PairPosFormat1 subtable for glyph 1 -> glyph 2.
        u16(&mut w, 1);
        u16(&mut w, 4);
        u16(&mut w, 2); // Lookup: pair adjustment
        u16(&mut w, 0);
        u16(&mut w, 1);
        u16(&mut w, 8);
        // PairPosFormat1, offsets relative to its start.
        u16(&mut w, 1);
        u16(&mut w, 18); // coverage, after the 12-byte header and the 6-byte pair set
        u16(&mut w, 0x0004); // valueFormat1: XAdvance
        u16(&mut w, 0);
        u16(&mut w, 1);
        u16(&mut w, 12); // pair set
        u16(&mut w, 1); // pair set: one record
        u16(&mut w, 2);
        w.extend_from_slice(&(-50i16).to_be_bytes());
        u16(&mut w, 1); // coverage format 1: glyph 1
        u16(&mut w, 1);
        u16(&mut w, 1);
        let feature_list = w.len();
        w[6..8].copy_from_slice(&(feature_list as u16).to_be_bytes());
        // FeatureList: every record is `kern` at the same feature.
        u16(&mut w, records as u16);
        let feature = 2 + 6 * records;
        for _ in 0..records {
            w.extend_from_slice(b"kern");
            u16(&mut w, feature as u16);
        }
        u16(&mut w, 0);
        u16(&mut w, 65535);
        for _ in 0..65535 {
            u16(&mut w, 0);
        }
        w
    }

    fn overlay_font(records: usize) -> Vec<u8> {
        let glyphs = vec![Vec::new(), simple_glyph(4), simple_glyph(4)];
        fixture(
            &glyphs,
            cmap_format12(&[(0x41, 0x42, 1)]),
            vec![(*b"GPOS", gpos_feature_overlay(records))],
        )
    }

    #[test]
    fn one_feature_of_every_index_is_read_and_kerns() {
        // 65535 indices named once stays under the budget, and the pair lookup is applied.
        let font = overlay_font(1);
        let face = Face::parse(&font, 0).unwrap();
        let read = pairs(&face, &[GlyphId(1), GlyphId(2)]);
        assert_eq!(
            read,
            vec![KernPair {
                first: 1,
                second: 2,
                x_advance: -50
            }]
        );
    }

    #[test]
    fn a_feature_overlay_ships_without_kerning_instead_of_being_walked() {
        // The same lookup, named 2000 × 65535 times: past the budget, so no kerning at all.
        let font = overlay_font(2000);
        let face = Face::parse(&font, 0).unwrap();
        assert!(face.tables().gpos.is_some());
        assert!(pairs(&face, &[GlyphId(1), GlyphId(2)]).is_empty());
    }

    #[test]
    fn a_font_without_gpos_or_kern_has_no_pairs() {
        let glyphs = vec![Vec::new(), simple_glyph(4)];
        let font = fixture(&glyphs, cmap_format12(&[(0x41, 0x41, 1)]), Vec::new());
        let face = Face::parse(&font, 0).unwrap();
        assert!(pairs(&face, &[GlyphId(1)]).is_empty());
    }
}

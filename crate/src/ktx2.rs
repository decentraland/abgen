//! KTX2 ingest for `KHR_texture_basisu`: a Basis Universal UASTC image, transcoded to the
//! RGBA8 every encoder downstream already takes.
//!
//! Only the shape the Decentraland explorer ships is taken, gated by [`ktx2_header_ok`] before
//! the transcoder reads anything: a single 2D UASTC LDR image (`vkFormat` undefined, stored
//! plain or Zstandard-supercompressed) no larger than [`KTX2_MAX_SIZE`] on a side. ETC1S
//! (BasisLZ), UASTC HDR, XUASTC, cubemaps, arrays and volumes are rejected, so they decode to
//! `None` exactly as an unreadable PNG does today. The vendored transcoder
//! (`crate/third_party/basisu`, UASTC only) bounds every size and offset it reads from the
//! container; this gate keeps the accepted formats and sizes to what the explorer takes.
//!
//! **Which level.** The decoded level is the first one no larger than [`KTX2_DECODE_MAX`] on a
//! side: level 0 for every file within that cap (the full-resolution picture, the same one a
//! PNG of it would decode to), otherwise the file's own smaller mip. The transcoder only
//! produces whole levels (and Zstandard-decompresses a whole level first), so the level picked
//! is what bounds memory: a few-KB file declaring an 8192 level 0 would otherwise allocate a
//! 256 MB RGBA8 buffer, per image, with a GLB's images decoded in parallel. A file with no
//! level within the cap (in practice a single-level file) decodes its first level no larger
//! than [`KTX2_LEVEL_MAX`] and is capped downstream like any large PNG; one with no level even
//! that small is refused by the header gate and does not decode, the outcome of a broken
//! image. Mips below the decoded level are rebuilt by the existing encoders.
//!
//! **Orientation.** A KTX2 image's row order is given by its `KTXorientation` key, defaulting
//! to `"rd"` (x right, y down: the first row is the top of the picture), and
//! `KHR_texture_basisu` requires that default. `basisu` returns rows in stored order, so the
//! RGBA8 buffer is top-down like a decoded PNG and the encoders' usual vertical flip produces
//! the same bytes for both.
//!
//! **Colour space.** The transcoder returns the stored values without any transfer-function
//! conversion, as a PNG decode does; whether a texture is encoded sRGB or linear stays decided
//! by its material usage, as for every other source.

use image::RgbaImage;

/// The KTX 2.0 file identifier.
pub const KTX2_MAGIC: &[u8; 12] = b"\xABKTX 20\xBB\r\n\x1A\n";

/// The largest level-0 width or height taken, the same bound the explorer applies. Level 0 is
/// only transcoded when it is within [`KTX2_LEVEL_MAX`]; above that the file must carry a
/// smaller mip.
pub const KTX2_MAX_SIZE: u32 = 8192;

/// The decode cap: the first level whose longest side is at most this is the one decoded. It
/// is the largest texture any Unity lane outputs (`texprofile::max_texture_size_for`: 1024 on
/// Windows and macOS, 512 on Linux and WebGL), so a file within it decodes exactly as before
/// and a larger one is downscaled by every lane anyway. It is also the level the explorer's
/// own KTX2 path picks (`bevyvariants::BEVY_TEXTURE_MAX`). A 4 MB RGBA8 decode.
pub const KTX2_DECODE_MAX: u32 = 1024;

/// The largest level ever transcoded, for a file with no level within [`KTX2_DECODE_MAX`]
/// (a single-level file, or a truncated mip chain). 2048 is the Unity importer default
/// (`texprofile::TEXTURE_IMPORTER_DEFAULT_MAX`), the largest size any Unity lane's import
/// produces; its RGBA8 decode is 16 MB, a sixteenth of the old 8192 bound. A file whose
/// smallest level is larger is refused by [`ktx2_header_ok`]: the transcoder has no partial
/// decode, so there is no way to take it with bounded memory.
pub const KTX2_LEVEL_MAX: u32 = 2048;

/// `KHR_DF_MODEL_UASTC` in the data format descriptor's colour-model byte.
const KHR_DF_MODEL_UASTC: u8 = 166;

/// Whether `raw` starts with the KTX2 file identifier. Says nothing about whether it decodes.
pub fn is_ktx2(raw: &[u8]) -> bool {
    raw.starts_with(KTX2_MAGIC)
}

/// The fixed 80-byte KTX2 header, read field by field (little-endian, offsets per the KTX 2.0
/// spec). Says nothing about whether the file decodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ktx2Header {
    pub vk_format: u32,
    pub type_size: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub layers: u32,
    pub faces: u32,
    pub levels: u32,
    pub supercompression: u32,
    pub dfd_offset: u32,
    pub dfd_len: u32,
    pub kvd_offset: u32,
    pub kvd_len: u32,
    pub sgd_offset: u64,
    pub sgd_len: u64,
}

/// Read the header fields of a file that starts with the KTX2 identifier.
pub fn read_header(bytes: &[u8]) -> Option<Ktx2Header> {
    if !is_ktx2(bytes) {
        return None;
    }
    let u32_at = |at: usize| -> Option<u32> {
        Some(u32::from_le_bytes(
            bytes.get(at..at.checked_add(4)?)?.try_into().ok()?,
        ))
    };
    let u64_at = |at: usize| -> Option<u64> {
        Some(u64::from_le_bytes(
            bytes.get(at..at.checked_add(8)?)?.try_into().ok()?,
        ))
    };
    Some(Ktx2Header {
        vk_format: u32_at(12)?,
        type_size: u32_at(16)?,
        width: u32_at(20)?,
        height: u32_at(24)?,
        depth: u32_at(28)?,
        layers: u32_at(32)?,
        faces: u32_at(36)?,
        levels: u32_at(40)?,
        supercompression: u32_at(44)?,
        dfd_offset: u32_at(48)?,
        dfd_len: u32_at(52)?,
        kvd_offset: u32_at(56)?,
        kvd_len: u32_at(60)?,
        sgd_offset: u64_at(64)?,
        sgd_len: u64_at(72)?,
    })
}

/// Level `level`'s index entry: `(byteOffset, byteLength, uncompressedByteLength)`.
pub fn level_index(bytes: &[u8], level: u32) -> Option<(u64, u64, u64)> {
    let at = 80usize.checked_add((level as usize).checked_mul(24)?)?;
    let field = |i: usize| -> Option<u64> {
        Some(u64::from_le_bytes(
            bytes.get(at + i * 8..at + i * 8 + 8)?.try_into().ok()?,
        ))
    };
    Some((field(0)?, field(1)?, field(2)?))
}

/// The data format descriptor (`dfdTotalSize` included) of a file whose header reads.
pub fn dfd<'a>(bytes: &'a [u8], header: &Ktx2Header) -> Option<&'a [u8]> {
    let start = usize::try_from(header.dfd_offset).ok()?;
    let len = usize::try_from(header.dfd_len).ok()?;
    bytes.get(start..start.checked_add(len)?)
}

/// The longest side of the smallest level the header declares (level `levelCount - 1`, or
/// level 0 for a `levelCount` of 0 or 1).
fn smallest_level_side(h: &Ktx2Header) -> u32 {
    let last = h.levels.max(1) - 1;
    h.width.max(h.height).checked_shr(last).unwrap_or(0).max(1)
}

/// The header gate: a single 2D UASTC image, supercompression none (0) or Zstandard (2),
/// 1..=[`KTX2_MAX_SIZE`] on each side, whose smallest declared level is within
/// [`KTX2_LEVEL_MAX`] (so [`decode_ktx2_uastc`] always has a level it may transcode). The data
/// format descriptor is checked before the transcoder is built, so no other codec's tables are
/// ever initialized for a rejected file.
pub fn ktx2_header_ok(bytes: &[u8]) -> bool {
    let valid = || -> Option<bool> {
        let h = read_header(bytes)?;
        let descriptor = dfd(bytes, &h)?;
        Some(
            matches!(h.dfd_len, 44 | 60)
                && descriptor.get(12) == Some(&KHR_DF_MODEL_UASTC)
                && h.vk_format == 0
                && matches!(h.supercompression, 0 | 2)
                && (1..=KTX2_MAX_SIZE).contains(&h.width)
                && (1..=KTX2_MAX_SIZE).contains(&h.height)
                && smallest_level_side(&h) <= KTX2_LEVEL_MAX
                && h.depth == 0
                && h.layers <= 1
                && h.faces == 1,
        )
    };
    valid() == Some(true)
}

/// Decode a UASTC KTX2 to RGBA8, top row first, at the first level no larger than
/// [`KTX2_DECODE_MAX`] on a side, else the first no larger than [`KTX2_LEVEL_MAX`] (see the
/// module docs). `None` for anything that is not a KTX2 the header gate accepts, or that the
/// transcoder rejects; never panics on malformed input that the vendored transcoder bounds.
pub fn decode_ktx2_uastc(raw: &[u8]) -> Option<RgbaImage> {
    use basisu::{DecodeFlags, TargetFormat, Transcoder};

    if !ktx2_header_ok(raw) {
        return None;
    }
    let texture = Transcoder::new(raw).ok()?;
    let first_within = |cap: u32| {
        (0..texture.level_count()).find(|&l| {
            texture
                .image_level_info(l)
                .is_ok_and(|i| i.width.max(i.height) <= cap)
        })
    };
    let level = first_within(KTX2_DECODE_MAX).or_else(|| first_within(KTX2_LEVEL_MAX))?;
    let info = texture.image_level_info(level).ok()?;
    let rgba = texture
        .transcode(level, TargetFormat::Rgba32, DecodeFlags::NONE)
        .ok()?;
    RgbaImage::from_raw(info.width, info.height, rgba)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 8x8 and 6x6 UASTC gradients, zstd-supercompressed (`basisu -uastc -ktx2`, Basis
    // Universal 2.50). Copied from decentraland/dcl-one,
    // `rendering/bevy-explorer/source/crates/image_processing/testdata/`.
    const UASTC_8: &[u8] = include_bytes!("../testdata/uastc_8.ktx2");
    const UASTC_6: &[u8] = include_bytes!("../testdata/uastc_6.ktx2");

    fn patched(at: usize, value: &[u8]) -> Vec<u8> {
        let mut bytes = UASTC_8.to_vec();
        bytes[at..at + value.len()].copy_from_slice(value);
        bytes
    }

    #[test]
    fn uastc_decodes_to_rgba8_at_its_own_size() {
        let img = decode_ktx2_uastc(UASTC_8).expect("8x8 UASTC decodes");
        assert_eq!(img.dimensions(), (8, 8));
        assert_eq!(img.as_raw().len(), 8 * 8 * 4);
        // Not whole 4x4 blocks: still the declared size, not the padded block grid.
        let img = decode_ktx2_uastc(UASTC_6).expect("6x6 UASTC decodes");
        assert_eq!(img.dimensions(), (6, 6));
        assert_eq!(img.as_raw().len(), 6 * 6 * 4);
    }

    #[test]
    fn the_fixtures_carry_no_orientation_override() {
        // The top-down claim rests on the KTX2 default `"rd"`; a fixture declaring another
        // orientation would make the decode test above prove nothing about row order.
        for f in [UASTC_8, UASTC_6] {
            assert!(!f.windows(14).any(|w| w == b"KTXorientation"));
        }
    }

    #[test]
    fn garbage_and_truncations_are_rejected_without_panicking() {
        assert!(decode_ktx2_uastc(&[]).is_none());
        assert!(decode_ktx2_uastc(KTX2_MAGIC).is_none());
        assert!(decode_ktx2_uastc(&[KTX2_MAGIC.as_slice(), &[0u8; 100]].concat()).is_none());
        assert!(decode_ktx2_uastc(b"\x89PNG\r\n\x1a\n").is_none());
        for len in [12, 48, 80, UASTC_8.len() / 2, UASTC_8.len() - 1] {
            assert!(decode_ktx2_uastc(&UASTC_8[..len]).is_none(), "len {len}");
        }
    }

    #[test]
    fn oversized_or_inconsistent_headers_are_rejected() {
        // 40000x40000: the RGBA8 decode alone would be 6.4 GB.
        let huge = [&40_000u32.to_le_bytes()[..], &40_000u32.to_le_bytes()].concat();
        assert!(decode_ktx2_uastc(&patched(20, &huge)).is_none());
        // Level 0 reaching past the end of the file.
        assert!(decode_ktx2_uastc(&patched(88, &u64::MAX.to_le_bytes())).is_none());
        // Level 0 declaring more bytes than an 8x8 image's four blocks hold.
        assert!(decode_ktx2_uastc(&patched(96, &(1u64 << 40).to_le_bytes())).is_none());
        // More levels than an 8x8 mip chain has.
        assert!(decode_ktx2_uastc(&patched(40, &5u32.to_le_bytes())).is_none());
    }

    #[test]
    fn other_codecs_and_shapes_are_rejected_by_the_header_gate() {
        assert!(ktx2_header_ok(UASTC_8));
        assert!(ktx2_header_ok(UASTC_6));
        // BasisLZ (ETC1S) supercompression, and a declared GPU format such as BC7.
        assert!(!ktx2_header_ok(&patched(44, &1u32.to_le_bytes())));
        assert!(decode_ktx2_uastc(&patched(44, &1u32.to_le_bytes())).is_none());
        assert!(!ktx2_header_ok(&patched(12, &145u32.to_le_bytes())));
        // A volume, an array of two, a cubemap.
        assert!(!ktx2_header_ok(&patched(28, &4u32.to_le_bytes())));
        assert!(!ktx2_header_ok(&patched(32, &2u32.to_le_bytes())));
        assert!(!ktx2_header_ok(&patched(36, &6u32.to_le_bytes())));
        // Another colour model in the DFD (ETC1S, UASTC HDR, ...), under either scheme.
        let dfd = u32::from_le_bytes(UASTC_8[48..52].try_into().unwrap()) as usize;
        for model in [163u8, 167, 168, 169] {
            for scheme in [0u32, 2] {
                let mut bytes = UASTC_8.to_vec();
                bytes[dfd + 12] = model;
                bytes[44..48].copy_from_slice(&scheme.to_le_bytes());
                assert!(!ktx2_header_ok(&bytes), "model {model} scheme {scheme}");
            }
        }
        // A DFD offset past the end.
        assert!(!ktx2_header_ok(&patched(48, &u32::MAX.to_le_bytes())));
    }

    #[test]
    fn the_decode_cap_is_the_largest_unity_output() {
        let largest = ["windows", "mac", "osx", "linux", "webgl"]
            .into_iter()
            .map(crate::texprofile::max_texture_size_for)
            .max();
        assert_eq!(Some(KTX2_DECODE_MAX), largest);
        assert_eq!(
            KTX2_LEVEL_MAX,
            crate::texprofile::TEXTURE_IMPORTER_DEFAULT_MAX
        );
    }

    /// The fixture's four 16-byte UASTC blocks (its decompressed level 0, row-major 2x2).
    fn fixture_blocks() -> Vec<Vec<u8>> {
        let levels = crate::ktx2_write::read_levels(UASTC_8).expect("fixture levels");
        levels[0].chunks_exact(16).map(<[u8]>::to_vec).collect()
    }

    /// A Zstandard-supercompressed UASTC KTX2 of `w`x`h` declaring `levels` levels, level `m`
    /// tiled with fixture block `m % 4` so the decoded level can be told apart. Only the
    /// container is built here; the blocks are the fixture's own `basisu` output.
    fn tiled_uastc(w: u32, h: u32, levels: u32) -> Vec<u8> {
        let blocks = fixture_blocks();
        let header = read_header(UASTC_8).unwrap();
        let descriptor = dfd(UASTC_8, &header).unwrap();
        let payload: Vec<(Vec<u8>, usize)> = (0..levels)
            .map(|m| {
                let (lw, lh) = ((w >> m).max(1), (h >> m).max(1));
                let n = lw.div_ceil(4) as usize * lh.div_ceil(4) as usize;
                let raw = blocks[m as usize % blocks.len()].repeat(n);
                (zstd::bulk::compress(&raw, 3).unwrap(), raw.len())
            })
            .collect();
        let dfd_offset = 80 + 24 * levels as usize;
        let mut at = (dfd_offset + descriptor.len()) as u64;
        let mut out = KTX2_MAGIC.to_vec();
        for v in [
            0u32, // vkFormat undefined
            1,    // typeSize
            w,
            h,
            0, // depth
            0, // layers
            1, // faces
            levels,
            2, // Zstandard
            dfd_offset as u32,
            descriptor.len() as u32,
            0, // kvd offset
            0, // kvd length
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&[0u8; 16]); // no supercompression global data
        for (comp, raw) in &payload {
            for v in [at, comp.len() as u64, *raw as u64] {
                out.extend_from_slice(&v.to_le_bytes());
            }
            at += comp.len() as u64;
        }
        out.extend_from_slice(descriptor);
        for (comp, _) in &payload {
            out.extend_from_slice(comp);
        }
        out
    }

    /// What a level tiled with fixture block `block` decodes to at `w`x`h`.
    fn tiled_pixels(block: usize, w: u32, h: u32) -> RgbaImage {
        let src = decode_ktx2_uastc(UASTC_8).unwrap();
        let (bx, by) = ((block % 2) as u32 * 4, (block / 2) as u32 * 4);
        RgbaImage::from_fn(w, h, |x, y| *src.get_pixel(bx + x % 4, by + y % 4))
    }

    /// Level 0 transcoded directly, the decode before the cap.
    fn level0(raw: &[u8]) -> RgbaImage {
        use basisu::{DecodeFlags, TargetFormat, Transcoder};
        let t = Transcoder::new(raw).unwrap();
        let i = t.image_level_info(0).unwrap();
        let rgba = t
            .transcode(0, TargetFormat::Rgba32, DecodeFlags::NONE)
            .unwrap();
        RgbaImage::from_raw(i.width, i.height, rgba).unwrap()
    }

    #[test]
    fn the_fixture_blocks_decode_to_distinct_tiles() {
        // Otherwise the level tests below could not tell which level was decoded.
        let tiles: Vec<_> = (0..4).map(|b| tiled_pixels(b, 4, 4)).collect();
        for a in 0..4 {
            for b in a + 1..4 {
                assert_ne!(tiles[a], tiles[b], "blocks {a} and {b}");
            }
        }
    }

    #[test]
    fn a_large_file_with_mips_decodes_its_first_level_within_the_cap() {
        // 4096x2048, four levels: level 2 (1024x512) is the first within 1024.
        let raw = tiled_uastc(4096, 2048, 4);
        assert!(ktx2_header_ok(&raw));
        let img = decode_ktx2_uastc(&raw).expect("decodes");
        assert_eq!(img.dimensions(), (1024, 512));
        assert_eq!(img, tiled_pixels(2, 1024, 512));
        // Square, two levels: level 1.
        let img = decode_ktx2_uastc(&tiled_uastc(2048, 2048, 2)).expect("decodes");
        assert_eq!(img, tiled_pixels(1, 1024, 1024));
    }

    #[test]
    fn a_file_within_the_cap_decodes_level_0_exactly_as_before() {
        for raw in [
            UASTC_8.to_vec(),
            UASTC_6.to_vec(),
            tiled_uastc(1024, 1024, 2),
            tiled_uastc(1024, 256, 11),
            tiled_uastc(12, 1024, 1),
        ] {
            let img = decode_ktx2_uastc(&raw).expect("decodes");
            assert_eq!(img, level0(&raw));
        }
        assert_eq!(
            decode_ktx2_uastc(&tiled_uastc(1024, 1024, 2)).unwrap(),
            tiled_pixels(0, 1024, 1024)
        );
    }

    #[test]
    fn a_file_with_no_level_within_the_cap_decodes_up_to_the_level_max() {
        // Single level 2048: no smaller level to take, decoded at its own size as before.
        let raw = tiled_uastc(2048, 1024, 1);
        assert_eq!(decode_ktx2_uastc(&raw).unwrap(), level0(&raw));
        // A truncated chain whose last level is 2048: that level, not level 0.
        let img = decode_ktx2_uastc(&tiled_uastc(4096, 4096, 2)).unwrap();
        assert_eq!(img, tiled_pixels(1, 2048, 2048));
    }

    #[test]
    fn the_header_gate_refuses_a_file_whose_smallest_level_is_past_the_level_max() {
        let dims = |w: u32, h: u32, levels: u32| {
            let mut bytes = patched(20, &[&w.to_le_bytes()[..], &h.to_le_bytes()].concat());
            bytes[40..44].copy_from_slice(&levels.to_le_bytes());
            bytes
        };
        // Single level: within the level max taken, past it refused.
        assert!(ktx2_header_ok(&dims(2048, 2048, 1)));
        assert!(ktx2_header_ok(&dims(2048, 64, 0)));
        assert!(!ktx2_header_ok(&dims(2049, 16, 1)));
        assert!(!ktx2_header_ok(&dims(16, 4096, 0)));
        assert!(!ktx2_header_ok(&dims(8192, 8192, 1)));
        // 8192 with mips down to 2048 or below: taken, only the small level is decoded.
        assert!(ktx2_header_ok(&dims(8192, 8192, 3)));
        assert!(ktx2_header_ok(&dims(8192, 4096, 14)));
        assert!(!ktx2_header_ok(&dims(8192, 8192, 2)));
        // A level count past any mip chain still gates on size alone (the transcoder rejects
        // it), and never overflows the shift.
        assert!(ktx2_header_ok(&dims(8192, 8192, 40)));
        // Level 0 past the explorer bound, whatever its mips.
        assert!(!ktx2_header_ok(&dims(16384, 16384, 15)));
        let raw = tiled_uastc(4096, 4096, 1);
        assert!(!ktx2_header_ok(&raw));
        assert!(decode_ktx2_uastc(&raw).is_none());
    }
}

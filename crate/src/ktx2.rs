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
//! Level 0 is transcoded, so the decoded image is the full-resolution picture, the same one a
//! PNG of it would decode to; mips are rebuilt by the existing encoders.
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

/// The largest width or height taken. Its RGBA8 decode is 256 MB, within what `image` allows a
/// PNG, and the same bound the explorer applies.
pub const KTX2_MAX_SIZE: u32 = 8192;

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

/// The header gate: a single 2D UASTC image, supercompression none (0) or Zstandard (2),
/// 1..=[`KTX2_MAX_SIZE`] on each side. The data format descriptor is checked before the
/// transcoder is built, so no other codec's tables are ever initialized for a rejected file.
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
                && h.depth == 0
                && h.layers <= 1
                && h.faces == 1,
        )
    };
    valid() == Some(true)
}

/// Decode a UASTC KTX2 to RGBA8, top row first. `None` for anything that is not a KTX2 the
/// header gate accepts, or that the transcoder rejects; never panics on malformed input that
/// the vendored transcoder bounds.
pub fn decode_ktx2_uastc(raw: &[u8]) -> Option<RgbaImage> {
    use basisu::{DecodeFlags, TargetFormat, Transcoder};

    if !ktx2_header_ok(raw) {
        return None;
    }
    let texture = Transcoder::new(raw).ok()?;
    let info = texture.image_level_info(0).ok()?;
    let rgba = texture
        .transcode(0, TargetFormat::Rgba32, DecodeFlags::NONE)
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
}

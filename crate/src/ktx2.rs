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
//! **Which level.** Always level 0, the full-resolution picture: the same pixels a PNG of the
//! image decodes to. The builder sizes a texture from the decoded image (its platform cap,
//! power-of-two target, oversized-texture placeholder), so decoding a smaller mip would give a
//! KTX2 a lower resolution than the same image as a PNG. A decodable texture is never dropped
//! for its size; the builder caps it like any large PNG, and the encoders rebuild the mips.
//!
//! **Memory.** The transcoder only produces whole levels (Zstandard-decompressing a whole level
//! first), so a few-KB file declaring an 8192 level decodes to a 256 MB RGBA8 buffer, the same
//! as an 8192 PNG. What a KTX2 adds is that such a file is tiny, so a GLB can carry many of
//! them and decode them all at once. That burst is bounded by a process-wide budget
//! ([`KTX2_DECODE_BUDGET`]) every decode reserves before it allocates: a GLB decoding its
//! images in parallel, or several JIT builds sharing one abcdn process, wait for each other
//! instead of allocating together. A decode larger than the whole budget waits until it can
//! run alone, and still runs.
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

/// The bytes all in-flight [`decode_ktx2_uastc`] calls may hold at once, process-wide.
/// 512 MiB is `image`'s default allocation limit for a single PNG decode, so KTX2 decodes
/// together never reserve more than one PNG decode may take alone; it fits one worst-case
/// 8192x8192 decode (256 MiB RGBA8 plus 64 MiB of decompressed UASTC level), or about a hundred
/// 1024x1024 ones at once.
pub const KTX2_DECODE_BUDGET: u64 = 512 << 20;

static DECODE_BUDGET: DecodeBudget = DecodeBudget::new(KTX2_DECODE_BUDGET);

/// A counting byte budget: [`DecodeBudget::acquire`] blocks until the bytes asked for fit, and
/// the returned guard gives them back on drop. A request larger than the whole budget is
/// charged the whole budget, so it waits until nothing else holds any and then runs alone; it
/// is never refused.
pub struct DecodeBudget {
    cap: u64,
    used: std::sync::Mutex<u64>,
    freed: std::sync::Condvar,
}

/// Bytes held from a [`DecodeBudget`] until dropped.
pub struct BudgetGuard<'a> {
    budget: &'a DecodeBudget,
    bytes: u64,
}

impl DecodeBudget {
    pub const fn new(cap: u64) -> Self {
        Self {
            cap,
            used: std::sync::Mutex::new(0),
            freed: std::sync::Condvar::new(),
        }
    }

    /// Reserve `bytes` (at most the whole budget), waiting while other holders leave too
    /// little. A holder that panicked still releases through its guard's drop, so a poisoned
    /// lock carries a correct count and is used as is.
    pub fn acquire(&self, bytes: u64) -> BudgetGuard<'_> {
        let bytes = bytes.min(self.cap);
        let mut used = self.used.lock().unwrap_or_else(|e| e.into_inner());
        while *used + bytes > self.cap {
            used = self.freed.wait(used).unwrap_or_else(|e| e.into_inner());
        }
        *used += bytes;
        BudgetGuard {
            budget: self,
            bytes,
        }
    }

    /// Bytes currently reserved.
    pub fn in_use(&self) -> u64 {
        *self.used.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for BudgetGuard<'_> {
    fn drop(&mut self) {
        let mut used = self.budget.used.lock().unwrap_or_else(|e| e.into_inner());
        *used -= self.bytes;
        drop(used);
        self.budget.freed.notify_all();
    }
}

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

/// Decode a UASTC KTX2 to RGBA8, top row first, at level 0 (see the module docs). `None` for
/// anything that is not a KTX2 the header gate accepts, or that the transcoder rejects; never
/// panics on malformed input that the vendored transcoder bounds.
///
/// The decode holds [`KTX2_DECODE_BUDGET`] for its peak (the RGBA8 output plus the level's
/// decompressed blocks) and releases it on return, not when the caller drops the image.
/// Callers keep decoded images for a whole build (the per-entity decode cache, a scene's
/// `images`), so a guard tied to the buffer would let one build's retained images hold the
/// budget and deadlock its own next decode. What the budget bounds is the burst: decodes in
/// flight at once, across a GLB's parallel image decodes and abcdn's concurrent builds.
pub fn decode_ktx2_uastc(raw: &[u8]) -> Option<RgbaImage> {
    decode_ktx2_uastc_in(raw, &DECODE_BUDGET)
}

fn decode_ktx2_uastc_in(raw: &[u8], budget: &DecodeBudget) -> Option<RgbaImage> {
    use basisu::{DecodeFlags, TargetFormat, Transcoder};

    if !ktx2_header_ok(raw) {
        return None;
    }
    let texture = Transcoder::new(raw).ok()?;
    let level = 0;
    let info = texture.image_level_info(level).ok()?;
    let (w, h) = (u64::from(info.width), u64::from(info.height));
    // RGBA8 out, plus 16 bytes per 4x4 UASTC block for the decompressed level.
    let _held = budget.acquire(w * h * 4 + w.div_ceil(4) * h.div_ceil(4) * 16);
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

    /// Level 0 transcoded directly.
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

    /// Every file decodes level 0, the full-resolution picture, whatever mips it carries: the
    /// builder sizes the texture from the decoded image, so a KTX2 gets the same platform cap,
    /// power-of-two target and oversized-texture placeholder as the same image as a PNG.
    #[test]
    fn every_file_decodes_level_0_at_its_full_size() {
        for (w, h, levels) in [
            (8, 8, 1),
            (1024, 1024, 2),
            (1024, 256, 11),
            (12, 1024, 1),
            (2048, 1024, 1),
            (3000, 1500, 3),
            (4096, 2048, 4),
            (4096, 4096, 2),
        ] {
            let raw = tiled_uastc(w, h, levels);
            assert!(ktx2_header_ok(&raw), "{w}x{h}");
            let img = decode_ktx2_uastc(&raw).expect("decodes");
            assert_eq!(img.dimensions(), (w, h), "{w}x{h} with {levels} levels");
            assert_eq!(img, tiled_pixels(0, w, h), "{w}x{h}: not level 0");
        }
        for raw in [UASTC_8, UASTC_6] {
            assert_eq!(decode_ktx2_uastc(raw).unwrap(), level0(raw));
        }
    }

    /// A large KTX2 with mips is sized like the same image as a PNG: the 3000 px case from the
    /// review, which a mip-picking decode turned into 512 instead of 1024.
    #[test]
    fn a_large_ktx2_with_mips_gets_the_png_target_size() {
        let raw = tiled_uastc(3000, 3000, 3);
        let img = decode_ktx2_uastc(&raw).expect("decodes");
        let png_like = crate::texprofile::bc7_target_size(3000, 3000, 1024);
        assert_eq!(
            crate::texprofile::bc7_target_size(img.width(), img.height(), 1024),
            png_like
        );
        assert_eq!(png_like, (1024, 1024));
    }

    #[test]
    fn the_header_gate_takes_any_level_count_within_the_max_size() {
        let dims = |w: u32, h: u32, levels: u32| {
            let mut bytes = patched(20, &[&w.to_le_bytes()[..], &h.to_le_bytes()].concat());
            bytes[40..44].copy_from_slice(&levels.to_le_bytes());
            bytes
        };
        for levels in [0, 1, 2, 14] {
            assert!(ktx2_header_ok(&dims(8192, 8192, levels)), "{levels}");
            assert!(ktx2_header_ok(&dims(4096, 16, levels)), "{levels}");
        }
        assert!(!ktx2_header_ok(&dims(16384, 16384, 15)));
    }

    #[test]
    fn the_budget_serializes_decodes_that_do_not_fit_together() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        let budget = DecodeBudget::new(100);
        let first = budget.acquire(60);
        let second_done = AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                let _g = budget.acquire(60);
                second_done.store(true, Ordering::SeqCst);
            });
            std::thread::sleep(Duration::from_millis(100));
            assert!(
                !second_done.load(Ordering::SeqCst),
                "60 + 60 > 100 must wait"
            );
            assert_eq!(budget.in_use(), 60);
            drop(first);
        });
        assert!(second_done.load(Ordering::SeqCst));
        assert_eq!(budget.in_use(), 0);
        // Requests that fit together do not wait.
        let a = budget.acquire(40);
        let b = budget.acquire(60);
        assert_eq!(budget.in_use(), 100);
        drop((a, b));
    }

    #[test]
    fn a_request_larger_than_the_budget_runs_alone_and_is_never_refused() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        let budget = DecodeBudget::new(100);
        let small = budget.acquire(10);
        let big_done = AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                let _g = budget.acquire(1 << 40);
                assert_eq!(budget.in_use(), 100, "charged the whole budget");
                big_done.store(true, Ordering::SeqCst);
            });
            std::thread::sleep(Duration::from_millis(100));
            assert!(
                !big_done.load(Ordering::SeqCst),
                "waits for the other holder"
            );
            drop(small);
        });
        assert!(big_done.load(Ordering::SeqCst));
        assert_eq!(budget.in_use(), 0);
    }

    #[test]
    fn a_decode_larger_than_the_budget_still_decodes() {
        // 2048x2048 single level needs 16 MiB + 4 MiB; a 1 KiB budget still lets it through,
        // and gives everything back.
        let budget = DecodeBudget::new(1024);
        let raw = tiled_uastc(2048, 2048, 1);
        let img = decode_ktx2_uastc_in(&raw, &budget).expect("decodes");
        assert_eq!(img, level0(&raw));
        assert_eq!(budget.in_use(), 0);
        // A rejected file releases too (it never reserves).
        assert!(decode_ktx2_uastc_in(&UASTC_8[..40], &budget).is_none());
        assert_eq!(budget.in_use(), 0);
    }
}

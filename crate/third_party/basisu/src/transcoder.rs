// Modified in dcl-one: sizes read from untrusted files are bounded; see
// patches/0001-bound-untrusted-sizes.patch beside this crate.
//! Top-level `Ktx2Transcoder` for Basis Universal KTX2 files. Parses the
//! container, decodes the ETC1S supercompression global data (codebooks +
//! per-image slice descriptors), and dispatches `transcode_image_level` to the
//! per-target converters. This is the public entry point callers use to
//! transcode a level.

use crate::basislz::etc1s::{Etc1sTranscoder, VideoState, MAX_PREV_FRAME_LEVELS};
use crate::dispatch;
use crate::ktx2::{BasisFormat, Ktx2Header, KTX2_SS_UASTC_HDR_6X6I, KTX2_SS_XUASTC_LDR};
use alloc::vec::Vec;

/// `KTX2_IMAGE_IS_P_FRAME`: an ETC1S image desc flag marking a predicted
/// (inter) video frame. Its presence on any image promotes a layer array to a
/// video, even without a `KTXanimData` key.
const KTX2_IMAGE_IS_P_FRAME: u32 = 2;

/// One ETC1S image's slice offsets/lengths (relative to its level data).
#[derive(Clone, Copy)]
struct Etc1sImageDesc {
    /// Per-image flag bits; `KTX2_IMAGE_IS_P_FRAME` here marks a predicted
    /// video frame.
    image_flags: u32,
    /// Byte offset of the RGB (color) slice within the level data.
    rgb_ofs: u32,
    /// Byte length of the RGB slice.
    rgb_len: u32,
    /// Byte offset of the alpha slice within the level data (only meaningful
    /// when the file has alpha).
    alpha_ofs: u32,
    /// Byte length of the alpha slice.
    alpha_len: u32,
}

/// A parsed + prepared Basis Universal KTX2, ready to transcode any level.
pub struct Ktx2Transcoder<'a> {
    /// The whole KTX2 file.
    data: &'a [u8],
    /// Parsed header, level index, and key/value-derived properties.
    pub header: Ktx2Header,
    /// Decoded ETC1S codebooks + Huffman tables (`None` for UASTC files).
    etc1s: Option<Etc1sTranscoder>,
    /// ETC1S per-image slice descriptors (empty for UASTC files).
    image_descs: Vec<Etc1sImageDesc>,
    /// UASTC HDR 6x6 intermediate per-image `(byte offset, byte length)`
    /// slice descriptors, relative to their level's byte offset (empty for
    /// every other format).
    slice_descs: Vec<(u32, u32)>,
    /// True for ETC1S video: a `KTXanimData` key, or any image desc carrying
    /// `KTX2_IMAGE_IS_P_FRAME` on a non-cubemap layer array.
    is_video: bool,
}

/// Inflate a Zstandard-supercompressed KTX2 level to exactly `want` bytes
/// (the level's `uncompressed_byte_length`). `None` if the stream is malformed
/// or does not produce exactly `want` bytes; a size mismatch means a corrupt
/// level. Decoding is pure Rust (`ruzstd`).
#[cfg(feature = "zstd")]
fn zstd_inflate(comp: &[u8], want: usize) -> Option<Vec<u8>> {
    use ruzstd::io::Read;
    // The decoder holds a frame's whole window before it returns a byte, and the file declares
    // the window. No level needs a window larger than itself (8 MiB covers encoders that declare
    // their default window for small inputs), so a larger one is rejected when the frame opens.
    let max_window = (want as u64).max(8 << 20);
    let mut dec =
        ruzstd::decoding::StreamingDecoder::new_with_max_window_size(comp, max_window).ok()?;
    // `want` comes from the file: a failed allocation must fail the level, not abort.
    let mut out = Vec::new();
    out.try_reserve_exact(want).ok()?;
    out.resize(want, 0);
    // Require an exact fill: read_exact errors if the stream yields fewer bytes.
    dec.read_exact(&mut out).ok()?;
    // Reject trailing data beyond `want`: a single extra byte means a
    // malformed or oversized stream.
    let mut extra = [0u8; 1];
    if dec.read(&mut extra).ok()? != 0 {
        return None;
    }
    Some(out)
}

/// Without the `zstd` feature, zstd-supercompressed levels are unsupported.
#[cfg(not(feature = "zstd"))]
fn zstd_inflate(_comp: &[u8], _want: usize) -> Option<Vec<u8>> {
    None
}

/// `start + offset .. start + offset + length` as a range within a `len`-byte buffer, or `None` if
/// a sum overflows or the range ends past `len`. Offsets and lengths come from the file, so they
/// are added as u64 before becoming `usize`, which is 32 bits on wasm32.
fn checked_range(
    start: u64,
    offset: u64,
    length: u64,
    len: usize,
) -> Option<core::ops::Range<usize>> {
    let begin = start.checked_add(offset)?;
    let end = begin.checked_add(length)?;
    if end > len as u64 {
        return None;
    }
    Some(begin as usize..end as usize)
}

/// Read a little-endian u16 at byte offset `o`. The KTX2 supercompression
/// global data stores all integers little-endian.
fn rd16(d: &[u8], o: usize) -> u32 {
    u16::from_le_bytes([d[o], d[o + 1]]) as u32
}
/// Read a little-endian u32 at byte offset `o`.
fn rd32(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]])
}

impl<'a> Ktx2Transcoder<'a> {
    /// Parse + prepare a KTX2 file. `None` if it is not a Basis payload or the
    /// supercompression global data is malformed.
    pub fn new(data: &'a [u8]) -> Option<Self> {
        let header = Ktx2Header::parse(data)?;
        let mut etc1s = None;
        let mut image_descs = Vec::new();
        let mut slice_descs = Vec::new();

        if header.format == BasisFormat::UastcHdr6x6
            || matches!(header.format, BasisFormat::XuastcLdr(_))
        {
            // The intermediate streams are variable-length per image, so the
            // supercompression global data is an array of per-image
            // offset/length descs: the Khronos schemes (4 for HDR 6x6, 5 for
            // XUASTC) use 12-byte structs (offset, length, profile), the
            // old-style v1.6/v2.0 presentation (BasisLZ scheme) 8-byte ones.
            // Any other scheme fails.
            let desc_size = match (header.supercompression, header.format) {
                (KTX2_SS_UASTC_HDR_6X6I, BasisFormat::UastcHdr6x6) => 12,
                (KTX2_SS_XUASTC_LDR, BasisFormat::XuastcLdr(_)) => 12,
                (1, _) => 8,
                _ => return None,
            };
            let image_count = u64::from(header.level_count.max(1))
                * u64::from(header.layer_count.max(1))
                * u64::from(header.face_count.max(1));
            let descs_length = image_count * desc_size;
            if header.sgd_byte_length != descs_length {
                return None;
            }
            let descs = checked_range(header.sgd_byte_offset, 0, descs_length, data.len())?;
            for i in 0..image_count as usize {
                let o = descs.start + i * desc_size as usize;
                let ofs = rd32(data, o);
                let len = rd32(data, o + 4);
                if len == 0 {
                    return None;
                }
                slice_descs.push((ofs, len));
            }
        }

        if header.format == BasisFormat::Etc1s {
            // BasisLZ global data header (20 bytes): endpoint/selector counts
            // (u16 each), the endpoint/selector/table byte lengths (u32 each),
            // and an extendedByteLength u32 this decoder never needs.
            let sgd = checked_range(header.sgd_byte_offset, 0, 20, data.len())?.start;
            let num_e = rd16(data, sgd);
            let num_s = rd16(data, sgd + 2);
            let e_len = u64::from(rd32(data, sgd + 4));
            let s_len = u64::from(rd32(data, sgd + 8));
            let t_len = u64::from(rd32(data, sgd + 12));

            let image_count = u64::from(header.level_count.max(1))
                * u64::from(header.layer_count.max(1))
                * u64::from(header.face_count.max(1));

            let descs = checked_range(header.sgd_byte_offset, 20, 20 * image_count, data.len())?;
            for i in 0..image_count as usize {
                let o = descs.start + 20 * i;
                image_descs.push(Etc1sImageDesc {
                    image_flags: rd32(data, o),
                    rgb_ofs: rd32(data, o + 4),
                    rgb_len: rd32(data, o + 8),
                    alpha_ofs: rd32(data, o + 12),
                    alpha_len: rd32(data, o + 16),
                });
            }

            // The payloads follow the image descs in a fixed order: endpoint
            // codebook, selector codebook, Huffman tables.
            let e_range = checked_range(descs.end as u64, 0, e_len, data.len())?;
            let s_range = checked_range(e_range.end as u64, 0, s_len, data.len())?;
            let t_range = checked_range(s_range.end as u64, 0, t_len, data.len())?;
            let mut t = Etc1sTranscoder::new();
            if !t.decode_palettes(num_e, &data[e_range], num_s, &data[s_range])
                || !t.decode_tables(&data[t_range])
            {
                return None;
            }
            etc1s = Some(t);
        }

        // Video detection: a `KTXanimData` key marks video; absent that, a
        // non-cubemap layer array is promoted to video if any ETC1S image desc
        // carries a P-frame flag.
        let mut is_video = header.has_anim_key;
        if !is_video
            && header.format == BasisFormat::Etc1s
            && header.face_count == 1
            && header.layer_count > 1
        {
            is_video = image_descs
                .iter()
                .any(|d| d.image_flags & KTX2_IMAGE_IS_P_FRAME != 0);
        }

        Some(Self {
            data,
            header,
            etc1s,
            image_descs,
            slice_descs,
            is_video,
        })
    }

    /// Whether this is an ETC1S video (P-frame conditional replenishment).
    pub fn is_video(&self) -> bool {
        self.is_video
    }

    /// `(num_blocks_x, num_blocks_y)` for a mip level, in the source format's
    /// own block size (4x4 for ETC1S/UASTC, the footprint for raw ASTC).
    fn level_blocks(&self, level: u32) -> (u32, u32) {
        let lw = (self.header.width >> level).max(1);
        let lh = (self.header.height >> level).max(1);
        let (bw, bh) = self.header.format.block_dims();
        (lw.div_ceil(bw), lh.div_ceil(bh))
    }

    /// Index of image (level, layer, face) in the per-image descriptor arrays, computed in u64:
    /// `layer_count` is unbounded. An index past `usize` misses every array.
    fn image_index(&self, level: u32, layer: u32, face: u32) -> usize {
        let faces = u64::from(self.header.face_count.max(1));
        let layers = u64::from(self.header.layer_count.max(1));
        let index = (u64::from(level) * layers + u64::from(layer)) * faces + u64::from(face);
        usize::try_from(index).unwrap_or(usize::MAX)
    }

    /// ETC1S RGB (+ optional alpha) slice bytes for image (level, layer, face).
    fn etc1s_slices(&self, level: u32, layer: u32, face: u32) -> Option<(&[u8], Option<&[u8]>)> {
        let d = self.image_descs.get(self.image_index(level, layer, face))?;
        let lo = self.header.levels.get(level as usize)?.byte_offset;
        let len = self.data.len();
        let rgb = &self.data[checked_range(lo, d.rgb_ofs.into(), d.rgb_len.into(), len)?];
        let alpha = if self.header.has_alpha {
            Some(&self.data[checked_range(lo, d.alpha_ofs.into(), d.alpha_len.into(), len)?])
        } else {
            None
        };
        Some((rgb, alpha))
    }

    /// Flags-aware transcode of image (level, layer 0, face 0) into `out`.
    /// `flags` are the `basisd_decode_flags` bits, forwarded to the per-target
    /// dispatch which interprets the bits each target honors (for example
    /// HIGH_QUALITY, BC1_FORBID_THREE_COLOR_BLOCKS, NO_ETC1S_CHROMA_FILTERING).
    pub fn transcode_image_level_flags(
        &self,
        level: u32,
        fmt: i32,
        flags: u32,
        out: &mut [u8],
    ) -> Option<()> {
        self.transcode_image_level_inner(level, 0, 0, fmt, flags, None, out)
    }

    /// Flags-aware transcode of a specific image `(level, layer, face)` into
    /// `out`. Cubemap faces and array layers select among the multiple images
    /// per level.
    pub fn transcode_image(
        &self,
        level: u32,
        layer: u32,
        face: u32,
        fmt: i32,
        flags: u32,
        out: &mut [u8],
    ) -> Option<()> {
        self.transcode_image_level_inner(level, layer, face, fmt, flags, None, out)
    }

    /// Transcode image (level, layer 0, face 0) to a target format with no
    /// decode flags, into `out`. `None` for unsupported (format, target)
    /// combinations.
    pub fn transcode_image_level(&self, level: u32, fmt: i32, out: &mut [u8]) -> Option<()> {
        self.transcode_image_level_inner(level, 0, 0, fmt, 0, None, out)
    }

    /// Transcode one video frame (`layer` is the frame index) with cross-frame
    /// state, into `out`. Frames of a level must be transcoded in ascending
    /// layer order for the conditional-replenishment decode to see the right
    /// previous frame. On a non-video file this behaves exactly like
    /// [`Self::transcode_image`].
    pub fn transcode_image_video(
        &self,
        level: u32,
        layer: u32,
        fmt: i32,
        flags: u32,
        state: &mut VideoState,
        out: &mut [u8],
    ) -> Option<()> {
        self.transcode_image_level_inner(level, layer, 0, fmt, flags, Some(state), out)
    }

    /// Shared body behind the public `transcode_*` entry points. Resolves the
    /// level's block and pixel dimensions, then dispatches to the ETC1S or
    /// UASTC path for the file's source format. `None` for an unsupported
    /// (source, target) pair or out-of-range image coordinates.
    #[allow(clippy::too_many_arguments)]
    fn transcode_image_level_inner(
        &self,
        level: u32,
        layer: u32,
        face: u32,
        fmt: i32,
        flags: u32,
        video: Option<&mut VideoState>,
        out: &mut [u8],
    ) -> Option<()> {
        let (bx, by) = self.level_blocks(level);
        let lw = (self.header.width >> level).max(1);
        let lh = (self.header.height >> level).max(1);
        match self.header.format {
            BasisFormat::Etc1s => {
                let t = self.etc1s.as_ref()?;
                let (rgb, alpha) = self.etc1s_slices(level, layer, face)?;
                // Cross-frame state only applies to video, and only for the
                // first MAX_PREV_FRAME_LEVELS levels; anything else decodes
                // stateless.
                let vid = if self.is_video && (level as usize) < MAX_PREV_FRAME_LEVELS {
                    video.map(|s| (s, level))
                } else {
                    None
                };
                dispatch::transcode_etc1s(t, rgb, alpha, bx, by, lw, lh, fmt, flags, vid, out)
            }
            // UASTC (LDR/HDR) and the raw ASTC sources share the level/slice
            // layout (contiguous raw 16-byte blocks per image); only the
            // per-target dispatch differs.
            BasisFormat::Uastc
            | BasisFormat::UastcHdr4x4
            | BasisFormat::AstcLdr(_)
            | BasisFormat::AstcHdr6x6 => {
                self.transcode_raw_level(level, layer, face, fmt, flags, bx, by, lw, lh, out)
            }
            // The XUASTC stream's slice comes from the SGD descs like the 6x6
            // intermediate; the transcode layer validates the stream header
            // against the level's block counts.
            BasisFormat::XuastcLdr(b) => {
                let image_index = self.image_index(level, layer, face);
                let &(ofs, len) = self.slice_descs.get(image_index)?;
                let lo = self.header.levels.get(level as usize)?.byte_offset;
                let stream =
                    &self.data[checked_range(lo, ofs.into(), len.into(), self.data.len())?];
                crate::xuastc::transcode::transcode_xuastc(
                    stream, b, bx, by, lw, lh, fmt, flags, out,
                )
            }
            // The intermediate stream decompresses to raw ASTC HDR 6x6 blocks
            // first; the per-target paths are then the raw 6x6 ones.
            BasisFormat::UastcHdr6x6 => {
                let image_index = self.image_index(level, layer, face);
                let &(ofs, len) = self.slice_descs.get(image_index)?;
                let lo = self.header.levels.get(level as usize)?.byte_offset;
                let stream =
                    &self.data[checked_range(lo, ofs.into(), len.into(), self.data.len())?];
                let (blocks, dec_w, dec_h) = crate::uastc_hdr_6x6::decode_6x6_hdr(stream)?;
                // The decoded dimensions must match the level's, or the stream
                // is inconsistent with the container.
                if dec_w != lw || dec_h != lh {
                    return None;
                }
                dispatch::transcode_astc_hdr_6x6(&blocks, bx, by, lw, lh, fmt, flags, out)
            }
        }
    }

    /// Raw-block (UASTC LDR/HDR or raw ASTC) image transcode into `out`. The
    /// level data is `layer_count*face_count` contiguous 2D images in
    /// layer-major order, each `num_blocks_x*num_blocks_y*16` bytes of the
    /// source's own block size. Uncompressed and Zstandard-supercompressed
    /// levels are handled; returns `None` for an unknown supercompression
    /// scheme, a zstd level when the `zstd` feature is off, or an unsupported
    /// target.
    #[allow(clippy::too_many_arguments)]
    fn transcode_raw_level(
        &self,
        level: u32,
        layer: u32,
        face: u32,
        fmt: i32,
        flags: u32,
        bx: u32,
        by: u32,
        lw: u32,
        lh: u32,
        out: &mut [u8],
    ) -> Option<()> {
        let l = self.header.levels.get(level as usize)?;
        let comp = &self.data[checked_range(l.byte_offset, 0, l.byte_length, self.data.len())?];
        // One image's raw 16-byte blocks, and the whole level: every layer and face. In u64:
        // `layer_count` is unbounded.
        let total_2d = u64::from(bx) * u64::from(by) * 16;
        let face_count = u64::from(self.header.face_count.max(1));
        let level_size = total_2d * u64::from(self.header.layer_count.max(1)) * face_count;
        // ss=2 (Zstandard): inflate the whole level to its uncompressed length
        // before indexing the per-image data. ss=1 (BasisLZ) never reaches the
        // raw-block path.
        let level_owned;
        let level_data: &[u8] = match self.header.supercompression {
            0 => comp,
            2 => {
                // The declared length sizes the inflate buffer; no valid level is larger than
                // its blocks.
                if l.uncompressed_byte_length > level_size {
                    return None;
                }
                let want = usize::try_from(l.uncompressed_byte_length).ok()?;
                level_owned = zstd_inflate(comp, want)?;
                &level_owned
            }
            _ => return None, // unknown supercompression scheme
        };
        // The raw ASTC LDR arm hard-checks the level size against the recorded
        // uncompressed length: for scheme 0 the two length fields must agree
        // (for zstd the inflate above already enforced it).
        if matches!(self.header.format, BasisFormat::AstcLdr(_))
            && self.header.supercompression == 0
            && l.byte_length != l.uncompressed_byte_length
        {
            return None;
        }
        let image = u64::from(layer) * face_count + u64::from(face);
        let img_ofs = image.checked_mul(total_2d)?;
        let img = &level_data[checked_range(img_ofs, 0, total_2d, level_data.len())?];

        match self.header.format {
            BasisFormat::UastcHdr4x4 => {
                dispatch::transcode_uastc_hdr(img, bx, by, lw, lh, fmt, out)
            }
            BasisFormat::AstcLdr(b) => dispatch::transcode_astc_ldr(
                img,
                b,
                bx,
                by,
                lw,
                lh,
                fmt,
                flags,
                self.header.astc_is_srgb,
                self.header.has_alpha,
                out,
            ),
            BasisFormat::AstcHdr6x6 => {
                dispatch::transcode_astc_hdr_6x6(img, bx, by, lw, lh, fmt, flags, out)
            }
            _ => dispatch::transcode_uastc(
                img,
                self.header.has_alpha,
                bx,
                by,
                lw,
                lh,
                fmt,
                flags,
                out,
            ),
        }
    }
}

#[cfg(all(test, feature = "zstd"))]
mod bounded_zstd_tests {
    use super::zstd_inflate;

    // A non-single-segment frame without content size; the first block is RLE.
    fn rle_frame(window: u8, lengths: &[u32]) -> Vec<u8> {
        let mut frame = vec![0x28, 0xb5, 0x2f, 0xfd, 0, window];
        for (i, &length) in lengths.iter().enumerate() {
            let header = (length << 3) | 2 | u32::from(i + 1 == lengths.len());
            frame.extend_from_slice(&header.to_le_bytes()[..3]);
            frame.push(42);
        }
        frame
    }

    #[test]
    fn exact_output_and_truncation() {
        assert_eq!(
            zstd_inflate(&rle_frame(0, &[16, 16]), 32),
            Some(vec![42; 32])
        );
        assert!(zstd_inflate(&rle_frame(0, &[16]), 32).is_none());
        let mut truncated = rle_frame(0, &[32]);
        truncated.pop();
        assert!(zstd_inflate(&truncated, 32).is_none());
    }

    #[test]
    fn rejects_large_window_and_excess_output() {
        // 128 MiB window rejected at initialization, including a tiny otherwise valid frame.
        assert!(zstd_inflate(&rle_frame(17 << 3, &[32]), 32).is_none());
        // Output beyond the level's length fails the level.
        assert!(zstd_inflate(&rle_frame(0, &[32, 32]), 32).is_none());
        assert!(zstd_inflate(&rle_frame(0, &[128 * 1024]), 32).is_none());
    }
    #[test]
    fn raw_and_compressed_literal_blocks_obey_output_budget() {
        for compressed in [false, true] {
            let mut frame = vec![0x28, 0xb5, 0x2f, 0xfd, 0, 0];
            let mut body = Vec::new();
            if compressed {
                body.push(16 << 3);
            } // raw literals header
            body.extend_from_slice(&[7; 16]);
            if compressed {
                body.push(0);
            } // no sequences
            let header = ((body.len() as u32) << 3) | 1 | if compressed { 4 } else { 0 };
            frame.extend_from_slice(&header.to_le_bytes()[..3]);
            frame.extend_from_slice(&body);
            assert_eq!(zstd_inflate(&frame, 16), Some(vec![7; 16]));
            assert!(zstd_inflate(&frame, 15).is_none());
        }
    }
}

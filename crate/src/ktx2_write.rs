//! A minimal KTX 2.0 writer for block-compressed 2D textures: one image, a mip chain, BC1 (RGB),
//! BC5 or BC7 payloads, each level Zstandard-supercompressed (`supercompressionScheme` 2).
//!
//! Output is a function of the input blocks alone:
//! - Zstandard at [`ZSTD_LEVEL`] (ruzstd's `Fastest`, a fixed pure-Rust algorithm, no
//!   threads, no dictionary). ruzstd writes a content checksum only when its `hash` feature is
//!   on; nothing in this workspace enables it, and enabling it would change every variant's
//!   bytes (and so its CID), not its pixels.
//! - Fixed key/value data: `KTXorientation` = `rd` (rows top-down, the first row is the top of
//!   the picture) and `KTXwriter` = [`WRITER`], with no version in it, so a release that does not
//!   change the bytes does not change any CID.
//! - Level data is stored smallest level first, as the spec recommends; with
//!   supercompression the level alignment is 1, so there is no padding between levels.

use anyhow::{bail, Result};

/// `KTXwriter` value.
pub const WRITER: &str = "abgen bevy-variants";
/// The Zstandard level used for every level's data.
pub const ZSTD_LEVEL: ruzstd::encoding::CompressionLevel =
    ruzstd::encoding::CompressionLevel::Fastest;
/// `KTX_SS_ZSTD`.
pub const SUPERCOMPRESSION_ZSTD: u32 = 2;

/// The payloads this writer describes. UNORM only: the sampler's colour space is chosen by
/// whoever loads the texture (Bevy applies sRGB from how the texture is requested).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BcFormat {
    /// `VK_FORMAT_BC1_RGB_UNORM_BLOCK`: opaque, four-colour blocks.
    Bc1Rgb,
    /// `VK_FORMAT_BC5_UNORM_BLOCK`: two channels (R, G).
    Bc5,
    /// `VK_FORMAT_BC7_UNORM_BLOCK`.
    Bc7,
}

impl BcFormat {
    pub fn vk_format(self) -> u32 {
        match self {
            BcFormat::Bc1Rgb => 131,
            BcFormat::Bc5 => 141,
            BcFormat::Bc7 => 145,
        }
    }

    pub fn block_bytes(self) -> usize {
        match self {
            BcFormat::Bc1Rgb => 8,
            BcFormat::Bc5 | BcFormat::Bc7 => 16,
        }
    }

    /// `KHR_DF_MODEL_BC1A` / `KHR_DF_MODEL_BC5` / `KHR_DF_MODEL_BC7`.
    pub fn color_model(self) -> u8 {
        match self {
            BcFormat::Bc1Rgb => 128,
            BcFormat::Bc5 => 132,
            BcFormat::Bc7 => 134,
        }
    }

    /// `(bitOffset, bitLength - 1, channelType)` per sample.
    fn samples(self) -> &'static [(u16, u8, u8)] {
        match self {
            // KHR_DF_CHANNEL_BC1A_COLOR: RGB without alpha.
            BcFormat::Bc1Rgb => &[(0, 63, 0)],
            // KHR_DF_CHANNEL_BC5_RED, KHR_DF_CHANNEL_BC5_GREEN.
            BcFormat::Bc5 => &[(0, 63, 0), (64, 63, 1)],
            // KHR_DF_CHANNEL_BC7_COLOR.
            BcFormat::Bc7 => &[(0, 127, 0)],
        }
    }
}

/// Bytes of one level of `w x h` texels in `format` (whole blocks, at least one).
pub fn level_bytes(format: BcFormat, w: u32, h: u32) -> usize {
    (w.div_ceil(4).max(1) as usize) * (h.div_ceil(4).max(1) as usize) * format.block_bytes()
}

/// Split a concatenated mip chain (level 0 first) into its levels.
pub fn split_chain(format: BcFormat, w: u32, h: u32, mips: u32, data: &[u8]) -> Result<Vec<&[u8]>> {
    let mut out = Vec::with_capacity(mips as usize);
    let mut at = 0usize;
    for m in 0..mips {
        let n = level_bytes(format, (w >> m).max(1), (h >> m).max(1));
        let Some(level) = data.get(at..at + n) else {
            bail!("mip chain is {} bytes, level {m} needs {}", data.len(), at + n);
        };
        out.push(level);
        at += n;
    }
    if at != data.len() {
        bail!("mip chain is {} bytes, {mips} levels take {at}", data.len());
    }
    Ok(out)
}

/// The basic data format descriptor, `dfdTotalSize` included.
pub fn basic_dfd(format: BcFormat) -> Vec<u8> {
    let samples = format.samples();
    let block_size = 24 + 16 * samples.len();
    let mut d = Vec::with_capacity(4 + block_size);
    d.extend_from_slice(&((4 + block_size) as u32).to_le_bytes());
    // vendorId 0 (Khronos), descriptorType 0 (basic).
    d.extend_from_slice(&0u32.to_le_bytes());
    // versionNumber 2 (KDF 1.3), descriptorBlockSize.
    d.extend_from_slice(&2u16.to_le_bytes());
    d.extend_from_slice(&(block_size as u16).to_le_bytes());
    // colorModel, colorPrimaries BT709 (1), transferFunction LINEAR (1), flags straight (0).
    d.extend_from_slice(&[format.color_model(), 1, 1, 0]);
    // texelBlockDimension0..3 (dimension - 1): 4x4x1x1.
    d.extend_from_slice(&[3, 3, 0, 0]);
    // bytesPlane0..7.
    d.extend_from_slice(&[format.block_bytes() as u8, 0, 0, 0, 0, 0, 0, 0]);
    for &(bit_offset, bit_length, channel) in samples {
        d.extend_from_slice(&bit_offset.to_le_bytes());
        d.push(bit_length);
        d.push(channel);
        d.extend_from_slice(&[0, 0, 0, 0]); // samplePosition0..3
        d.extend_from_slice(&0u32.to_le_bytes()); // sampleLower
        d.extend_from_slice(&u32::MAX.to_le_bytes()); // sampleUpper
    }
    debug_assert_eq!(d.len(), 4 + block_size);
    d
}

fn key_value_data() -> Vec<u8> {
    let mut kvd = Vec::new();
    // Keys sorted by codepoint, values NUL-terminated, each entry padded to 4 bytes.
    for (k, v) in [("KTXorientation", "rd"), ("KTXwriter", WRITER)] {
        let len = k.len() + 1 + v.len() + 1;
        kvd.extend_from_slice(&(len as u32).to_le_bytes());
        kvd.extend_from_slice(k.as_bytes());
        kvd.push(0);
        kvd.extend_from_slice(v.as_bytes());
        kvd.push(0);
        while !kvd.len().is_multiple_of(4) {
            kvd.push(0);
        }
    }
    kvd
}

pub fn zstd(data: &[u8]) -> Vec<u8> {
    ruzstd::encoding::compress_to_vec(data, ZSTD_LEVEL)
}

/// Write a KTX2 of `levels` (level 0 first, each exactly [`level_bytes`] long, rows top-down)
/// of a `width x height` texture.
pub fn write_ktx2(format: BcFormat, width: u32, height: u32, levels: &[&[u8]]) -> Result<Vec<u8>> {
    if levels.is_empty() || width == 0 || height == 0 {
        bail!("ktx2: empty texture");
    }
    for (m, l) in levels.iter().enumerate() {
        let want = level_bytes(format, (width >> m).max(1), (height >> m).max(1));
        if l.len() != want {
            bail!("ktx2: level {m} is {} bytes, expected {want}", l.len());
        }
    }
    let n = levels.len();
    let index_len = 24 * n;
    let dfd = basic_dfd(format);
    let kvd = key_value_data();
    let dfd_offset = 80 + index_len;
    let kvd_offset = dfd_offset + dfd.len();
    let data_start = kvd_offset + kvd.len();

    let compressed: Vec<Vec<u8>> = levels.iter().map(|l| zstd(l)).collect::<Vec<_>>();
    // Smallest level first in the file.
    let mut offsets = vec![0u64; n];
    let mut at = data_start;
    for m in (0..n).rev() {
        offsets[m] = at as u64;
        at += compressed[m].len();
    }

    let mut out = Vec::with_capacity(at);
    out.extend_from_slice(crate::ktx2::KTX2_MAGIC);
    for v in [
        format.vk_format(),
        1, // typeSize: 1 for block-compressed formats
        width,
        height,
        0, // pixelDepth
        0, // layerCount
        1, // faceCount
        n as u32,
        SUPERCOMPRESSION_ZSTD,
        dfd_offset as u32,
        dfd.len() as u32,
        kvd_offset as u32,
        kvd.len() as u32,
    ] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&0u64.to_le_bytes()); // sgdByteOffset
    out.extend_from_slice(&0u64.to_le_bytes()); // sgdByteLength
    for ((off, comp), level) in offsets.iter().zip(&compressed).zip(levels) {
        out.extend_from_slice(&off.to_le_bytes());
        out.extend_from_slice(&(comp.len() as u64).to_le_bytes());
        out.extend_from_slice(&(level.len() as u64).to_le_bytes());
    }
    debug_assert_eq!(out.len(), dfd_offset);
    out.extend_from_slice(&dfd);
    out.extend_from_slice(&kvd);
    for comp in compressed.iter().rev() {
        out.extend_from_slice(comp);
    }
    Ok(out)
}

/// Read back every level of a file this module wrote (or any Zstandard-supercompressed KTX2),
/// level 0 first, decompressed. For tests and checks; `None` on anything malformed.
pub fn read_levels(bytes: &[u8]) -> Option<Vec<Vec<u8>>> {
    use ruzstd::io::Read;
    let h = crate::ktx2::read_header(bytes)?;
    let mut out = Vec::with_capacity(h.levels as usize);
    for m in 0..h.levels.max(1) {
        let (off, len, raw) = crate::ktx2::level_index(bytes, m)?;
        let start = usize::try_from(off).ok()?;
        let comp = bytes.get(start..start.checked_add(usize::try_from(len).ok()?)?)?;
        let raw = usize::try_from(raw).ok()?;
        let level = match h.supercompression {
            0 => comp.to_vec(),
            SUPERCOMPRESSION_ZSTD => {
                let mut dec = ruzstd::decoding::StreamingDecoder::new(comp).ok()?;
                let mut buf = vec![0u8; raw];
                dec.read_exact(&mut buf).ok()?;
                buf
            }
            _ => return None,
        };
        if level.len() != raw {
            return None;
        }
        out.push(level);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(format: BcFormat, w: u32, h: u32, mips: u32) -> Vec<Vec<u8>> {
        (0..mips)
            .map(|m| {
                let n = level_bytes(format, (w >> m).max(1), (h >> m).max(1));
                (0..n).map(|i| (i * 7 + m as usize) as u8).collect()
            })
            .collect()
    }

    #[test]
    fn round_trips_through_the_ktx2_header_reader() {
        for (format, w, h) in [
            (BcFormat::Bc7, 16u32, 8u32),
            (BcFormat::Bc1Rgb, 8, 8),
            (BcFormat::Bc5, 32, 4),
        ] {
            let mips = 32 - w.max(h).leading_zeros();
            let levels = chain(format, w, h, mips);
            let refs: Vec<&[u8]> = levels.iter().map(Vec::as_slice).collect();
            let bytes = write_ktx2(format, w, h, &refs).unwrap();
            assert_eq!(bytes, write_ktx2(format, w, h, &refs).unwrap(), "deterministic");

            assert!(crate::ktx2::is_ktx2(&bytes));
            let hd = crate::ktx2::read_header(&bytes).unwrap();
            assert_eq!(hd.vk_format, format.vk_format());
            assert_eq!((hd.type_size, hd.width, hd.height), (1, w, h));
            assert_eq!((hd.depth, hd.layers, hd.faces), (0, 0, 1));
            assert_eq!(hd.levels, mips);
            assert_eq!(hd.supercompression, SUPERCOMPRESSION_ZSTD);
            assert_eq!((hd.sgd_offset, hd.sgd_len), (0, 0));
            assert_eq!(hd.dfd_offset as usize, 80 + 24 * mips as usize);
            // The colour model sits where the UASTC gate reads it; this file is BC, not
            // UASTC, so that gate (and the UASTC transcoder) correctly turn it away.
            let dfd = crate::ktx2::dfd(&bytes, &hd).unwrap();
            assert_eq!(dfd.get(12), Some(&format.color_model()));
            assert_eq!(
                u32::from_le_bytes(dfd[..4].try_into().unwrap()) as usize,
                dfd.len()
            );
            assert!(!crate::ktx2::ktx2_header_ok(&bytes));
            // KVD carries the orientation.
            let kvd = &bytes[hd.kvd_offset as usize..(hd.kvd_offset + hd.kvd_len) as usize];
            assert!(kvd.windows(17).any(|x| x == b"KTXorientation\0rd"));
            // Smallest level first, levels back to back.
            let (off0, _, _) = crate::ktx2::level_index(&bytes, 0).unwrap();
            let (off_last, len_last, _) = crate::ktx2::level_index(&bytes, mips - 1).unwrap();
            assert!(off_last < off0 || mips == 1);
            assert_eq!(off_last, (hd.kvd_offset + hd.kvd_len) as u64);
            assert!(off_last + len_last <= bytes.len() as u64);
            assert_eq!(read_levels(&bytes).unwrap(), levels);
        }
    }

    #[test]
    fn level_sizes_and_chain_split() {
        assert_eq!(level_bytes(BcFormat::Bc7, 1, 1), 16);
        assert_eq!(level_bytes(BcFormat::Bc1Rgb, 8, 4), 16);
        assert_eq!(level_bytes(BcFormat::Bc5, 2, 2), 16);
        let data = vec![0u8; 64 + 16 + 16 + 16];
        let parts = split_chain(BcFormat::Bc7, 8, 8, 4, &data).unwrap();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            [64, 16, 16, 16]
        );
        assert!(split_chain(BcFormat::Bc7, 8, 8, 3, &data).is_err());
        assert!(write_ktx2(BcFormat::Bc7, 8, 8, &[&[0u8; 63][..]]).is_err());
    }
}

//! Texture variants: PNG / JPEG in, KTX2 (BC1 / BC5 / BC7 payload, Zstandard-supercompressed,
//! level 0 only with no mip chain unless `TexOptions::mips`, at most [`BEVY_TEXTURE_MAX`] on a
//! side by default, top-down rows) out; creator UASTC KTX2 in, a single-level pure block
//! transcode to BC7 KTX2 out. There is no size gate: a variant larger than its original is still
//! the one served, because the explorer never draws a PNG/JPEG as is (it decodes it to RGBA8 and
//! converts it on the player's CPU). See the module docs of [`super`].

use anyhow::{Context, Result};
use image::RgbaImage;

use super::stats::{timed, NoVariant, SourceKind, StageTimes};
use super::BEVY_TEXTURE_MAX;
use crate::bc7_pure::Bc7Profile;
use crate::ktx2_write::{split_chain, write_ktx2, BcFormat, DEFAULT_ZSTD_LEVEL};

/// Encoder settings that change variant bytes (and so CIDs). The scene lanes use the defaults
/// apart from `zstd_level`; the far LOD copy ([`super::farlod`]) turns on mips and the
/// unused-alpha rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TexOptions {
    /// Zstandard level of every KTX2 level (`crate::ktx2_write::check_zstd_level`).
    pub zstd_level: i32,
    /// Longest side of a PNG/JPEG encode (powers of two). The UASTC transcode ignores it: it
    /// picks the level the explorer's local path picks.
    pub max_side: u32,
    /// Write the full mip chain down to 1x1 instead of level 0 only (PNG/JPEG encodes only).
    pub mips: bool,
    /// GLB rewrites only: an embedded image whose alpha channel no material reads (see
    /// `model::alpha_unused_images`) is encoded as opaque, so it can take BC1. Standalone
    /// texture variants never see it: their key does not say how the alpha is used.
    pub drop_unused_alpha: bool,
}

impl Default for TexOptions {
    fn default() -> Self {
        TexOptions {
            zstd_level: DEFAULT_ZSTD_LEVEL,
            max_side: BEVY_TEXTURE_MAX,
            mips: false,
            drop_unused_alpha: false,
        }
    }
}

/// No variant, and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Miss {
    pub reason: NoVariant,
}

impl From<NoVariant> for Miss {
    fn from(reason: NoVariant) -> Self {
        Miss { reason }
    }
}

/// An opaque sRGB image keeps its BC1 encode only when level 0 decodes to at least this PSNR
/// (dB, RGB, against the source at the variant size); below it the image gets BC7. Posters,
/// text and other hard-edged art fall below it (BC1 has four colours per block); photos and
/// smooth textures stay well above.
pub const BC1_MIN_PSNR_DB: f64 = 36.0;

/// Decode one BC1 level (`w x h` texels, both multiples of 4, rows top-down) to RGBA8 with
/// alpha 255. Both block modes are decoded; the three-colour mode's fourth index gives black.
pub fn decode_bc1_rgba(blocks: &[u8], w: u32, h: u32) -> Vec<u8> {
    use crate::gpu::corelib::dxt1::unpack_565;
    let (w, h) = (w as usize, h as usize);
    let bw = w.div_ceil(4);
    let mut out = vec![0u8; w * h * 4];
    for (bi, block) in blocks.chunks_exact(8).enumerate() {
        let (bx, by) = (bi % bw, bi / bw);
        let c0 = u16::from_le_bytes([block[0], block[1]]);
        let c1 = u16::from_le_bytes([block[2], block[3]]);
        let p0 = unpack_565(c0);
        let p1 = unpack_565(c1);
        let mix = |a: u8, b: u8, wa: u32, wb: u32, div: u32| -> u8 {
            ((u32::from(a) * wa + u32::from(b) * wb + div / 2) / div) as u8
        };
        let palette: [[u8; 3]; 4] = if c0 > c1 {
            [
                p0,
                p1,
                [
                    mix(p0[0], p1[0], 2, 1, 3),
                    mix(p0[1], p1[1], 2, 1, 3),
                    mix(p0[2], p1[2], 2, 1, 3),
                ],
                [
                    mix(p0[0], p1[0], 1, 2, 3),
                    mix(p0[1], p1[1], 1, 2, 3),
                    mix(p0[2], p1[2], 1, 2, 3),
                ],
            ]
        } else {
            [
                p0,
                p1,
                [
                    mix(p0[0], p1[0], 1, 1, 2),
                    mix(p0[1], p1[1], 1, 1, 2),
                    mix(p0[2], p1[2], 1, 1, 2),
                ],
                [0, 0, 0],
            ]
        };
        for ty in 0..4 {
            let row = block[4 + ty];
            for tx in 0..4 {
                let (x, y) = (bx * 4 + tx, by * 4 + ty);
                if x >= w || y >= h {
                    continue;
                }
                let c = palette[usize::from((row >> (tx * 2)) & 3)];
                let o = (y * w + x) * 4;
                out[o..o + 3].copy_from_slice(&c);
                out[o + 3] = 255;
            }
        }
    }
    out
}

/// PSNR in dB of `b` against `a` over the RGB channels of two RGBA8 buffers of the same size
/// (`f64::INFINITY` when identical).
pub fn psnr_rgb_db(a: &[u8], b: &[u8]) -> f64 {
    debug_assert_eq!(a.len(), b.len());
    let mut se: u64 = 0;
    let mut n: u64 = 0;
    for (pa, pb) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
        for c in 0..3 {
            let d = i64::from(pa[c]) - i64::from(pb[c]);
            se += (d * d) as u64;
        }
        n += 3;
    }
    if se == 0 {
        return f64::INFINITY;
    }
    let mse = se as f64 / n.max(1) as f64;
    10.0 * (255.0f64 * 255.0 / mse).log10()
}

/// How the entity's models sample an image.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Slot {
    /// Colour data (base colour, emissive, specular colour) or unknown use: sRGB.
    Srgb,
    /// Linear data (metallic-roughness, occlusion).
    Linear,
    /// A normal map and nothing else.
    Normal,
}

/// Every use an image has across the entity's models.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Uses {
    pub srgb: bool,
    pub normal: bool,
    pub linear: bool,
}

impl From<super::ImageUse> for Uses {
    fn from(u: super::ImageUse) -> Self {
        Uses {
            srgb: u.srgb,
            normal: u.normal,
            linear: u.linear,
        }
    }
}

impl From<Uses> for super::ImageUse {
    fn from(u: Uses) -> Self {
        super::ImageUse {
            srgb: u.srgb,
            normal: u.normal,
            linear: u.linear,
        }
    }
}

impl Slot {
    /// The slot's name in a variant key (`{sourceCid}_{slot}`).
    pub fn name(self) -> &'static str {
        match self {
            Slot::Srgb => "srgb",
            Slot::Linear => "linear",
            Slot::Normal => "normal",
        }
    }
}

impl Uses {
    pub fn merge(&mut self, other: Uses) {
        self.srgb |= other.srgb;
        self.normal |= other.normal;
        self.linear |= other.linear;
    }

    /// Colour use wins (it is the visible one); BC5 is taken only when every use is a normal
    /// map; an image no model binds (scene-code textures) is treated as colour.
    pub fn slot(self) -> Slot {
        if self.srgb {
            Slot::Srgb
        } else if self.normal && !self.linear {
            Slot::Normal
        } else if self.linear {
            Slot::Linear
        } else {
            Slot::Srgb
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TexFormat {
    Bc7,
    Bc1,
    Bc5,
}

impl TexFormat {
    pub fn manifest_name(self) -> &'static str {
        match self {
            TexFormat::Bc7 => "ktx2-bc7",
            TexFormat::Bc1 => "ktx2-bc1",
            TexFormat::Bc5 => "ktx2-bc5",
        }
    }

    pub fn bc(self) -> BcFormat {
        match self {
            TexFormat::Bc7 => BcFormat::Bc7,
            TexFormat::Bc1 => BcFormat::Bc1Rgb,
            TexFormat::Bc5 => BcFormat::Bc5,
        }
    }

    /// Bytes per 4x4 block.
    pub fn block_bytes(self) -> usize {
        self.bc().block_bytes()
    }
}

/// The format rule: BC5 for normal-only images, BC1 when every texel is opaque, else BC7. An
/// opaque sRGB image's BC1 is then checked against [`BC1_MIN_PSNR_DB`] and replaced by BC7 when
/// it falls short (`encode_rgba`); linear images keep BC1.
pub fn choose_format(slot: Slot, opaque: bool) -> TexFormat {
    match slot {
        Slot::Normal => TexFormat::Bc5,
        _ if opaque => TexFormat::Bc1,
        _ => TexFormat::Bc7,
    }
}

/// Which path produced a variant (exposed for tests and logs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodePath {
    /// Creator UASTC KTX2: the explorer's chosen level block-transcoded to BC7, nothing else.
    Ktx2Transcode,
    /// PNG/JPEG decoded to RGBA8 and encoded.
    Encode,
}

#[derive(Clone, Debug)]
pub struct TextureVariant {
    /// The KTX2 file.
    pub bytes: Vec<u8>,
    pub format: TexFormat,
    pub width: u32,
    pub height: u32,
    pub mips: u32,
    pub path: EncodePath,
    /// Opaque sRGB images: the PSNR of the BC1 encode's level 0 against the source.
    pub bc1_psnr_db: Option<f64>,
    /// The BC1 encode fell below [`BC1_MIN_PSNR_DB`] and this variant is BC7 instead.
    pub bc1_rejected: bool,
    /// The source had non-opaque texels, but no material reads its alpha, so it was encoded
    /// as opaque (`TexOptions::drop_unused_alpha`).
    pub alpha_dropped: bool,
}

impl TextureVariant {
    /// GPU bytes of every level the KTX2 carries.
    pub fn gpu_bytes(&self) -> u64 {
        chain_gpu_bytes(self.format.bc(), self.width, self.height, self.mips)
    }
}

/// GPU bytes of a BC level 0 of `w x h` and its first `mips - 1` mips.
pub fn chain_gpu_bytes(format: BcFormat, w: u32, h: u32, mips: u32) -> u64 {
    (0..mips.max(1))
        .map(|m| crate::ktx2_write::level_bytes(format, (w >> m).max(1), (h >> m).max(1)) as u64)
        .sum()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Png,
    Jpeg,
    Ktx2,
}

/// The source type by bytes, for stats.
pub fn source_kind(bytes: &[u8]) -> SourceKind {
    match sniff(bytes) {
        Some(Source::Png) => SourceKind::Png,
        Some(Source::Jpeg) => SourceKind::Jpeg,
        Some(Source::Ktx2) => SourceKind::Ktx2,
        None => SourceKind::Other,
    }
}

fn sniff(bytes: &[u8]) -> Option<Source> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(Source::Png)
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(Source::Jpeg)
    } else if crate::ktx2::is_ktx2(bytes) {
        Some(Source::Ktx2)
    } else {
        None
    }
}

/// Most source pixels of a PNG/JPEG the lane decodes (from its header): 2^28, 1 GiB as RGBA8.
/// Larger images get no variant (`too-large`) and the client loads the original.
pub const MAX_SOURCE_PIXELS: u64 = 1 << 28;

/// Images this small get no variant: the original is as cheap as anything we could ship.
pub fn is_tiny(w: u32, h: u32) -> bool {
    w <= 2 || h <= 2
}

/// Build the texture variant of `bytes` for `slot`. `Ok(None)` means "no variant": not a PNG,
/// JPEG or UASTC KTX2 (sniffed by bytes, never by name), tiny, or a KTX2 the pure transcode
/// does not take. `Err` is a PNG/JPEG that does not decode.
pub fn encode_texture(bytes: &[u8], slot: Slot) -> Result<Option<TextureVariant>> {
    encode_texture_with(
        bytes,
        slot,
        TexOptions::default(),
        &mut StageTimes::default(),
    )
    .map(Result::ok)
}

/// [`encode_texture`] with explicit settings, adding each stage's time to `times` and saying
/// why there is no variant. Not gated on size (see the module docs).
pub fn encode_texture_with(
    bytes: &[u8],
    slot: Slot,
    opts: TexOptions,
    times: &mut StageTimes,
) -> Result<std::result::Result<TextureVariant, Miss>> {
    encode_image(bytes, slot, false, opts, times)
}

/// [`encode_texture_with`] for an image whose caller knows how its alpha is used:
/// `alpha_unused` (honoured only with `opts.drop_unused_alpha`) encodes it as opaque.
pub fn encode_image(
    bytes: &[u8],
    slot: Slot,
    alpha_unused: bool,
    opts: TexOptions,
    times: &mut StageTimes,
) -> Result<std::result::Result<TextureVariant, Miss>> {
    let Some(source) = sniff(bytes) else {
        return Ok(Err(NoVariant::NotAnImage.into()));
    };
    if source == Source::Ktx2 {
        return Ok(transcode_uastc_bc7_with(bytes, opts, times).map_err(Miss::from));
    }
    debug_assert!(matches!(source, Source::Png | Source::Jpeg));
    // The header's size first: a small, highly compressible file can decode to gigabytes.
    if let Ok((w, h)) = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(anyhow::Error::from)
        .and_then(|r| r.into_dimensions().map_err(anyhow::Error::from))
    {
        if u64::from(w) * u64::from(h) > MAX_SOURCE_PIXELS {
            return Ok(Err(NoVariant::TooLarge.into()));
        }
    }
    let img =
        timed(&mut times.decode_ns, || image::load_from_memory(bytes)).context("decode image")?;
    let img = timed(&mut times.decode_ns, || img.into_rgba8());
    // Tiny at the source, or at the variant size: an elongated source (4096x8) downscales to a
    // level 0 thinner than one 4x4 BC block (1024x2), which wgpu refuses.
    // Capped to `opts.max_side` and snapped to powers of two, as the bvwebgpu lane does: powers
    // of two of at least 4 are whole 4x4 blocks, which wgpu requires of a BC level 0.
    let (tw, th) = crate::texprofile::bc7_target_size(img.width(), img.height(), opts.max_side);
    if is_tiny(img.width(), img.height()) || tw < 4 || th < 4 {
        return Ok(Err(NoVariant::Tiny.into()));
    }
    let alpha_unused = alpha_unused && opts.drop_unused_alpha;
    encode_rgba(img, slot, alpha_unused, opts, times).map(Ok)
}

/// The creator-KTX2 rule: a pure block-for-block UASTC -> BC7 transcode of exactly the one level
/// the explorer's local path transcodes (the first level no larger than [`BEVY_TEXTURE_MAX`] on
/// a side, at its own size), and nothing else: one level, no mips, so remote and localhost
/// produce the same texture (both vendor the same basisu sources). The slot is ignored: no BC1,
/// no BC5, no resize, no re-encode, no generated mips. Keeping the file's own lower mips would
/// need the explorer's local transcode to keep them too (a separate change).
///
/// `None` (no variant: the client loads the original) when the file is not a UASTC KTX2 the
/// header gate takes, has no such level, that level is not whole 4x4 blocks (the explorer
/// would decode and re-encode it), or it fails to transcode.
pub fn transcode_uastc_bc7(raw: &[u8]) -> Option<TextureVariant> {
    transcode_uastc_bc7_with(raw, TexOptions::default(), &mut StageTimes::default()).ok()
}

/// [`transcode_uastc_bc7`] with explicit settings, stage times and the reason for no variant.
pub fn transcode_uastc_bc7_with(
    raw: &[u8],
    opts: TexOptions,
    times: &mut StageTimes,
) -> std::result::Result<TextureVariant, NoVariant> {
    use basisu::{DecodeFlags, TargetFormat, Transcoder};
    if !crate::ktx2::ktx2_header_ok(raw) {
        return Err(NoVariant::Ktx2HeaderGate);
    }
    let t = timed(&mut times.transcode_ns, || Transcoder::new(raw))
        .map_err(|_| NoVariant::Ktx2TranscodeFailed)?;
    if !t.supports(TargetFormat::Bc7Rgba) {
        return Err(NoVariant::Ktx2TranscodeFailed);
    }
    let level = (0..t.level_count())
        .find(|&l| {
            t.image_level_info(l)
                .is_ok_and(|i| i.width.max(i.height) <= BEVY_TEXTURE_MAX)
        })
        .ok_or(NoVariant::Ktx2NoLevelInRange)?;
    let info = t
        .image_level_info(level)
        .map_err(|_| NoVariant::Ktx2TranscodeFailed)?;
    let (w, h) = (info.width, info.height);
    if !w.is_multiple_of(4) || !h.is_multiple_of(4) || is_tiny(w, h) {
        return Err(NoVariant::Ktx2NotWholeBlocks);
    }
    let blocks = timed(&mut times.transcode_ns, || {
        t.transcode(level, TargetFormat::Bc7Rgba, DecodeFlags::NONE)
    })
    .map_err(|_| NoVariant::Ktx2TranscodeFailed)?;
    if blocks.len() != crate::ktx2_write::level_bytes(BcFormat::Bc7, w, h) {
        return Err(NoVariant::Ktx2TranscodeFailed);
    }
    let bytes = timed(&mut times.zstd_ns, || {
        write_ktx2(BcFormat::Bc7, w, h, &[blocks.as_slice()], opts.zstd_level)
    })
    .map_err(|_| NoVariant::EncodeError)?;
    Ok(TextureVariant {
        bytes,
        format: TexFormat::Bc7,
        width: w,
        height: h,
        mips: 1,
        path: EncodePath::Ktx2Transcode,
        bc1_psnr_db: None,
        bc1_rejected: false,
        alpha_dropped: false,
    })
}

/// Levels the PNG/JPEG encode writes by default: level 0 only, no mip chain, so the variant
/// compares like for like with the single-level creator UASTC KTX2 files (and the UASTC
/// transcode path). `TexOptions::mips` writes the full chain instead (`None`).
const LEVELS: Option<i32> = Some(1);

fn encode_rgba(
    img: RgbaImage,
    slot: Slot,
    alpha_unused: bool,
    opts: TexOptions,
    times: &mut StageTimes,
) -> Result<TextureVariant> {
    let levels = if opts.mips { None } else { LEVELS };
    let (w, h) = img.dimensions();
    let (tw, th) = crate::texprofile::bc7_target_size(w, h, opts.max_side);
    let srgb = slot == Slot::Srgb;
    let mut buf = if (tw, th) != (w, h) {
        timed(&mut times.resize_ns, || {
            crate::resize::box_downscale_rgba(
                img.as_raw(),
                w as usize,
                h as usize,
                tw as usize,
                th as usize,
                srgb,
            )
        })
    } else {
        img.into_raw()
    };
    let mut opaque = buf.iter().skip(3).step_by(4).all(|&a| a == 255);
    // No material reads the alpha: make it opaque, so the image takes the opaque rules (BC1
    // behind the PSNR check, else an opaque BC7 with no alpha bleed).
    let alpha_dropped = alpha_unused && !opaque;
    if alpha_dropped {
        for a in buf.iter_mut().skip(3).step_by(4) {
            *a = 255;
        }
        opaque = true;
    }
    let mut format = choose_format(slot, opaque);
    let mut bc1_psnr_db = None;
    let mut bc1_rejected = false;
    let encode = |format: TexFormat, buf: &mut Vec<u8>, times: &mut StageTimes| -> (Vec<u8>, i32) {
        let started = std::time::Instant::now();
        let out = match format {
            // Opaque blocks only: the encoder always orders endpoints c0 > c1 (four-colour
            // mode), so no texel decodes as BC1's transparent black.
            TexFormat::Bc1 => {
                crate::dxt1_pure::encode_dxt1_mip_chain(buf, tw, th, levels, false, srgb)
            }
            TexFormat::Bc5 => {
                // The BC5 kernel packs (A, G) into (R, G) - Unity's DXT5nm layout. Bevy reads
                // a two-channel normal map as (x, y) = (R, G), so put x into alpha first.
                for px in buf.chunks_exact_mut(4) {
                    px[3] = px[0];
                }
                crate::bc5_pure::encode_bc5_mip_chain(buf, tw, th, levels, false)
            }
            TexFormat::Bc7 => {
                if !opaque {
                    crate::alpha_bleed::alpha_bleed_inplace(buf, tw, th);
                }
                crate::bc7_pure::encode_bc7_mip_chain_with_profile(
                    buf,
                    tw,
                    th,
                    levels,
                    false,
                    srgb,
                    srgb,
                    Bc7Profile::Basic,
                )
            }
        };
        let slot_ns = match format {
            TexFormat::Bc1 => &mut times.bc1_ns,
            TexFormat::Bc5 => &mut times.bc5_ns,
            TexFormat::Bc7 => &mut times.bc7_ns,
        };
        *slot_ns += super::stats::elapsed_ns(started);
        out
    };
    let (mut data, mut mips) = encode(format, &mut buf, times);
    if format == TexFormat::Bc1 && srgb {
        // The BC1 rule for colour data: keep it only when level 0 is close enough to the source.
        // Linear (ORM) images keep BC1 regardless.
        let psnr = timed(&mut times.bc1_check_ns, || {
            let level0 = &data[..crate::ktx2_write::level_bytes(BcFormat::Bc1Rgb, tw, th)];
            psnr_rgb_db(&buf, &decode_bc1_rgba(level0, tw, th))
        });
        bc1_psnr_db = Some(psnr);
        if psnr < BC1_MIN_PSNR_DB {
            bc1_rejected = true;
            format = TexFormat::Bc7;
            (data, mips) = encode(format, &mut buf, times);
        }
    }
    let levels = split_chain(format.bc(), tw, th, mips as u32, &data)?;
    let bytes = timed(&mut times.zstd_ns, || {
        write_ktx2(format.bc(), tw, th, &levels, opts.zstd_level)
    })?;
    Ok(TextureVariant {
        bytes,
        format,
        width: tw,
        height: th,
        mips: mips as u32,
        path: EncodePath::Encode,
        bc1_psnr_db,
        bc1_rejected,
        alpha_dropped,
    })
}

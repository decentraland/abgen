//! Texture variants: PNG / JPEG / UASTC KTX2 in, DDS (DX10, UNORM, full mips, at most
//! [`BEVY_TEXTURE_MAX`] on a side) out. See the module docs of [`super`] for the format rules.

use anyhow::{Context, Result};
use image::RgbaImage;

use super::BEVY_TEXTURE_MAX;
use crate::bc7_pure::Bc7Profile;
use crate::bvwebgpu::emit;

/// How the entity's models sample an image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
            TexFormat::Bc7 => "dds-bc7",
            TexFormat::Bc1 => "dds-bc1",
            TexFormat::Bc5 => "dds-bc5",
        }
    }

    fn dxgi(self) -> ddsfile::DxgiFormat {
        match self {
            TexFormat::Bc7 => ddsfile::DxgiFormat::BC7_UNorm,
            TexFormat::Bc1 => ddsfile::DxgiFormat::BC1_UNorm,
            TexFormat::Bc5 => ddsfile::DxgiFormat::BC5_UNorm,
        }
    }

    fn alpha_mode(self) -> ddsfile::AlphaMode {
        match self {
            TexFormat::Bc7 => ddsfile::AlphaMode::Straight,
            TexFormat::Bc1 => ddsfile::AlphaMode::Opaque,
            TexFormat::Bc5 => ddsfile::AlphaMode::Unknown,
        }
    }

    /// Bytes per 4x4 block.
    pub fn block_bytes(self) -> usize {
        match self {
            TexFormat::Bc1 => 8,
            TexFormat::Bc7 | TexFormat::Bc5 => 16,
        }
    }
}

/// The format rule: BC5 for normal-only images, BC1 when every texel is opaque, else BC7.
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
    /// UASTC blocks transcoded to BC7 (levels the KTX2 lacks are re-encoded).
    Ktx2Transcode,
    /// Decoded to RGBA8 and encoded.
    Encode,
}

#[derive(Clone, Debug)]
pub struct TextureVariant {
    pub dds: Vec<u8>,
    pub format: TexFormat,
    pub width: u32,
    pub height: u32,
    pub mips: u32,
    pub path: EncodePath,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Png,
    Jpeg,
    Ktx2,
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

/// Images this small get no variant: the original is as cheap as anything we could ship.
pub fn is_tiny(w: u32, h: u32) -> bool {
    w <= 2 || h <= 2
}

/// The variant size: capped to [`BEVY_TEXTURE_MAX`] and snapped to powers of two, as the
/// bvwebgpu lane does. Powers of two of at least 4 are whole 4x4 blocks, which wgpu requires of
/// a block-compressed level 0.
pub fn target_size(w: u32, h: u32) -> (u32, u32) {
    crate::texprofile::bc7_target_size(w, h, BEVY_TEXTURE_MAX)
}

/// Build the texture variant of `bytes` for `slot`. `Ok(None)` means "no variant": not a PNG,
/// JPEG or accepted UASTC KTX2 (sniffed by bytes, never by name), or tiny. `Err` is a file that
/// claims a supported format but does not decode.
pub fn encode_texture(bytes: &[u8], slot: Slot) -> Result<Option<TextureVariant>> {
    let Some(source) = sniff(bytes) else {
        return Ok(None);
    };
    if source == Source::Ktx2 {
        return encode_ktx2(bytes, slot);
    }
    debug_assert!(matches!(source, Source::Png | Source::Jpeg));
    let img = image::load_from_memory(bytes)
        .context("decode image")?
        .to_rgba8();
    if is_tiny(img.width(), img.height()) {
        return Ok(None);
    }
    encode_rgba(img, slot).map(Some)
}

fn encode_ktx2(bytes: &[u8], slot: Slot) -> Result<Option<TextureVariant>> {
    use basisu::Transcoder;
    if !crate::ktx2::ktx2_header_ok(bytes) {
        return Ok(None);
    }
    let Some((w, h)) = Transcoder::new(bytes)
        .ok()
        .and_then(|t| t.image_level_info(0).ok())
        .map(|i| (i.width, i.height))
    else {
        return Ok(None);
    };
    if is_tiny(w, h) {
        return Ok(None);
    }
    let (tw, th) = target_size(w, h);
    if slot != Slot::Normal {
        if let Some((data, mips)) = ktx2_bc7_chain(bytes, tw, th, slot == Slot::Srgb) {
            let dds = emit::write_dds(
                TexFormat::Bc7.dxgi(),
                TexFormat::Bc7.alpha_mode(),
                tw,
                th,
                data,
                mips,
            )?;
            return Ok(Some(TextureVariant {
                dds,
                format: TexFormat::Bc7,
                width: tw,
                height: th,
                mips: mips as u32,
                path: EncodePath::Ktx2Transcode,
            }));
        }
    }
    let Some(img) = crate::ktx2::decode_ktx2_uastc(bytes) else {
        return Ok(None);
    };
    encode_rgba(img, slot).map(Some)
}

/// Block-for-block UASTC -> BC7 for every KTX2 level from the one sized `tw x th` down; the
/// levels the file does not carry are built from the last transcoded level (decoded, halved,
/// BC7-encoded). `None` when no level has the target size, the size is not whole blocks, or
/// the transcoder refuses: the caller then decodes and re-encodes.
fn ktx2_bc7_chain(raw: &[u8], tw: u32, th: u32, srgb: bool) -> Option<(Vec<u8>, i32)> {
    use basisu::{DecodeFlags, TargetFormat, Transcoder};
    if !tw.is_multiple_of(4) || !th.is_multiple_of(4) {
        return None;
    }
    let t = Transcoder::new(raw).ok()?;
    if !t.supports(TargetFormat::Bc7Rgba) {
        return None;
    }
    let levels = t.level_count();
    let start = (0..levels).find(|&l| {
        t.image_level_info(l)
            .is_ok_and(|i| (i.width, i.height) == (tw, th))
    })?;
    let total = crate::bc7_pure::compute_default_mip_count(tw, th);
    let mut data = Vec::with_capacity(crate::bc7_pure::compute_mip_chain_size(tw, th, total));
    let mut mips = 0i32;
    let mut level = start;
    while mips < total && level < levels {
        let info = t.image_level_info(level).ok()?;
        let (ew, eh) = ((tw >> mips).max(1), (th >> mips).max(1));
        if (info.width, info.height) != (ew, eh) {
            break;
        }
        let blocks = t
            .transcode(level, TargetFormat::Bc7Rgba, DecodeFlags::NONE)
            .ok()?;
        if blocks.len() != (ew.div_ceil(4) * eh.div_ceil(4)) as usize * 16 {
            return None;
        }
        data.extend_from_slice(&blocks);
        mips += 1;
        level += 1;
    }
    if mips < total {
        let last = level - 1;
        let info = t.image_level_info(last).ok()?;
        let (lw, lh) = (info.width, info.height);
        let rgba = t
            .transcode(last, TargetFormat::Rgba32, DecodeFlags::NONE)
            .ok()?;
        if rgba.len() != (lw as usize) * (lh as usize) * 4 {
            return None;
        }
        let (nw, nh) = ((lw / 2).max(1), (lh / 2).max(1));
        let half = crate::resize::box_downscale_rgba(
            &rgba,
            lw as usize,
            lh as usize,
            nw as usize,
            nh as usize,
            srgb,
        );
        let want = total - mips;
        let (rest, n) = crate::bc7_pure::encode_bc7_mip_chain_with_profile(
            &half,
            nw,
            nh,
            Some(want),
            false,
            srgb,
            srgb,
            Bc7Profile::Basic,
        );
        if n != want {
            return None;
        }
        data.extend_from_slice(&rest);
        mips += n;
    }
    Some((data, mips))
}

fn encode_rgba(img: RgbaImage, slot: Slot) -> Result<TextureVariant> {
    let (w, h) = img.dimensions();
    let (tw, th) = target_size(w, h);
    let srgb = slot == Slot::Srgb;
    let mut buf = if (tw, th) != (w, h) {
        crate::resize::box_downscale_rgba(
            img.as_raw(),
            w as usize,
            h as usize,
            tw as usize,
            th as usize,
            srgb,
        )
    } else {
        img.into_raw()
    };
    let opaque = buf.iter().skip(3).step_by(4).all(|&a| a == 255);
    let format = choose_format(slot, opaque);
    let (data, mips) = match format {
        // Opaque blocks only: the encoder always orders endpoints c0 > c1 (four-colour mode),
        // so no texel decodes as BC1's transparent black.
        TexFormat::Bc1 => crate::dxt1_pure::encode_dxt1_mip_chain(&buf, tw, th, None, false, srgb),
        TexFormat::Bc5 => {
            // The BC5 kernel packs (A, G) into (R, G) - Unity's DXT5nm layout. Bevy reads a
            // two-channel normal map as (x, y) = (R, G), so put x into alpha first.
            for px in buf.chunks_exact_mut(4) {
                px[3] = px[0];
            }
            crate::bc5_pure::encode_bc5_mip_chain(&buf, tw, th, None, false)
        }
        TexFormat::Bc7 => {
            if !opaque {
                crate::alpha_bleed::alpha_bleed_inplace(&mut buf, tw, th);
            }
            crate::bc7_pure::encode_bc7_mip_chain_with_profile(
                &buf,
                tw,
                th,
                None,
                false,
                srgb,
                srgb,
                Bc7Profile::Basic,
            )
        }
    };
    let dds = emit::write_dds(format.dxgi(), format.alpha_mode(), tw, th, data, mips)?;
    Ok(TextureVariant {
        dds,
        format,
        width: tw,
        height: th,
        mips: mips as u32,
        path: EncodePath::Encode,
    })
}

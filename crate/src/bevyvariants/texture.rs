//! Texture variants: PNG / JPEG in, KTX2 (BC1 / BC5 / BC7 payload, Zstandard-supercompressed,
//! full mip chain, at most [`BEVY_TEXTURE_MAX`] on a side, top-down rows) out; creator UASTC
//! KTX2 in, a single-level pure block transcode to BC7 KTX2 out. See the module docs of [`super`].

use anyhow::{Context, Result};
use image::RgbaImage;

use super::BEVY_TEXTURE_MAX;
use crate::bc7_pure::Bc7Profile;
use crate::ktx2_write::{split_chain, write_ktx2, BcFormat};

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
/// JPEG or UASTC KTX2 (sniffed by bytes, never by name), tiny, or a KTX2 the pure transcode
/// does not take or does not make smaller. `Err` is a PNG/JPEG that does not decode.
pub fn encode_texture(bytes: &[u8], slot: Slot) -> Result<Option<TextureVariant>> {
    let Some(source) = sniff(bytes) else {
        return Ok(None);
    };
    if source == Source::Ktx2 {
        return Ok(transcode_uastc_bc7(bytes).filter(|v| v.bytes.len() <= bytes.len()));
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

/// The creator-KTX2 rule: a pure block-for-block UASTC -> BC7 transcode of exactly the one level
/// the explorer's local path transcodes (the first level no larger than [`BEVY_TEXTURE_MAX`] on
/// a side, at its own size), and nothing else: one level, no mips, so remote and localhost
/// produce the same texture (both vendor the same basisu sources). The slot is ignored: no BC1,
/// no BC5, no resize, no re-encode, no generated mips. Keeping the file's own lower mips would
/// need the explorer's local transcode to keep them too (a separate change).
///
/// `None` (no variant: the client loads the original) when the file is not a UASTC KTX2 the
/// header gate takes, has no such level, that level is not whole 4x4 blocks (the explorer
/// would decode and re-encode it), or it fails to transcode. The size gate against the
/// original is applied by the caller.
pub fn transcode_uastc_bc7(raw: &[u8]) -> Option<TextureVariant> {
    use basisu::{DecodeFlags, TargetFormat, Transcoder};
    if !crate::ktx2::ktx2_header_ok(raw) {
        return None;
    }
    let t = Transcoder::new(raw).ok()?;
    if !t.supports(TargetFormat::Bc7Rgba) {
        return None;
    }
    let level = (0..t.level_count()).find(|&l| {
        t.image_level_info(l)
            .is_ok_and(|i| i.width.max(i.height) <= BEVY_TEXTURE_MAX)
    })?;
    let info = t.image_level_info(level).ok()?;
    let (w, h) = (info.width, info.height);
    if !w.is_multiple_of(4) || !h.is_multiple_of(4) || is_tiny(w, h) {
        return None;
    }
    let blocks = t
        .transcode(level, TargetFormat::Bc7Rgba, DecodeFlags::NONE)
        .ok()?;
    if blocks.len() != crate::ktx2_write::level_bytes(BcFormat::Bc7, w, h) {
        return None;
    }
    let bytes = write_ktx2(BcFormat::Bc7, w, h, &[blocks.as_slice()]).ok()?;
    Some(TextureVariant {
        bytes,
        format: TexFormat::Bc7,
        width: w,
        height: h,
        mips: 1,
        path: EncodePath::Ktx2Transcode,
    })
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
    let levels = split_chain(format.bc(), tw, th, mips as u32, &data)?;
    let bytes = write_ktx2(format.bc(), tw, th, &levels)?;
    Ok(TextureVariant {
        bytes,
        format,
        width: tw,
        height: th,
        mips: mips as u32,
        path: EncodePath::Encode,
    })
}

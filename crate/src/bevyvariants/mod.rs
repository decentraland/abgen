//! **Prototype.** The Bevy variants lane: per scene entity, a manifest that maps each original
//! content CID to GPU-ready variants for the Bevy explorer, each stored under its own CID.
//!
//! - PNG/JPEG textures become KTX2 (`crate::ktx2_write`): BC1 / BC5 / BC7 UNORM blocks, each
//!   level Zstandard-supercompressed, full mip chain, longest side at most [`BEVY_TEXTURE_MAX`]
//!   (powers of two). BC5 for images the entity's models use only as normal maps, BC1 for fully
//!   opaque images whose BC1 level 0 stays within `texture::BC1_MIN_PSNR_DB` of the source
//!   (linear ORM images keep BC1 regardless), BC7 otherwise.
//! - Every texture variant must be smaller than its original file, or the original is served
//!   as is (`size-gate` in the stats).
//! - Creator UASTC KTX2 textures (standalone or embedded in a GLB) are only ever transcoded block
//!   for block to BC7, and only the single level the explorer's local path picks (the first
//!   level at most 1024 on a side, no mips), so remote equals localhost; anything that would
//!   need a resize or re-encode gets no variant. Keeping the file's lower mips needs the
//!   explorer's local path to keep them too.
//! - Models (opt-in: `ABGEN_BEVY_GLB_VARIANTS=1` or `abgen-corpus --bevy-glb-variants`; off by
//!   default because Bevy's glTF loader does not take them yet) become a GLB with Draco decoded, `KHR_texture_basisu` resolved and every embedded
//!   image replaced by its KTX2 variant (`image/ktx2`). External image URIs are left as they are:
//!   they resolve to their own original CID, which has its own texture variant.
//! - Nothing else gets a variant (scene code, audio, video, source maps, unknown files).
//!
//! The client contract: no manifest, no entry, or any failure means the client loads the
//! original file. Variants are content-addressed, so there is no version: a new encoder gives
//! new variant CIDs and a regenerated manifest.
//!
//! **Orientation.** Rows are stored top row first (`KTXorientation` = `rd`), which is what
//! Bevy's loaders and glTF's top-left UV origin expect. Decoded PNG/JPEG buffers and UASTC KTX2
//! (`"rd"`, required by `KHR_texture_basisu`) are already top-down, so every encoder here runs
//! with `flip = false`. This differs from the Unity bundle lanes, which flip vertically.
//!
//! On-disk (and object-storage) layout mirrors the served routes:
//! `bevy/manifest/{entity}.json` and `bevy/contents/{variantCid}`.

#[cfg(feature = "bevy-variants")]
mod build;
#[cfg(feature = "bevy-variants")]
pub mod model;
#[cfg(feature = "bevy-variants")]
pub mod stats;
#[cfg(feature = "bevy-variants")]
pub mod texture;

#[cfg(feature = "bevy-variants")]
pub use build::{build_entity_variants, BuildOptions, BuildOutput};

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The platform string the JIT lane and fail cache key this lane by.
pub const BEVY_PLATFORM: &str = "bevy";
/// Longest side of a texture variant.
pub const BEVY_TEXTURE_MAX: u32 = 1024;
/// Manifest schema version.
pub const MANIFEST_VERSION: u32 = 1;
/// Served (and stored) prefix of manifests.
pub const MANIFEST_PREFIX: &str = "bevy/manifest";
/// Served (and stored) prefix of variant bytes.
pub const CONTENTS_PREFIX: &str = "bevy/contents";

/// `"abgen <crate version>"`, stamped into every manifest.
pub fn generator() -> String {
    format!("abgen {}", env!("CARGO_PKG_VERSION"))
}

/// Entity ids and CIDs taken by the routes: non-empty ASCII alphanumerics only, so they are
/// always a single safe path component. Preview (`b64-…`) ids are refused: the lane is off for
/// preview scenes.
pub fn is_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

pub fn manifest_key(entity: &str) -> String {
    format!("{MANIFEST_PREFIX}/{entity}.json")
}

pub fn content_key(cid: &str) -> String {
    format!("{CONTENTS_PREFIX}/{cid}")
}

pub fn manifest_path(root: &Path, entity: &str) -> Option<PathBuf> {
    is_id(entity).then(|| {
        root.join("bevy")
            .join("manifest")
            .join(format!("{entity}.json"))
    })
}

pub fn content_path(root: &Path, cid: &str) -> Option<PathBuf> {
    is_id(cid).then(|| root.join("bevy").join("contents").join(cid))
}

/// CIDv1, raw codec (0x55), sha2-256 multihash, multibase base32 lower (`b…`): the `bafkrei…`
/// form catalysts use for content files, so the client verifies a variant with the same check
/// it applies to originals.
pub fn cid_v1_raw_sha256(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let digest = crate::hashes::sha256(bytes);
    let mut raw = Vec::with_capacity(36);
    raw.extend_from_slice(&[0x01, 0x55, 0x12, 0x20]);
    raw.extend_from_slice(&digest);
    let mut out = String::with_capacity(60);
    out.push('b');
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for &b in &raw {
        acc = ((acc << 8) | u32::from(b)) & 0xFFFF;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((acc >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((acc << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// One variant of an original file.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct VariantEntry {
    pub cid: String,
    /// `"texture"` or `"model"`.
    pub kind: String,
    /// `"ktx2-bc7"`, `"ktx2-bc1"`, `"ktx2-bc5"` or `"glb"`.
    pub format: String,
    /// Texture variants only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mips: Option<u32>,
    pub size: u64,
}

/// The per-entity manifest (`v: 1`). `files` is keyed by original content CID and ordered, so
/// the serialized bytes are a function of the variants alone.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub v: u32,
    pub entity: String,
    pub generator: String,
    pub files: BTreeMap<String, Vec<VariantEntry>>,
}

#[cfg(all(test, feature = "bevy-variants"))]
mod tests;

#[cfg(feature = "bevy-variants")]
impl crate::live::Proxy {
    /// JIT entry point: resolve the entity through this proxy's content sources (the same
    /// ones the bvwebgpu lane uses), build its variants into `out_root` and, when object
    /// storage is configured, write variants and then the manifest back to it.
    pub fn build_bevy_variants(
        self: &std::sync::Arc<Self>,
        out_root: &Path,
        cid: &str,
    ) -> anyhow::Result<()> {
        let started = std::time::Instant::now();
        let ctx = self.entity_ctx(cid)?;
        let fetch = |hash: &str| -> Option<Vec<u8>> {
            match self.content_bytes_allow_empty(hash) {
                Ok(b) => Some(b),
                Err(e) => {
                    tracing::warn!(hash = %hash, error = %format!("{e:#}"), "bevy variants: content fetch failed");
                    None
                }
            }
        };
        let out = build_entity_variants(&ctx.scene, &fetch, out_root, BuildOptions::from_env())?;
        if self.space_configured() {
            for (vcid, path) in &out.variants {
                match std::fs::read(path) {
                    Ok(bytes) => self.space_put_key(&content_key(vcid), &bytes),
                    Err(e) => {
                        tracing::warn!(cid = %vcid, error = %e, "bevy variants: re-read for upload failed")
                    }
                }
            }
            self.space_put_key(&manifest_key(cid), &out.manifest_bytes);
        }
        tracing::info!(
            entity = %cid,
            originals = out.manifest.files.len(),
            variants = out.variants.len(),
            ms = started.elapsed().as_millis() as u64,
            "bevy variants built"
        );
        Ok(())
    }
}

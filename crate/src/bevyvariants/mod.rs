//! **Prototype.** The Bevy variants lane: per scene entity, a manifest that maps each original
//! content CID to GPU-ready variants for the Bevy explorer.
//!
//! - PNG/JPEG textures become KTX2 (`crate::ktx2_write`): BC1 / BC5 / BC7 UNORM blocks,
//!   Zstandard-supercompressed, level 0 only (no mip chain, like the single-level creator UASTC
//!   files), longest side at most [`BEVY_TEXTURE_MAX`] (powers of two). BC5 for images the
//!   entity's models use only as normal maps, BC1 for fully opaque images whose BC1 level 0
//!   stays within `texture::BC1_MIN_PSNR_DB` of the source (linear ORM images keep BC1
//!   regardless), BC7 otherwise.
//! - Every standalone texture variant must be smaller than its original file, or the original
//!   is served as is (`size-gate` in the stats). Images embedded in a GLB variant are not gated
//!   (see below).
//! - Creator UASTC KTX2 textures (standalone or embedded in a GLB) are only ever transcoded block
//!   for block to BC7, and only the single level the explorer's local path picks (the first
//!   level at most 1024 on a side, no mips), so remote equals localhost; anything that would
//!   need a resize or re-encode gets no variant. Keeping the file's lower mips needs the
//!   explorer's local path to keep them too.
//! - GLB variants ([`model`], the `g` class, on by default; `ABGEN_BEVY_GLB_VARIANTS=0` or
//!   `abgen-corpus --bevy-no-glb-variants` turns them off): a GLB with Draco decoded,
//!   `KHR_texture_basisu` resolved and every embedded image replaced by a BC KTX2
//!   (`image/ktx2`), so the explorer holds it at 0.5-1 byte per pixel instead of 4. Embedded
//!   images skip the size gate: the goal is GPU memory, and a JPEG's BC KTX2 is often larger on
//!   the wire than the JPEG. A GLB whose embedded KTX2 cannot be transcoded gets no variant.
//!   External image URIs are left as they are: they resolve to their own original CID, which
//!   has its own texture variant. With `.br` sidecars on, a GLB variant also gets one when it is
//!   smaller (the geometry inside still compresses).
//! - Every `.glb` also gets a `glb-br` model variant (`model`, on by default,
//!   `ABGEN_BEVY_MODELS=0` or `ABGEN_BEVY_PRECOMPRESS=none` turns it off; [`glbbr`]): the
//!   original `.glb`, byte for byte, plus its Brotli sidecar `{key}.br`, kept only when the
//!   sidecar is smaller than the original, and served with `Content-Encoding: br` to clients
//!   that accept it. The CID is always of the plain bytes. A `.gltf` gets none (`not-glb`).
//!   Standalone KTX2 variants are never Brotli'd.
//! - Nothing else gets a variant (scene code, audio, video, source maps, unknown files).
//!
//! The client contract: no manifest, no entry, or any failure means the client loads the
//! original file. A variant is fetched at a path its manifest lists; the path's prefix letter
//! is its class. Its full entry (CID, format, size) stays in its record, which is not served.
//!
//! **Asset lane.** Variants are stored under a name computed from their *inputs*, so an entity
//! build can ask the store whether a variant exists before fetching or encoding anything, the
//! way the Unity lanes probe `{version}/assets/{hash}_{digest}_{platform}`:
//!
//! - textures: `bevy/t{TEX_GEN}/{sourceCid}_{slot}` (`slot` = `srgb` | `linear` | `normal`,
//!   how the entity's models sample the image; the same image used differently in two scenes
//!   is two variants). A non-default Zstandard level is part of the prefix (`t3z9`).
//! - `glb-br` models: `bevy/m{MESH_GEN}/{sourceCid}`.
//! - GLB variants: `bevy/g{GLB_GEN}/{sourceCid}`, with the digest of the external
//!   `.bin` buffers appended for a `.gltf` source (they are inlined, so they are an input;
//!   image URIs are not).
//!
//! Beside every name sits its **record**, `{key}.json` ([`Record`]): the manifest entry the
//! variant gets (its CID, format, size, dimensions, whether a `.br` sidecar exists), or the
//! reason there is no variant (so a size-gated or tiny image is not retried by every entity
//! that references it), plus, for models, how the model samples each image URI (so a warm
//! entity needs no model bytes to pick its textures' slots). The record is what a build
//! reads before doing any work: its output directory, then (the store-backed build) Redis and
//! the store; a miss fetches and encodes. Records and
//! variants are immutable within a generation: [`TEX_GEN`], [`MESH_GEN`] and [`GLB_GEN`] are bumped by hand
//! when an encoder's output bytes change (precision, level, size cap, codec), which moves
//! every key of that class to a new prefix; the previous prefix is garbage and can be deleted
//! whole. The manifest records the prefixes it was built from (`lanes`), and an entity whose
//! manifest names an older prefix is rebuilt, re-encoding only the class that moved. An
//! operator can force a full rebuild without a release by setting `ABGEN_BEVY_EPOCH`
//! ([`EPOCH_ENV`]), which appends `e{N}` to every prefix.
//!
//! **Orientation.** Rows are stored top row first (`KTXorientation` = `rd`), which is what
//! Bevy's loaders and glTF's top-left UV origin expect. Decoded PNG/JPEG buffers and UASTC KTX2
//! (`"rd"`, required by `KHR_texture_basisu`) are already top-down, so every encoder here runs
//! with `flip = false`. This differs from the Unity bundle lanes, which flip vertically.
//!
//! On-disk (and object-storage) layout mirrors the served routes: `bevy/manifest/{entity}.json`
//! and `bevy/{prefix}/{name}` (plus `{name}.br` and `{name}.json` beside it, never routed).

#[cfg(feature = "bevy-variants")]
mod build;
#[cfg(feature = "bevy-variants")]
pub mod glbbr;
#[cfg(feature = "bevy-variants")]
pub mod model;
#[cfg(feature = "bevy-variants")]
pub mod stats;
#[cfg(feature = "bevy-variants")]
pub mod texture;

#[cfg(feature = "bevy-variants")]
pub use build::{build_entity_variants, BuildOptions, BuildOutput, Lane, Upload};

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The lane's name: the first segment of its routes and keys.
pub const BEVY_PLATFORM: &str = "bevy";
/// Longest side of a texture variant.
pub const BEVY_TEXTURE_MAX: u32 = 1024;
/// Manifest schema version. 2: entries carry `path`, the manifest carries `lanes`. 3: the
/// manifest is only `v`, `lanes` and original CID -> variant paths; the full entries stay in
/// the records.
pub const MANIFEST_VERSION: u32 = 3;
/// Record schema version.
pub const RECORD_VERSION: u32 = 1;
/// Served (and stored) prefix of manifests.
pub const MANIFEST_PREFIX: &str = "bevy/manifest";

/// Generation of the texture encoders (BC1/BC5/BC7 kernels, mips, size cap, KTX2 writer, the
/// UASTC transcode). Bump when any of them changes its output bytes.
/// 2: sources whose variant size would be under 4 px on a side get no variant (`tiny`).
/// 3: texture variants are single-level, no mip chain.
pub const TEX_GEN: u32 = 3;
/// Generation of the model variants. Bump when their output bytes change.
/// 2: a model variant is the original `.glb` unchanged, with its `.br` sidecar (format
/// `glb-br`; geometry is not re-encoded, so the client needs only its glTF loader).
pub const MESH_GEN: u32 = 2;
/// Generation of the GLB variants: their own rewrite rules and the texture encoders inside, so
/// bump it whenever [`TEX_GEN`] is bumped too. It starts at 4, above the `g3` the class used
/// when it was named after [`TEX_GEN`].
/// 4: embedded images are not size-gated; a GLB with an embedded KTX2 that does not transcode
/// gets no variant; `.br` sidecars.
pub const GLB_GEN: u32 = 4;

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

/// A variant class: which encoder's generation names its prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// `t{TEX_GEN}` (`t{TEX_GEN}z{level}` off the default Zstandard level).
    Texture,
    /// `m{MESH_GEN}`.
    Mesh,
    /// `g{GLB_GEN}` (same suffix rule as textures: the KTX2 inside follow it).
    Glb,
}

impl Class {
    /// The manifest `lanes` key of this class.
    pub fn lane(self) -> &'static str {
        match self {
            Class::Texture => "texture",
            Class::Mesh => "mesh",
            Class::Glb => "glb",
        }
    }

    /// The prefix a variant of this class is stored under: the encoder generation, plus the
    /// Zstandard level when it is not the default (it is part of the bytes), plus the
    /// operator's epoch when one is set ([`EPOCH_ENV`]).
    pub fn prefix(self, zstd_level: i32, epoch: Option<u32>) -> String {
        let z = if zstd_level == crate::ktx2_write::DEFAULT_ZSTD_LEVEL {
            String::new()
        } else {
            format!("z{zstd_level}")
        };
        let e = epoch.map(|e| format!("e{e}")).unwrap_or_default();
        match self {
            Class::Texture => format!("t{TEX_GEN}{z}{e}"),
            Class::Mesh => format!("m{MESH_GEN}{e}"),
            Class::Glb => format!("g{GLB_GEN}{z}{e}"),
        }
    }
}

/// Operator switch for a full rebuild without a release: when set to a number, every prefix
/// gets `e{N}` appended (`t1e2`, `m1e2`), so every key misses, every manifest reads as stale
/// and the lane re-encodes into the new prefixes. Unset (the default) adds nothing. Changing
/// it back finds the old prefixes again. The generations stay in code: they describe the
/// encoders, the epoch describes a deployment's decision to start over.
pub const EPOCH_ENV: &str = "ABGEN_BEVY_EPOCH";
/// Largest accepted epoch.
pub const EPOCH_MAX: u32 = 9999;

/// [`EPOCH_ENV`], parsed: unset or empty is `None`; anything that is not a number in
/// `0..=EPOCH_MAX` is logged and ignored.
pub fn epoch_from_env() -> Option<u32> {
    let v = std::env::var(EPOCH_ENV).ok()?;
    parse_epoch(&v).unwrap_or_else(|| {
        tracing::warn!(value = %v, "{EPOCH_ENV} ignored: expected a number 0..={EPOCH_MAX}");
        None
    })
}

/// `Some(None)` for an empty value, `Some(Some(n))` for a valid number, `None` if invalid.
fn parse_epoch(v: &str) -> Option<Option<u32>> {
    let v = v.trim();
    if v.is_empty() {
        return Some(None);
    }
    if !v.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    v.parse::<u32>().ok().filter(|n| *n <= EPOCH_MAX).map(Some)
}

/// A lane prefix as the routes take it: `t`, `m` or `g`, a generation, then optionally `z`
/// and a level, then optionally `e` and an epoch. Never `manifest`.
pub fn is_prefix(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 2 || b.len() > 16 || !matches!(b[0], b't' | b'm' | b'g') {
        return false;
    }
    let digits = |d: &str| !d.is_empty() && d.bytes().all(|c| c.is_ascii_digit());
    let rest = &s[1..];
    let (rest, epoch) = match rest.split_once('e') {
        Some((r, e)) => (r, Some(e)),
        None => (rest, None),
    };
    let (gen, level) = match rest.split_once('z') {
        Some((g, l)) => (g, Some(l)),
        None => (rest, None),
    };
    digits(gen) && level.is_none_or(digits) && epoch.is_none_or(digits)
}

/// A variant name as the routes take it: ASCII alphanumerics and `_` (a source CID, a slot, a
/// digest). No dot, so a `.br` sidecar or a `.json` record is never routable.
pub fn is_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 200 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

pub fn manifest_key(entity: &str) -> String {
    format!("{MANIFEST_PREFIX}/{entity}.json")
}

/// The object key (and served path) of a variant: `bevy/{prefix}/{name}`.
pub fn variant_key(prefix: &str, name: &str) -> String {
    format!("{BEVY_PLATFORM}/{prefix}/{name}")
}

/// The record beside a variant key.
pub fn record_key(variant_key: &str) -> String {
    format!("{variant_key}.json")
}

pub fn manifest_path(root: &Path, entity: &str) -> Option<PathBuf> {
    is_id(entity).then(|| {
        root.join("bevy")
            .join("manifest")
            .join(format!("{entity}.json"))
    })
}

pub fn variant_path(root: &Path, prefix: &str, name: &str) -> Option<PathBuf> {
    (is_prefix(prefix) && is_name(name)).then(|| root.join("bevy").join(prefix).join(name))
}

/// The local file of a variant key (`bevy/{prefix}/{name}`) or of its `.br` sidecar or
/// `.json` record, or `None` for anything else.
pub fn key_path(root: &Path, key: &str) -> Option<PathBuf> {
    let mut segs = key.split('/');
    let (Some(BEVY_PLATFORM), Some(prefix), Some(file), None) =
        (segs.next(), segs.next(), segs.next(), segs.next())
    else {
        return None;
    };
    let (name, suffix) = [".json", ".br"]
        .into_iter()
        .find_map(|s| file.strip_suffix(s).map(|n| (n, s)))
        .unwrap_or((file, ""));
    let path = variant_path(root, prefix, name)?;
    if suffix.is_empty() {
        return Some(path);
    }
    let mut s = path.into_os_string();
    s.push(suffix);
    Some(PathBuf::from(s))
}

/// CIDv1, raw codec (0x55), sha2-256 multihash, multibase base32 lower (`b…`): the `bafkrei…`
/// form catalysts use for content files. Recorded in each variant's record (`VariantEntry::cid`)
/// for checks and tooling; the v3 manifest does not carry it.
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
    /// Where to fetch it, relative to the lane's base: `bevy/{prefix}/{name}`.
    pub path: String,
    /// CIDv1 raw sha256 of the bytes at `path`, for the client's check.
    pub cid: String,
    /// `"texture"` or `"model"`.
    pub kind: String,
    /// `"ktx2-bc7"`, `"ktx2-bc1"`, `"ktx2-bc5"`, `"glb-br"` or `"glb"`.
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

/// How one model samples one image (by the image's URI in the model).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImageUse {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub srgb: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub normal: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub linear: bool,
}

/// The record beside a variant key (`{key}.json`): the outcome of building that key, final
/// within its generation. Either `variant` (the manifest entry) or `reason` (why there is
/// none; a stats reason name such as `size-gate`). `uses` is set on model records: image URI
/// -> how the model samples it, so textures' slots are known without the model's bytes.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct Record {
    pub v: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<VariantEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// A `.br` sidecar of the variant exists beside it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub br: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub uses: BTreeMap<String, ImageUse>,
}

impl Record {
    pub fn parse(bytes: &[u8]) -> Option<Record> {
        let r: Record = serde_json::from_slice(bytes).ok()?;
        (r.v == RECORD_VERSION).then_some(r)
    }
}

/// The per-entity manifest (`v: 3`): what the client needs and nothing else. `files` maps
/// each original content CID to the paths of its variants (one per class, so at most one
/// texture, one `glb-br` and one `glb`), ordered, so the serialized bytes are a function
/// of the variants alone. The class of a path is its prefix letter (`t` BC KTX2, `m`
/// `glb-br`, `g` GLB); the KTX2 header carries the BC format and size. `lanes` names
/// the prefix each class was built from (`{"texture":"t3","mesh":"m2","glb":"g4"}`), read back by
/// [`Manifest::is_current`]. The full entry (CID, format, dimensions, size) of each variant
/// is in its record.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub v: u32,
    #[serde(default)]
    pub lanes: BTreeMap<String, String>,
    pub files: BTreeMap<String, Vec<String>>,
}

impl Manifest {
    /// Whether this manifest was built from every prefix in `lanes` (the classes a build
    /// would use now, by their current prefixes). A class the manifest does not name, or
    /// names under another prefix, means a rebuild; a class it names that `lanes` does not
    /// (an option since turned off) does not.
    pub fn is_current(&self, lanes: &BTreeMap<String, String>) -> bool {
        self.v == MANIFEST_VERSION
            && lanes
                .iter()
                .all(|(class, prefix)| self.lanes.get(class) == Some(prefix))
    }
}

#[cfg(all(test, feature = "bevy-variants"))]
mod tests;

/// The disk-cache key of a Bevy-lane file under `root`: its path below the root with `/`
/// separators (`bevy/t3/{name}`, `bevy/t3/{name}.json`, `bevy/manifest/{entity}.json`). `None`
/// for a path that is not under `{root}/bevy/`.
pub fn cache_key(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let mut comps = rel.components().map(|c| c.as_os_str().to_str());
    if comps.next()?? != BEVY_PLATFORM {
        return None;
    }
    let mut key = BEVY_PLATFORM.to_string();
    for c in comps {
        key.push('/');
        key.push_str(c?);
    }
    Some(key)
}

/// What [`crate::live::Proxy::build_bevy_variants`] did for one entity.
#[cfg(feature = "bevy-variants")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BevyBuild {
    /// The store already had a current manifest for the entity: nothing was built.
    pub skipped: bool,
    /// Originals with at least one variant.
    pub originals: usize,
    /// Variants reused from their records (no fetch, no encode).
    pub reused: u64,
    /// Variants this build encoded.
    pub built: u64,
    /// Files whose content could not be fetched or whose variant could not be built
    /// ([`BuildOutput::incomplete`]). When not zero the manifest was not written (what did
    /// build was uploaded): the client uses the originals and the next conversion retries.
    pub incomplete: usize,
}

/// Whether `bytes` is a manifest built from the prefixes `lanes` names ([`Manifest::is_current`]).
#[cfg(feature = "bevy-variants")]
fn manifest_bytes_current(bytes: &[u8], lanes: &BTreeMap<String, String>) -> bool {
    serde_json::from_slice::<Manifest>(bytes).is_ok_and(|m| m.is_current(lanes))
}

#[cfg(feature = "bevy-variants")]
impl crate::live::Proxy {
    /// The lane's probe against the store: Redis first (the record is the marker's value),
    /// then the record object. Any error is a miss, so the worst case is an encode that was
    /// not needed. The record read from the store is marked for the next probe.
    pub fn bevy_probe(&self, key: &str) -> Option<Record> {
        let rk = record_key(key);
        let cache_key = self.reuse_cache_key(&rk);
        if let Some(ck) = &cache_key {
            if let Some(v) = crate::rediscache::get(ck) {
                if let Some(r) = Record::parse(v.as_bytes()) {
                    metrics::counter!("abgen_bevy_probe_total", "outcome" => "redis").increment(1);
                    return Some(r);
                }
            }
        }
        if !self.space_configured() {
            return None;
        }
        let Some(bytes) = self.space_get_key(&rk) else {
            metrics::counter!("abgen_bevy_probe_total", "outcome" => "miss").increment(1);
            return None;
        };
        let Some(r) = Record::parse(&bytes) else {
            tracing::warn!(key = %rk, "bevy variants: unreadable record, rebuilding");
            metrics::counter!("abgen_bevy_probe_total", "outcome" => "miss").increment(1);
            return None;
        };
        if let (Some(ck), Ok(v)) = (&cache_key, std::str::from_utf8(&bytes)) {
            crate::rediscache::mark_with(ck, v);
        }
        metrics::counter!("abgen_bevy_probe_total", "outcome" => "space").increment(1);
        Some(r)
    }

    /// Build one entity's Bevy variants against the store, the way the bundle lanes build an
    /// entity. Called by the conversion job, once per entity; no request lane calls it.
    ///
    /// 1. Unless `force`, a current manifest already in the store means the entity is done:
    ///    nothing is resolved, fetched or built.
    /// 2. The entity is resolved through this proxy's content sources. Each variant key is
    ///    probed ([`Self::bevy_probe`]: Redis, then its record in the store); a hit fills the
    ///    manifest row, a miss fetches and encodes into `out_root`.
    /// 3. What was built is written back: each variant's bytes, then its sidecar, then its
    ///    record (marked in Redis once written), and the manifest last, so nothing in the
    ///    store ever names an object that is not there. A key whose objects did not all land
    ///    is dropped whole, and the manifest is not written: the next build finds what did.
    ///    Nor is it written when a file could not be fetched or built
    ///    ([`BevyBuild::incomplete`]): the client uses the originals, the next build retries.
    ///
    /// With object storage the store is the only record of what exists: records left under
    /// `out_root` by an earlier run are not trusted (that run may have died before its
    /// uploads), so every key is probed in the store. Without object storage it only builds
    /// into `out_root`, reusing the records there.
    pub fn build_bevy_variants(
        self: &std::sync::Arc<Self>,
        out_root: &Path,
        cid: &str,
        force: bool,
    ) -> anyhow::Result<BevyBuild> {
        let started = std::time::Instant::now();
        let opts = BuildOptions::from_env();
        if !force && self.space_configured() {
            let current = self
                .space_get_key(&manifest_key(cid))
                .is_some_and(|b| manifest_bytes_current(&b, &opts.lanes()));
            if current {
                tracing::info!(entity = %cid, "bevy variants: current manifest in the store, skipped");
                return Ok(BevyBuild {
                    skipped: true,
                    ..Default::default()
                });
            }
        }
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
        let probe = |key: &str| self.bevy_probe(key);
        let stored = |key: &str| -> Option<Vec<u8>> {
            if self.space_configured() {
                self.space_get_key(key)
            } else {
                None
            }
        };
        let lane = Lane {
            fetch: &fetch,
            probe: &probe,
            stored: &stored,
            local_records: !self.space_configured(),
        };
        let out = build_entity_variants(&ctx.scene, &lane, out_root, opts)?;
        if self.space_configured() {
            // A key whose bytes, sidecar or record did not all land is dropped whole: its
            // later objects are skipped (so the store never holds a record naming bytes it
            // lacks), nothing is marked in Redis, and its local record is deleted so the next
            // build here re-encodes and re-uploads it instead of reusing it.
            let mut failed = 0usize;
            let mut failed_owner: Option<&str> = None;
            for obj in &out.uploads {
                if failed_owner == Some(obj.owner.as_str()) {
                    continue;
                }
                let put = std::fs::read(&obj.path)
                    .map_err(|e| anyhow::anyhow!("re-read {}: {e}", obj.path.display()))
                    .and_then(|bytes| {
                        self.space_put_key_strict(&obj.key, &bytes)?;
                        Ok(bytes)
                    });
                let bytes = match put {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(key = %obj.key, error = %format!("{e:#}"), "bevy variants: upload failed");
                        failed += 1;
                        failed_owner = Some(obj.owner.as_str());
                        if let Some(rec) = key_path(out_root, &record_key(&obj.owner)) {
                            let _ = std::fs::remove_file(rec);
                        }
                        continue;
                    }
                };
                if obj.record {
                    if let (Some(ck), Ok(v)) =
                        (self.reuse_cache_key(&obj.key), std::str::from_utf8(&bytes))
                    {
                        crate::rediscache::mark_with(&ck, v);
                    }
                }
            }
            if failed > 0 {
                anyhow::bail!("{failed} uploads failed; manifest not written");
            }
            // What did build is uploaded above (content-addressed, correct for its keys); the
            // manifest waits for a build in which every file has a final outcome.
            if out.incomplete.is_empty() {
                self.space_put_key_strict(&manifest_key(cid), &out.manifest_bytes)?;
            }
        }
        let build = BevyBuild {
            skipped: false,
            originals: out.entries.len(),
            reused: out.stats.reused_textures
                + out.stats.reused_models
                + out.stats.glb_variants.reused,
            built: out.stats.unique_variants,
            incomplete: out.incomplete.len(),
        };
        if build.incomplete > 0 {
            tracing::warn!(
                entity = %cid,
                incomplete = build.incomplete,
                files = ?out.incomplete,
                "bevy variants: files could not be fetched or built; manifest not written"
            );
        }
        tracing::info!(
            entity = %cid,
            originals = build.originals,
            reused = build.reused,
            built = build.built,
            incomplete = build.incomplete,
            ms = started.elapsed().as_millis() as u64,
            "bevy variants built"
        );
        Ok(build)
    }
}

//! The per-entity builder: name every input's key, probe for it, fetch and encode the misses,
//! store bytes and records, write the manifest last.

use anyhow::{Context, Result};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use super::glbbr::{self, NoModelVariant};
use super::model::{image_uses, transform_model_with, NoGlbVariant};
use super::stats::{
    elapsed_ns, timed, BuildStats, FileStat, GlbStat, ModelStat, NoVariant, StageTimes,
};
use super::texture::{encode_texture_with, source_kind, Miss, Slot, TexOptions, Uses};
use super::{
    cid_v1_raw_sha256, key_path, manifest_path, record_key, variant_key, Class, ImageUse, Manifest,
    Record, VariantEntry, MANIFEST_VERSION, RECORD_VERSION,
};
use crate::catalyst::Scene;

/// What a build produces besides texture variants, and how.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuildOptions {
    /// GLB variants (the `g` class, [`super::model`]): every embedded image as a BC KTX2. On by default; `ABGEN_BEVY_GLB_VARIANTS=0` turns them off. Images
    /// embedded in GLBs never get a texture variant of their own either way. With
    /// [`BuildOptions::precompress_br`] a GLB variant also gets a `.br` sidecar when smaller.
    pub glb_variants: bool,
    /// Model variants (`glb-br`, the `m` class, [`super::glbbr`]): each `.glb` stored
    /// unchanged with a Brotli `.br` sidecar, kept when the sidecar is smaller than the
    /// original. On by default, independent of `glb_variants`.
    pub models: bool,
    /// Store `.br` sidecars of model and GLB variants. On by default. A model variant is only
    /// worth anything through its sidecar, so with this off no model variants are built at all
    /// ([`BuildOptions::builds_models`]); GLB variants are still built, without sidecars. A
    /// reused model variant whose record has no sidecar gets one added (a GLB variant does not:
    /// its record says `br` only when the sidecar was smaller).
    pub precompress_br: bool,
    /// Zstandard level of every KTX2 level ([`crate::ktx2_write::DEFAULT_ZSTD_LEVEL`] by
    /// default). Part of the variant bytes, so part of the texture prefix.
    pub zstd_level: i32,
    /// Operator epoch (`ABGEN_BEVY_EPOCH`, unset by default): appended to every prefix as
    /// `e{N}`, so setting or changing it starts every class over without a release.
    pub epoch: Option<u32>,
}

impl Default for BuildOptions {
    fn default() -> Self {
        BuildOptions {
            glb_variants: true,
            models: true,
            precompress_br: true,
            zstd_level: crate::ktx2_write::DEFAULT_ZSTD_LEVEL,
            epoch: None,
        }
    }
}

/// The env var that sets [`BuildOptions::zstd_level`].
pub const ZSTD_LEVEL_ENV: &str = "ABGEN_BEVY_ZSTD_LEVEL";
/// `0` turns [`BuildOptions::glb_variants`] off.
pub const GLB_VARIANTS_ENV: &str = "ABGEN_BEVY_GLB_VARIANTS";
/// `0` turns [`BuildOptions::models`] off.
pub const MODELS_ENV: &str = "ABGEN_BEVY_MODELS";
/// `none` turns [`BuildOptions::precompress_br`] off; `br` (the default) keeps it on.
pub const PRECOMPRESS_ENV: &str = "ABGEN_BEVY_PRECOMPRESS";

/// [`PRECOMPRESS_ENV`]'s value: `br` (or `1`/`true`/`on`) is on, `none` (or `0`/`false`/
/// `off`) is off, unset or empty is the default (on); anything else is logged and ignored.
fn precompress_from_env() -> bool {
    let Ok(v) = std::env::var(PRECOMPRESS_ENV) else {
        return true;
    };
    match v.trim().to_ascii_lowercase().as_str() {
        "" | "br" | "1" | "true" | "on" => true,
        "none" | "0" | "false" | "off" => false,
        _ => {
            tracing::warn!(value = %v, "{PRECOMPRESS_ENV} ignored: expected br or none");
            true
        }
    }
}

impl BuildOptions {
    /// `ABGEN_BEVY_GLB_VARIANTS=0` turns GLB variants off (default on);
    /// `ABGEN_BEVY_MODELS=0` turns `glb-br` model variants off (default on);
    /// `ABGEN_BEVY_PRECOMPRESS=none` stops storing `.br` sidecars, so model variants too
    /// (default on);
    /// `ABGEN_BEVY_ZSTD_LEVEL` (1..=19) sets the Zstandard level (default 19; an invalid value
    /// is logged and the default is used); `ABGEN_BEVY_EPOCH` sets the epoch (default unset).
    pub fn from_env() -> Self {
        let zstd_level = match std::env::var(ZSTD_LEVEL_ENV) {
            Ok(v) => match v
                .trim()
                .parse::<i32>()
                .map_err(anyhow::Error::from)
                .and_then(crate::ktx2_write::check_zstd_level)
            {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(value = %v, error = %format!("{e:#}"), "{ZSTD_LEVEL_ENV} ignored");
                    crate::ktx2_write::DEFAULT_ZSTD_LEVEL
                }
            },
            Err(_) => crate::ktx2_write::DEFAULT_ZSTD_LEVEL,
        };
        BuildOptions {
            glb_variants: crate::clihelp::env_bool(GLB_VARIANTS_ENV, true),
            models: crate::clihelp::env_bool(MODELS_ENV, true),
            precompress_br: precompress_from_env(),
            zstd_level,
            epoch: super::epoch_from_env(),
        }
    }

    /// Whether model variants are built: they need both switches.
    pub fn builds_models(self) -> bool {
        self.models && self.precompress_br
    }

    fn tex(self) -> TexOptions {
        TexOptions {
            zstd_level: self.zstd_level,
        }
    }

    /// The classes these options build and the prefix each is stored under: what a manifest
    /// records as `lanes`, and what [`Manifest::is_current`] is asked against.
    pub fn lanes(self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        out.insert(
            Class::Texture.lane().to_string(),
            Class::Texture.prefix(self.zstd_level, self.epoch),
        );
        if self.builds_models() {
            out.insert(
                Class::Mesh.lane().to_string(),
                Class::Mesh.prefix(self.zstd_level, self.epoch),
            );
        }
        if self.glb_variants {
            out.insert(
                Class::Glb.lane().to_string(),
                Class::Glb.prefix(self.zstd_level, self.epoch),
            );
        }
        out
    }
}

/// Where a build reads from. `fetch` returns a content file's bytes by hash (`None` =
/// unavailable: that file gets no variant and no record). `probe` returns the record of a
/// variant key that already exists somewhere this build does not write (a shared store);
/// `None` is a miss. Local records under `out_root` are read before `probe` is asked.
/// `stored` returns a stored variant's bytes by key (`None` = not there); a variant under
/// `out_root` is read before it is asked. It is only used to add a `.br` sidecar to a reused
/// variant that has none.
///
/// `local_records` says whether records already under `out_root` count. A build whose output
/// directory is the store of record (the offline corpus) sets it. A build that writes to a
/// shared store must not: a local record proves nothing about what the store holds (an
/// earlier run may have been killed before uploading), so only `probe` is asked.
pub struct Lane<'a> {
    pub fetch: &'a (dyn Fn(&str) -> Option<Vec<u8>> + Sync),
    pub probe: &'a (dyn Fn(&str) -> Option<Record> + Sync),
    pub stored: &'a (dyn Fn(&str) -> Option<Vec<u8>> + Sync),
    pub local_records: bool,
}

/// One object this build wrote under `out_root`, in the order they are written (and a store
/// should receive them): a variant's bytes, then its `.br` sidecar, then its record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upload {
    /// Object key, also the path under `out_root`.
    pub key: String,
    /// The variant key this object belongs to (its own key for the variant bytes). The
    /// uploads of one owner are contiguous and its record comes last.
    pub owner: String,
    pub path: PathBuf,
    /// This is a record (`{key}.json`).
    pub record: bool,
}

pub struct BuildOutput {
    pub manifest: Manifest,
    /// Original CID -> the full entry of each variant (what the records hold; the manifest
    /// lists only their paths).
    pub entries: BTreeMap<String, Vec<VariantEntry>>,
    /// The manifest exactly as written to `bevy/manifest/{entity}.json` (when the build is
    /// complete; otherwise what it would have been, and nothing is written).
    pub manifest_bytes: Vec<u8>,
    /// The files (content paths) whose outcome is not final: a content fetch failed, an encode
    /// failed or panicked, or a model whose image uses were needed could not be read. Empty for
    /// a complete build. When not empty the manifest is not written (and an older one at its
    /// path is removed): no manifest tells the client to use the originals, and the entity's
    /// next build retries these files, reusing everything else from its records.
    pub incomplete: Vec<String>,
    /// Everything this build wrote besides the manifest, in upload order. Reused variants are
    /// not here: they are already where the probe found them.
    pub uploads: Vec<Upload>,
    /// Stage times and outcomes (not part of any hashed output).
    pub stats: BuildStats,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Model,
    Image,
}

fn kind_of(file: &str) -> Option<Kind> {
    let f = file.to_ascii_lowercase();
    if f.ends_with(".glb") || f.ends_with(".gltf") {
        Some(Kind::Model)
    } else if [".png", ".jpg", ".jpeg", ".ktx2"]
        .iter()
        .any(|e| f.ends_with(e))
    {
        Some(Kind::Image)
    } else {
        // Scene code, source maps, audio, video and everything unknown: no variant.
        None
    }
}

fn model_ext(file: &str) -> &'static str {
    if file.to_ascii_lowercase().ends_with(".gltf") {
        ".gltf"
    } else {
        ".glb"
    }
}

/// The record of `key` under `out_root`, if this or an earlier build here wrote one and the
/// variant it names is still beside it (the serving node's disk cache evicts files one at a
/// time, so a record can outlive its bytes; such a record is a miss and is deleted).
fn local_record(out_root: &Path, key: &str) -> Option<Record> {
    let rp = key_path(out_root, &record_key(key))?;
    let bytes = std::fs::read(&rp).ok()?;
    let rec = Record::parse(&bytes)?;
    if rec.variant.is_some() && !key_path(out_root, key).is_some_and(|p| p.is_file()) {
        let _ = std::fs::remove_file(&rp);
        return None;
    }
    Some(rec)
}

/// Local disk (when the lane counts local records), then the lane's probe.
fn probe(lane: &Lane, out_root: &Path, key: &str, times: &mut StageTimes) -> Option<Record> {
    timed(&mut times.probe_ns, || {
        lane.local_records
            .then(|| local_record(out_root, key))
            .flatten()
            .or_else(|| (lane.probe)(key))
    })
}

fn write_atomic(dst: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let tmp = crate::tmppath::tmp_sibling(dst);
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, dst).with_context(|| format!("rename to {}", dst.display()))?;
    Ok(())
}

/// Write a key's objects under `out_root` in store order: the variant bytes (if any), its
/// `.br` sidecar (if any), its record. A file already there is left alone (a key's bytes are
/// final within its generation). Returns the uploads in that order.
fn store(
    out_root: &Path,
    key: &str,
    bytes: Option<&[u8]>,
    br: Option<&[u8]>,
    record: &Record,
    times: &mut StageTimes,
) -> Result<Vec<Upload>> {
    let started = std::time::Instant::now();
    let mut out = Vec::with_capacity(3);
    let dst = key_path(out_root, key).ok_or_else(|| anyhow::anyhow!("bad variant key {key:?}"))?;
    if let Some(b) = bytes {
        if !dst.is_file() {
            write_atomic(&dst, b)?;
        }
        out.push(Upload {
            key: key.to_string(),
            owner: key.to_string(),
            path: dst.clone(),
            record: false,
        });
    }
    if let Some(b) = br {
        let br_dst = glbbr::br_sidecar(&dst);
        if !br_dst.is_file() {
            write_atomic(&br_dst, b)?;
        }
        out.push(Upload {
            key: format!("{key}.br"),
            owner: key.to_string(),
            path: br_dst,
            record: false,
        });
    }
    let rk = record_key(key);
    let rec_dst = key_path(out_root, &rk).expect("a variant key's record is a valid key");
    if !rec_dst.is_file() {
        write_atomic(
            &rec_dst,
            &serde_json::to_vec(record).context("serialize record")?,
        )?;
    }
    out.push(Upload {
        key: rk,
        owner: key.to_string(),
        path: rec_dst,
        record: true,
    });
    times.hash_write_ns += elapsed_ns(started);
    Ok(out)
}

/// Add the `.br` sidecar to a reused `glb-br` variant whose record says it has none: read
/// the plain bytes (local, then the lane's store), check them against the record's CID,
/// compress, write the sidecar, then replace the record with one that says `br`. Uploads in
/// that order (sidecar, record), so the store never has a record naming a missing sidecar.
/// `None` when the bytes are not available or do not match: the variant is served plain.
fn backfill_br(
    lane: &Lane,
    out_root: &Path,
    key: &str,
    rec: &Record,
    times: &mut StageTimes,
) -> Result<Option<(Record, u64, Vec<Upload>)>> {
    let Some(v) = rec.variant.as_ref() else {
        return Ok(None);
    };
    let dst = key_path(out_root, key).ok_or_else(|| anyhow::anyhow!("bad variant key {key:?}"))?;
    let plain = std::fs::read(&dst).ok().or_else(|| (lane.stored)(key));
    let Some(plain) = plain.filter(|b| cid_v1_raw_sha256(b) == v.cid) else {
        tracing::warn!(key = %key, "bevy glb-br: reused variant unavailable; no .br added");
        return Ok(None);
    };
    let br = timed(&mut times.brotli_ns, || glbbr::brotli_compress(&plain))?;
    let started = std::time::Instant::now();
    let br_dst = glbbr::br_sidecar(&dst);
    write_atomic(&br_dst, &br)?;
    let mut rec = rec.clone();
    rec.br = true;
    let rk = record_key(key);
    let rec_dst = key_path(out_root, &rk).expect("a variant key's record is a valid key");
    write_atomic(
        &rec_dst,
        &serde_json::to_vec(&rec).context("serialize record")?,
    )?;
    times.hash_write_ns += elapsed_ns(started);
    let uploads = vec![
        Upload {
            key: format!("{key}.br"),
            owner: key.to_string(),
            path: br_dst,
            record: false,
        },
        Upload {
            key: rk,
            owner: key.to_string(),
            path: rec_dst,
            record: true,
        },
    ];
    Ok(Some((rec, br.len() as u64, uploads)))
}

fn entry(key: &str, bytes: &[u8], kind: &str, format: &str) -> VariantEntry {
    VariantEntry {
        path: key.to_string(),
        cid: cid_v1_raw_sha256(bytes),
        kind: kind.into(),
        format: format.into(),
        width: None,
        height: None,
        mips: None,
        size: bytes.len() as u64,
    }
}

fn reason_name<T: serde::Serialize>(r: T) -> String {
    serde_json::to_value(r)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

fn reason_of<T: serde::de::DeserializeOwned>(name: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(name.to_string())).ok()
}

/// Whether a no-variant outcome is a property of the input (recorded, never retried) or of
/// the run (not recorded: a missing file may appear, a failed write may succeed).
fn texture_reason_is_final(r: NoVariant) -> bool {
    !matches!(r, NoVariant::Missing | NoVariant::EncodeError)
}

fn model_reason_is_final(r: NoModelVariant) -> bool {
    !matches!(r, NoModelVariant::Missing | NoModelVariant::Error)
}

/// The `&'static str` the stats carry for a format name read back from a record.
fn static_format(name: &str) -> Option<&'static str> {
    [
        "ktx2-bc1",
        "ktx2-bc5",
        "ktx2-bc7",
        glbbr::GLB_BR_FORMAT,
        "glb",
    ]
    .into_iter()
    .find(|f| *f == name)
}

/// The name of a model's GLB variant: its source CID, plus, for a `.gltf`, a digest of the
/// external buffers it inlines (`uri` -> content hash, sorted), so the same `.gltf` beside different
/// `.bin` files is two variants. Image URIs are not inputs: the variant leaves them as they are.
fn model_name(
    hash: &str,
    file: &str,
    bytes: Option<&[u8]>,
    by_file: &HashMap<String, String>,
) -> String {
    let Some(bytes) = bytes else {
        return hash.to_string();
    };
    if model_ext(file) != ".gltf" {
        return hash.to_string();
    }
    let Ok(doc) = crate::naming::parse_gltf_doc(bytes, ".gltf") else {
        return hash.to_string();
    };
    let mut deps: Vec<(String, String)> = doc
        .get("buffers")
        .and_then(|b| b.as_array())
        .into_iter()
        .flatten()
        .filter_map(|b| b.get("uri").and_then(|u| u.as_str()))
        .filter(|u| !u.starts_with("data:"))
        .filter_map(|u| {
            crate::naming::uri_content_hash(u, file, by_file).map(|h| (u.to_string(), h.clone()))
        })
        .collect();
    if deps.is_empty() {
        return hash.to_string();
    }
    deps.sort();
    deps.dedup();
    let mut payload = Vec::new();
    for (u, h) in &deps {
        payload.extend_from_slice(u.as_bytes());
        payload.push(0);
        payload.extend_from_slice(h.as_bytes());
        payload.push(b'\n');
    }
    format!("{hash}_{}", &crate::hashes::sha256_hex(&payload)[..16])
}

/// A model's uses of its images, by URI, from its bytes.
fn uses_by_uri(
    bytes: &[u8],
    ext: &str,
    resolve: crate::gltf::Resolve,
) -> BTreeMap<String, ImageUse> {
    let mut out: BTreeMap<String, ImageUse> = BTreeMap::new();
    let Some((per_image, uris)) = image_uses(bytes, ext, resolve) else {
        return out;
    };
    for (u, uri) in per_image.into_iter().zip(uris) {
        let Some(uri) = uri else { continue };
        let e = out.entry(uri).or_default();
        let mut merged: Uses = (*e).into();
        merged.merge(u);
        *e = merged.into();
    }
    out
}

/// One original's outcome: its hash and its manifest entries (one per class that gave one).
type Row = (String, Vec<VariantEntry>);

struct ModelOut {
    row: Row,
    stat: Option<ModelStat>,
    uploads: Vec<Upload>,
    /// Image URI -> use, from the mesh record or the bytes.
    uses: BTreeMap<String, ImageUse>,
    /// The GLB variant's outcome, when one was asked for.
    glb: Option<GlbStat>,
    /// Some outcome of this model is not final (see [`BuildOutput::incomplete`]).
    incomplete: bool,
}

/// Build every variant of `scene` and its manifest under `out_root`, in the served layout.
/// Per-file failures are logged and leave that file without a variant; only I/O errors on
/// `out_root` fail the build. Variants and records are written before the manifest, so a
/// manifest on disk never names a variant that is not. A file whose outcome is not final (a
/// failed fetch or encode) makes the build incomplete ([`BuildOutput::incomplete`]): what did
/// build is still written, but the manifest is not.
pub fn build_entity_variants(
    scene: &Scene,
    lane: &Lane,
    out_root: &Path,
    opts: BuildOptions,
) -> Result<BuildOutput> {
    crate::ktx2_write::check_zstd_level(opts.zstd_level)?;
    let started = std::time::Instant::now();
    let entity = scene.entity_id.as_str();
    let manifest_dst = manifest_path(out_root, entity)
        .with_context(|| format!("entity id {entity:?} is not a plain CID"))?;
    let content_by_file = scene.content_by_file();
    let skipped = scene.metadata_only_hashes();
    let fetch = lane.fetch;
    let tex_prefix = Class::Texture.prefix(opts.zstd_level, opts.epoch);
    let mesh_prefix = Class::Mesh.prefix(opts.zstd_level, opts.epoch);
    let glb_prefix = Class::Glb.prefix(opts.zstd_level, opts.epoch);
    let tex_opts = opts.tex();

    // One (file, kind) per hash: the lexicographically first path, so the relative URIs a
    // model resolves are the same on every run.
    let mut by_hash: BTreeMap<&str, (&str, Kind)> = BTreeMap::new();
    for c in &scene.content {
        let Some(kind) = kind_of(&c.file) else {
            continue;
        };
        if skipped.contains(&c.hash) {
            continue;
        }
        let e = by_hash
            .entry(c.hash.as_str())
            .or_insert((c.file.as_str(), kind));
        if c.file.as_str() < e.0 {
            *e = (c.file.as_str(), kind);
        }
    }
    let models: Vec<(&str, &str)> = by_hash
        .iter()
        .filter(|(_, (_, k))| *k == Kind::Model)
        .map(|(h, (f, _))| (*h, *f))
        .collect();
    let images: Vec<(&str, &str)> = by_hash
        .iter()
        .filter(|(_, (_, k))| *k == Kind::Image)
        .map(|(h, (f, _))| (*h, *f))
        .collect();

    // Pass 1: models. Each one's key is probed first; a hit gives the manifest entry and how
    // the model samples its images without fetching it. A miss fetches, classifies and encodes.
    let model_outs: Vec<ModelOut> = models
        .par_iter()
        .map(|&(hash, file)| -> Result<ModelOut> {
            let ext = model_ext(file);
            let resolve = |uri: &str| -> Option<Vec<u8>> {
                fetch(crate::naming::uri_content_hash(uri, file, &content_by_file)?.as_str())
            };
            let mut times = StageTimes::default();
            let mut bytes: Option<Vec<u8>> = None;
            let mut fetched = false;
            let mut out = ModelOut {
                row: (hash.to_string(), Vec::new()),
                stat: None,
                uploads: Vec::new(),
                uses: BTreeMap::new(),
                glb: None,
                incomplete: false,
            };
            let mut uses_known = false;

            if opts.builds_models() {
                // The original's bytes are the only input, so its CID is the name.
                let key = variant_key(&mesh_prefix, hash);
                let mut stat = ModelStat {
                    hash: hash.to_string(),
                    file: file.to_string(),
                    original_size: 0,
                    format: None,
                    variant_path: None,
                    variant_cid: None,
                    variant_size: None,
                    br_size: None,
                    gated_size: None,
                    reason: None,
                    reused: false,
                    times: StageTimes::default(),
                };
                if let Some(mut rec) = probe(lane, out_root, &key, &mut times) {
                    // `original_size` stays 0: the source was not fetched.
                    stat.reused = true;
                    // A variant whose record says it has no sidecar (lost, or a record from
                    // before sidecars) gets one here, without a fetch.
                    if !rec.br && rec.variant.is_some() {
                        if let Some((r, br_size, ups)) =
                            backfill_br(lane, out_root, &key, &rec, &mut times)?
                        {
                            rec = r;
                            stat.br_size = Some(br_size);
                            out.uploads.extend(ups);
                        }
                    }
                    out.uses = rec.uses.clone();
                    uses_known = true;
                    match rec.variant {
                        Some(v) => {
                            stat.format = static_format(&v.format);
                            stat.variant_path = Some(v.path.clone());
                            stat.variant_cid = Some(v.cid.clone());
                            stat.variant_size = Some(v.size);
                            out.row.1.push(v);
                        }
                        None => {
                            stat.reason = rec.reason.as_deref().and_then(reason_of);
                        }
                    }
                } else {
                    match ensure_bytes(&mut bytes, &mut fetched, fetch, hash, &mut times) {
                        None => {
                            stat.reason = Some(NoModelVariant::Missing);
                            out.incomplete = true;
                        }
                        Some(b) => {
                            stat.original_size = b.len() as u64;
                            out.uses = uses_by_uri(b, ext, Some(&resolve));
                            uses_known = true;
                            // The variant is the `.glb` as it is; what it saves is its
                            // Brotli sidecar, kept only when smaller than the original.
                            let outcome = if ext != ".glb" {
                                Err(NoModelVariant::NotGlb)
                            } else {
                                let br = timed(&mut times.brotli_ns, || {
                                    glbbr::brotli_compress(b)
                                })?;
                                if br.len() >= b.len() {
                                    stat.gated_size = Some(br.len() as u64);
                                    Err(NoModelVariant::SizeGate)
                                } else {
                                    Ok(br)
                                }
                            };
                            match outcome {
                                Ok(br) => {
                                    let e = entry(&key, b, "model", glbbr::GLB_BR_FORMAT);
                                    let rec = Record {
                                        v: RECORD_VERSION,
                                        variant: Some(e.clone()),
                                        reason: None,
                                        br: true,
                                        uses: out.uses.clone(),
                                    };
                                    out.uploads.extend(store(
                                        out_root,
                                        &key,
                                        Some(b),
                                        Some(&br),
                                        &rec,
                                        &mut times,
                                    )?);
                                    stat.format = Some(glbbr::GLB_BR_FORMAT);
                                    stat.variant_path = Some(key.clone());
                                    stat.variant_cid = Some(e.cid.clone());
                                    stat.variant_size = Some(e.size);
                                    stat.br_size = Some(br.len() as u64);
                                    out.row.1.push(e);
                                }
                                Err(reason) => {
                                    stat.reason = Some(reason);
                                    out.incomplete |= !model_reason_is_final(reason);
                                    if model_reason_is_final(reason) {
                                        let rec = Record {
                                            v: RECORD_VERSION,
                                            variant: None,
                                            reason: Some(reason_name(reason)),
                                            br: false,
                                            uses: out.uses.clone(),
                                        };
                                        out.uploads.extend(store(out_root, &key, None, None, &rec, &mut times,
                                        )?);
                                    }
                                }
                            }
                        }
                    }
                }
                out.stat = Some(stat);
            }

            if opts.glb_variants {
                // Its own stage times: `times` is the `glb-br` row's (and pays the fetch).
                let mut gt = StageTimes::default();
                // A `.gltf` names its buffers in its JSON: read it to name the variant.
                let name = if ext == ".gltf" {
                    let b = ensure_bytes(&mut bytes, &mut fetched, fetch, hash, &mut times);
                    // Unread, its name (and so its key) is not known.
                    out.incomplete |= b.is_none();
                    model_name(hash, file, b, &content_by_file)
                } else {
                    hash.to_string()
                };
                let key = variant_key(&glb_prefix, &name);
                let mut stat = GlbStat {
                    hash: hash.to_string(),
                    file: file.to_string(),
                    original_size: 0,
                    variant_path: None,
                    variant_size: None,
                    br_size: None,
                    br: false,
                    reason: None,
                    reused: false,
                    images: Vec::new(),
                    times: StageTimes::default(),
                };
                if let Some(rec) = probe(lane, out_root, &key, &mut gt) {
                    stat.reused = true;
                    if !uses_known {
                        out.uses = rec.uses.clone();
                        uses_known = true;
                    }
                    stat.reason = rec.reason.clone();
                    stat.br = rec.br;
                    if let Some(v) = &rec.variant {
                        stat.variant_path = Some(v.path.clone());
                        stat.variant_size = Some(v.size);
                    }
                    out.row.1.extend(rec.variant);
                } else if let Some(b) =
                    ensure_bytes(&mut bytes, &mut fetched, fetch, hash, &mut times)
                {
                    stat.original_size = b.len() as u64;
                    if !uses_known {
                        out.uses = uses_by_uri(b, ext, Some(&resolve));
                        uses_known = true;
                    }
                    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        transform_model_with(b, ext, Some(&resolve), tex_opts)
                    }));
                    // A variant, or a reason there is none, is a final answer; a failure is not.
                    match caught {
                        Ok(Ok(o)) => {
                            gt.add(&o.times);
                            stat.images = o.images;
                            match o.variant {
                                Some(v) => {
                                    // The geometry inside still compresses: a `.br` sidecar
                                    // when it is smaller. The variant is kept either way (its
                                    // point is GPU memory, not download).
                                    let br = if opts.precompress_br {
                                        let br = timed(&mut gt.brotli_ns, || {
                                            glbbr::brotli_compress_with(
                                                &v.glb,
                                                glbbr::GLB_VARIANT_BROTLI_QUALITY,
                                            )
                                        })?;
                                        (br.len() < v.glb.len()).then_some(br)
                                    } else {
                                        None
                                    };
                                    let e = entry(&key, &v.glb, "model", "glb");
                                    let rec = Record {
                                        v: RECORD_VERSION,
                                        variant: Some(e.clone()),
                                        reason: None,
                                        br: br.is_some(),
                                        uses: out.uses.clone(),
                                    };
                                    out.uploads.extend(store(
                                        out_root,
                                        &key,
                                        Some(&v.glb),
                                        br.as_deref(),
                                        &rec,
                                        &mut gt,
                                    )?);
                                    stat.variant_path = Some(key.clone());
                                    stat.variant_size = Some(e.size);
                                    stat.br = br.is_some();
                                    stat.br_size = br.map(|b| b.len() as u64);
                                    out.row.1.push(e);
                                }
                                None => {
                                    let reason = reason_name(
                                        o.reason.unwrap_or(NoGlbVariant::NothingToChange),
                                    );
                                    let rec = Record {
                                        v: RECORD_VERSION,
                                        variant: None,
                                        reason: Some(reason.clone()),
                                        br: false,
                                        uses: out.uses.clone(),
                                    };
                                    out.uploads
                                        .extend(store(out_root, &key, None, None, &rec, &mut gt)?);
                                    stat.reason = Some(reason);
                                }
                            }
                        }
                        Ok(Err(e)) => {
                            tracing::warn!(entity = %entity, file = %file, error = %format!("{e:#}"), "bevy glb variant skipped");
                            stat.reason = Some("error".to_string());
                            out.incomplete = true;
                        }
                        Err(_) => {
                            tracing::warn!(entity = %entity, file = %file, "bevy glb variant panicked; skipped");
                            stat.reason = Some("error".to_string());
                            out.incomplete = true;
                        }
                    }
                } else {
                    // Not fetched: no outcome for this key.
                    stat.reason = Some("missing".to_string());
                    out.incomplete = true;
                }
                stat.times = gt;
                out.glb = Some(stat);
            }

            if !uses_known {
                // Without the model its images' slots are unknown, and the defaults may be
                // wrong (a normal map encoded as sRGB): the build is not complete.
                match ensure_bytes(&mut bytes, &mut fetched, fetch, hash, &mut times) {
                    Some(b) => out.uses = uses_by_uri(b, ext, Some(&resolve)),
                    None => out.incomplete = true,
                }
            }
            if let Some(s) = out.stat.as_mut() {
                s.times = times;
            }
            Ok(out)
        })
        .collect::<Result<_>>()?;

    // How the entity's models sample each external image, by content hash.
    let mut uses: HashMap<String, Uses> = HashMap::new();
    for ((_, file), m) in models.iter().zip(&model_outs) {
        for (uri, u) in &m.uses {
            if let Some(h) = crate::naming::uri_content_hash(uri, file, &content_by_file) {
                uses.entry(h.clone()).or_default().merge((*u).into());
            }
        }
    }

    // Pass 2: standalone textures, keyed by source and slot.
    let (textures, file_stats, tex_uploads): (Vec<Row>, Vec<FileStat>, Vec<Vec<Upload>>) = images
        .par_iter()
        .map(|&(hash, file)| -> Result<(Row, FileStat, Vec<Upload>)> {
            let mut times = StageTimes::default();
            let mut stat = FileStat {
                hash: hash.to_string(),
                file: file.to_string(),
                source: super::stats::SourceKind::Other,
                original_size: 0,
                format: None,
                variant_path: None,
                variant_cid: None,
                variant_size: None,
                width: None,
                height: None,
                mips: None,
                reason: None,
                bc1_psnr_db: None,
                bc1_rejected: None,
                reused: false,
                times,
            };
            let slot: Slot = uses.get(hash).copied().unwrap_or_default().slot();
            let key = variant_key(&tex_prefix, &format!("{hash}_{}", slot.name()));
            let row = |e: Option<VariantEntry>| (hash.to_string(), e.into_iter().collect());
            if let Some(rec) = probe(lane, out_root, &key, &mut times) {
                stat.reused = true;
                stat.times = times;
                return Ok(match rec.variant {
                    Some(v) => {
                        stat.format = static_format(&v.format);
                        stat.variant_path = Some(v.path.clone());
                        stat.variant_cid = Some(v.cid.clone());
                        stat.variant_size = Some(v.size);
                        stat.width = v.width;
                        stat.height = v.height;
                        stat.mips = v.mips;
                        (row(Some(v)), stat, Vec::new())
                    }
                    None => {
                        stat.reason = rec.reason.as_deref().and_then(reason_of);
                        (row(None), stat, Vec::new())
                    }
                });
            }
            let Some(bytes) = timed(&mut times.fetch_ns, || fetch(hash)) else {
                stat.reason = Some(NoVariant::Missing);
                stat.times = times;
                return Ok((row(None), stat, Vec::new()));
            };
            stat.source = source_kind(&bytes);
            stat.original_size = bytes.len() as u64;
            let v = match encode_texture_with(&bytes, slot, tex_opts, &mut times) {
                Ok(Ok(v)) => v,
                Ok(Err(Miss { reason })) => {
                    stat.reason = Some(reason);
                    let uploads = if texture_reason_is_final(reason) {
                        let rec = Record {
                            v: RECORD_VERSION,
                            variant: None,
                            reason: Some(reason_name(reason)),
                            br: false,
                            uses: BTreeMap::new(),
                        };
                        store(out_root, &key, None, None, &rec, &mut times)?
                    } else {
                        Vec::new()
                    };
                    stat.times = times;
                    return Ok((row(None), stat, uploads));
                }
                Err(e) => {
                    tracing::warn!(entity = %entity, hash = %hash, error = %format!("{e:#}"), "bevy texture variant skipped");
                    stat.reason = Some(NoVariant::DecodeError);
                    let rec = Record {
                        v: RECORD_VERSION,
                        variant: None,
                        reason: Some(reason_name(NoVariant::DecodeError)),
                        br: false,
                        uses: BTreeMap::new(),
                    };
                    let uploads = store(out_root, &key, None, None, &rec, &mut times)?;
                    stat.times = times;
                    return Ok((row(None), stat, uploads));
                }
            };
            let mut e = entry(&key, &v.bytes, "texture", v.format.manifest_name());
            e.width = Some(v.width);
            e.height = Some(v.height);
            e.mips = Some(v.mips);
            let rec = Record {
                v: RECORD_VERSION,
                variant: Some(e.clone()),
                reason: None,
                br: false,
                uses: BTreeMap::new(),
            };
            let uploads = store(out_root, &key, Some(&v.bytes), None, &rec, &mut times)?;
            stat.format = Some(v.format.manifest_name());
            stat.variant_path = Some(key.clone());
            stat.variant_cid = Some(e.cid.clone());
            stat.variant_size = Some(e.size);
            stat.width = Some(v.width);
            stat.height = Some(v.height);
            stat.mips = Some(v.mips);
            stat.bc1_psnr_db = v.bc1_psnr_db.map(|p| p as f32);
            stat.bc1_rejected = v.bc1_psnr_db.map(|_| v.bc1_rejected);
            stat.times = times;
            Ok((row(Some(e)), stat, uploads))
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .fold(
            (Vec::new(), Vec::new(), Vec::new()),
            |(mut a, mut b, mut c), (r, s, u)| {
                a.push(r);
                b.push(s);
                c.push(u);
                (a, b, c)
            },
        );

    let mut files: BTreeMap<String, Vec<VariantEntry>> = BTreeMap::new();
    let mut uploads: Vec<Upload> = tex_uploads.into_iter().flatten().collect();
    let mut model_stats: Vec<ModelStat> = Vec::with_capacity(model_outs.len());
    let mut glb_stats: Vec<GlbStat> = Vec::new();
    let mut incomplete: Vec<String> = file_stats
        .iter()
        .filter(|s| s.reason.is_some_and(|r| !texture_reason_is_final(r)))
        .map(|s| s.file.clone())
        .collect();
    for (&(_, file), m) in models.iter().zip(&model_outs) {
        if m.incomplete {
            incomplete.push(file.to_string());
        }
    }
    incomplete.sort();
    for (hash, entries) in textures {
        if !entries.is_empty() {
            files.insert(hash, entries);
        }
    }
    for m in model_outs {
        if !m.row.1.is_empty() {
            files.entry(m.row.0).or_default().extend(m.row.1);
        }
        uploads.extend(m.uploads);
        model_stats.extend(m.stat);
        glb_stats.extend(m.glb);
    }
    let manifest = Manifest {
        v: MANIFEST_VERSION,
        lanes: opts.lanes(),
        files: files
            .iter()
            .map(|(cid, es)| (cid.clone(), es.iter().map(|e| e.path.clone()).collect()))
            .collect(),
    };
    let manifest_bytes = serde_json::to_vec(&manifest).context("serialize manifest")?;
    if incomplete.is_empty() {
        write_atomic(&manifest_dst, &manifest_bytes)?;
    } else {
        tracing::warn!(entity = %entity, files = ?incomplete, "bevy variants: incomplete build; manifest not written");
        // An older manifest here was not built from everything this build needs (or it would
        // have reused its records instead of fetching): nothing serves a partial manifest.
        match std::fs::remove_file(&manifest_dst) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("remove {}", manifest_dst.display()));
            }
        }
    }
    let mut stats = BuildStats::from_files(file_stats, opts.zstd_level, started.elapsed());
    stats.models = models.len() as u64;
    stats.add_models(model_stats);
    stats.add_glbs(glb_stats);
    let built: BTreeMap<&str, u64> = uploads
        .iter()
        .filter(|u| !u.record && !u.key.ends_with(".br"))
        .filter_map(|u| {
            std::fs::metadata(&u.path)
                .ok()
                .map(|m| (u.key.as_str(), m.len()))
        })
        .collect();
    stats.unique_variants = built.len() as u64;
    stats.unique_variant_bytes = built.values().sum();
    Ok(BuildOutput {
        manifest,
        entries: files,
        manifest_bytes,
        incomplete,
        uploads,
        stats,
    })
}

/// The model's bytes, fetched once (the first caller pays the fetch time).
fn ensure_bytes<'a>(
    slot: &'a mut Option<Vec<u8>>,
    fetched: &mut bool,
    fetch: &(dyn Fn(&str) -> Option<Vec<u8>> + Sync),
    hash: &str,
    times: &mut StageTimes,
) -> Option<&'a [u8]> {
    if !*fetched {
        *slot = timed(&mut times.fetch_ns, || fetch(hash));
        *fetched = true;
    }
    slot.as_deref()
}

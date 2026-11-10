//! The per-entity builder: name every input's key, probe for it, fetch and encode the misses,
//! store bytes and records, write the manifest last.

use anyhow::{Context, Result};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use super::meshopt::{self, NoModelVariant};
use super::model::{image_uses, transform_model_with};
use super::stats::{
    elapsed_ns, timed, BuildStats, FileStat, ModelStat, NoVariant, StageTimes,
};
use super::texture::{encode_texture_with, source_kind, Miss, Slot, TexOptions, Uses};
use super::{
    cid_v1_raw_sha256, generator, key_path, manifest_path, record_key, variant_key, Class,
    ImageUse, Manifest, Record, VariantEntry, MANIFEST_VERSION, RECORD_VERSION,
};
use crate::catalyst::Scene;

/// What a build produces besides texture variants, and how.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuildOptions {
    /// Also build GLB model variants. Off by default: Bevy's glTF loader does not take them yet
    /// (it rejects `KHR_texture_basisu` in `extensionsRequired`, so a GLB embedding KTX2 cannot
    /// load), so the manifest lists texture variants only. Images embedded in GLBs never get a
    /// texture variant of their own either way.
    pub glb_variants: bool,
    /// `glb-meshopt` model variants (`super::meshopt`): on by default, independent of
    /// `glb_variants`.
    pub glb_meshopt: bool,
    /// Store each `glb-meshopt` variant Brotli-compressed as `{key}.br` beside it, too. Only
    /// a variant built by this run gets one: a reused variant keeps whatever its builder left.
    pub precompress_br: bool,
    /// Zstandard level of every KTX2 level ([`crate::ktx2_write::DEFAULT_ZSTD_LEVEL`] by
    /// default). Part of the variant bytes, so part of the texture prefix.
    pub zstd_level: i32,
}

impl Default for BuildOptions {
    fn default() -> Self {
        BuildOptions {
            glb_variants: false,
            glb_meshopt: true,
            precompress_br: false,
            zstd_level: crate::ktx2_write::DEFAULT_ZSTD_LEVEL,
        }
    }
}

/// The env var that sets [`BuildOptions::zstd_level`].
pub const ZSTD_LEVEL_ENV: &str = "ABGEN_BEVY_ZSTD_LEVEL";
/// `0` turns [`BuildOptions::glb_meshopt`] off.
pub const GLB_MESHOPT_ENV: &str = "ABGEN_BEVY_GLB_MESHOPT";
/// `br` turns [`BuildOptions::precompress_br`] on (anything else: off).
pub const PRECOMPRESS_ENV: &str = "ABGEN_BEVY_PRECOMPRESS";

impl BuildOptions {
    /// `ABGEN_BEVY_GLB_VARIANTS=1` turns GLB variants on (default off);
    /// `ABGEN_BEVY_GLB_MESHOPT=0` turns `glb-meshopt` variants off (default on);
    /// `ABGEN_BEVY_PRECOMPRESS=br` stores `.br` sidecars of them (default off);
    /// `ABGEN_BEVY_ZSTD_LEVEL` (1..=19) sets the Zstandard level (default 19; an invalid value
    /// is logged and the default is used).
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
            glb_variants: crate::clihelp::env_bool("ABGEN_BEVY_GLB_VARIANTS", false),
            glb_meshopt: crate::clihelp::env_bool(GLB_MESHOPT_ENV, true),
            precompress_br: std::env::var(PRECOMPRESS_ENV)
                .is_ok_and(|v| v.trim().eq_ignore_ascii_case("br")),
            zstd_level,
        }
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
            Class::Texture.prefix(self.zstd_level),
        );
        if self.glb_meshopt {
            out.insert(
                Class::Mesh.lane().to_string(),
                Class::Mesh.prefix(self.zstd_level),
            );
        }
        if self.glb_variants {
            out.insert(
                Class::Glb.lane().to_string(),
                Class::Glb.prefix(self.zstd_level),
            );
        }
        out
    }
}

/// Where a build reads from. `fetch` returns a content file's bytes by hash (`None` =
/// unavailable: that file gets no variant and no record). `probe` returns the record of a
/// variant key that already exists somewhere this build does not write (the store, through
/// Redis); `None` is a miss. Local records under `out_root` are read before `probe` is asked.
pub struct Lane<'a> {
    pub fetch: &'a (dyn Fn(&str) -> Option<Vec<u8>> + Sync),
    pub probe: &'a (dyn Fn(&str) -> Option<Record> + Sync),
}

/// One object this build wrote under `out_root`, in the order the store must receive them:
/// a variant's bytes, then its `.br` sidecar, then its record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upload {
    /// Object key, also the path under `out_root`.
    pub key: String,
    pub path: PathBuf,
    /// This is a record (`{key}.json`): once stored it is marked in Redis.
    pub record: bool,
}

pub struct BuildOutput {
    pub manifest: Manifest,
    /// The manifest exactly as written to `bevy/manifest/{entity}.json`.
    pub manifest_bytes: Vec<u8>,
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

/// The record of `key` under `out_root`, if this or an earlier build here wrote one.
fn local_record(out_root: &Path, key: &str) -> Option<Record> {
    let p = key_path(out_root, &record_key(key))?;
    let bytes = std::fs::read(p).ok()?;
    Record::parse(&bytes)
}

/// Local disk, then the lane's probe.
fn probe(lane: &Lane, out_root: &Path, key: &str, times: &mut StageTimes) -> Option<Record> {
    timed(&mut times.probe_ns, || {
        local_record(out_root, key).or_else(|| (lane.probe)(key))
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
            path: dst.clone(),
            record: false,
        });
    }
    if let Some(b) = br {
        let br_dst = meshopt::br_sidecar(&dst);
        if !br_dst.is_file() {
            write_atomic(&br_dst, b)?;
        }
        out.push(Upload {
            key: format!("{key}.br"),
            path: br_dst,
            record: false,
        });
    }
    let rk = record_key(key);
    let rec_dst = key_path(out_root, &rk).expect("a variant key's record is a valid key");
    if !rec_dst.is_file() {
        write_atomic(&rec_dst, &serde_json::to_vec(record).context("serialize record")?)?;
    }
    out.push(Upload {
        key: rk,
        path: rec_dst,
        record: true,
    });
    times.hash_write_ns += elapsed_ns(started);
    Ok(out)
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
    ["ktx2-bc1", "ktx2-bc5", "ktx2-bc7", meshopt::FORMAT, "glb"]
        .into_iter()
        .find(|f| *f == name)
}

/// The variant name of a model: its source CID, plus, for a `.gltf`, a digest of the external
/// buffers it inlines (`uri` -> content hash, sorted), so the same `.gltf` beside different
/// `.bin` files is two variants. Image URIs are not inputs: the variant leaves them as they are.
fn model_name(hash: &str, file: &str, bytes: Option<&[u8]>, by_file: &HashMap<String, String>) -> String {
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
fn uses_by_uri(bytes: &[u8], ext: &str, resolve: crate::gltf::Resolve) -> BTreeMap<String, ImageUse> {
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
}

/// Build every variant of `scene` and its manifest under `out_root`, in the served layout.
/// Per-file failures are logged and leave that file without a variant; only I/O errors on
/// `out_root` fail the build. Variants and records are written before the manifest, so a
/// manifest on disk never names a variant that is not.
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
    let tex_prefix = Class::Texture.prefix(opts.zstd_level);
    let mesh_prefix = Class::Mesh.prefix(opts.zstd_level);
    let glb_prefix = Class::Glb.prefix(opts.zstd_level);
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
            // A `.gltf` names its buffers in its JSON: read it to name the variant.
            let name = if ext == ".gltf" {
                let b = ensure_bytes(&mut bytes, &mut fetched, fetch, hash, &mut times);
                model_name(hash, file, b, &content_by_file)
            } else {
                hash.to_string()
            };
            let mut out = ModelOut {
                row: (hash.to_string(), Vec::new()),
                stat: None,
                uploads: Vec::new(),
                uses: BTreeMap::new(),
            };
            let mut uses_known = false;

            if opts.glb_meshopt {
                let key = variant_key(&mesh_prefix, &name);
                let mut stat = ModelStat {
                    hash: hash.to_string(),
                    file: file.to_string(),
                    original_size: 0,
                    format: None,
                    variant_path: None,
                    variant_cid: None,
                    variant_size: None,
                    br_size: None,
                    streams: None,
                    gated_size: None,
                    reason: None,
                    reused: false,
                    times: StageTimes::default(),
                };
                if let Some(rec) = probe(lane, out_root, &key, &mut times) {
                    // `original_size` stays 0: the source was not fetched.
                    stat.reused = true;
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
                        None => stat.reason = Some(NoModelVariant::Missing),
                        Some(b) => {
                            stat.original_size = b.len() as u64;
                            out.uses = uses_by_uri(b, ext, Some(&resolve));
                            uses_known = true;
                            let caught = timed(&mut times.meshopt_ns, || {
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    meshopt::compress_model(b, ext, Some(&resolve))
                                }))
                            });
                            let outcome = match caught {
                                Ok(Ok(Ok(v))) => {
                                    stat.streams = Some(v.streams as u64);
                                    if v.glb.len() >= b.len() {
                                        stat.gated_size = Some(v.glb.len() as u64);
                                        Err(NoModelVariant::SizeGate)
                                    } else {
                                        Ok(v)
                                    }
                                }
                                Ok(Ok(Err(reason))) => Err(reason),
                                Ok(Err(e)) => {
                                    tracing::warn!(entity = %entity, file = %file, error = %format!("{e:#}"), "bevy glb-meshopt variant skipped");
                                    Err(NoModelVariant::Error)
                                }
                                Err(_) => {
                                    tracing::warn!(entity = %entity, file = %file, "bevy glb-meshopt variant panicked; skipped");
                                    Err(NoModelVariant::Error)
                                }
                            };
                            match outcome {
                                Ok(v) => {
                                    let br = if opts.precompress_br {
                                        Some(timed(&mut times.brotli_ns, || {
                                            meshopt::brotli_compress(&v.glb)
                                        })?)
                                    } else {
                                        None
                                    };
                                    let e = entry(&key, &v.glb, "model", meshopt::FORMAT);
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
                                        &mut times,
                                    )?);
                                    stat.format = Some(meshopt::FORMAT);
                                    stat.variant_path = Some(key.clone());
                                    stat.variant_cid = Some(e.cid.clone());
                                    stat.variant_size = Some(e.size);
                                    stat.br_size = br.map(|b| b.len() as u64);
                                    out.row.1.push(e);
                                }
                                Err(reason) => {
                                    stat.reason = Some(reason);
                                    if model_reason_is_final(reason) {
                                        let rec = Record {
                                            v: RECORD_VERSION,
                                            variant: None,
                                            reason: Some(reason_name(reason)),
                                            br: false,
                                            uses: out.uses.clone(),
                                        };
                                        out.uploads.extend(store(
                                            out_root, &key, None, None, &rec, &mut times,
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
                let key = variant_key(&glb_prefix, &name);
                if let Some(rec) = probe(lane, out_root, &key, &mut times) {
                    if !uses_known {
                        out.uses = rec.uses.clone();
                        uses_known = true;
                    }
                    out.row.1.extend(rec.variant);
                } else if let Some(b) =
                    ensure_bytes(&mut bytes, &mut fetched, fetch, hash, &mut times)
                {
                    if !uses_known {
                        out.uses = uses_by_uri(b, ext, Some(&resolve));
                        uses_known = true;
                    }
                    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        transform_model_with(b, ext, Some(&resolve), tex_opts)
                    }));
                    // A variant, or "nothing to change", is a final answer; a failure is not.
                    match caught {
                        Ok(Ok(Some(v))) => {
                            let e = entry(&key, &v.glb, "model", "glb");
                            let rec = Record {
                                v: RECORD_VERSION,
                                variant: Some(e.clone()),
                                reason: None,
                                br: false,
                                uses: out.uses.clone(),
                            };
                            out.uploads.extend(store(
                                out_root,
                                &key,
                                Some(&v.glb),
                                None,
                                &rec,
                                &mut times,
                            )?);
                            out.row.1.push(e);
                        }
                        Ok(Ok(None)) => {
                            let rec = Record {
                                v: RECORD_VERSION,
                                variant: None,
                                reason: Some("nothing-to-change".to_string()),
                                br: false,
                                uses: out.uses.clone(),
                            };
                            out.uploads
                                .extend(store(out_root, &key, None, None, &rec, &mut times)?);
                        }
                        Ok(Err(e)) => {
                            tracing::warn!(entity = %entity, file = %file, error = %format!("{e:#}"), "bevy model variant skipped");
                        }
                        Err(_) => {
                            tracing::warn!(entity = %entity, file = %file, "bevy model variant panicked; skipped");
                        }
                    }
                }
            }

            if !uses_known {
                if let Some(b) = ensure_bytes(&mut bytes, &mut fetched, fetch, hash, &mut times) {
                    out.uses = uses_by_uri(b, ext, Some(&resolve));
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
                gated_size: None,
                gated_format: None,
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
                Ok(Err(Miss {
                    reason,
                    gated_size,
                    gated_format,
                })) => {
                    stat.reason = Some(reason);
                    stat.gated_size = gated_size;
                    stat.gated_format = gated_format;
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
    }
    let manifest = Manifest {
        v: MANIFEST_VERSION,
        entity: entity.to_string(),
        generator: generator(),
        lanes: opts.lanes(),
        files,
    };
    let manifest_bytes = serde_json::to_vec(&manifest).context("serialize manifest")?;
    write_atomic(&manifest_dst, &manifest_bytes)?;
    let mut stats = BuildStats::from_files(file_stats, opts.zstd_level, started.elapsed());
    stats.models = models.len() as u64;
    stats.add_models(model_stats);
    let built: BTreeMap<&str, u64> = uploads
        .iter()
        .filter(|u| !u.record && !u.key.ends_with(".br"))
        .filter_map(|u| std::fs::metadata(&u.path).ok().map(|m| (u.key.as_str(), m.len())))
        .collect();
    stats.unique_variants = built.len() as u64;
    stats.unique_variant_bytes = built.values().sum();
    Ok(BuildOutput {
        manifest,
        manifest_bytes,
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

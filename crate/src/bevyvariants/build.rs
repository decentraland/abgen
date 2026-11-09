//! The per-entity builder: classify, encode, store by CID, write the manifest last.

use anyhow::{Context, Result};
use rayon::prelude::*;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use super::model::{image_uses, transform_model_with};
use super::stats::{elapsed_ns, timed, BuildStats, FileStat, NoVariant, StageTimes};
use super::texture::{encode_texture_with, source_kind, Miss, TexOptions, Uses};
use super::{
    cid_v1_raw_sha256, content_path, generator, manifest_path, Manifest, VariantEntry,
    MANIFEST_VERSION,
};
use crate::catalyst::Scene;

/// One original's outcome: its hash, and its variant entry and file when it got one.
type Row = (String, Option<VariantEntry>, Option<PathBuf>);

/// What a build produces besides texture variants, and how.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuildOptions {
    /// Also build GLB model variants. Off by default: Bevy's glTF loader does not take them yet
    /// (it rejects `KHR_texture_basisu` in `extensionsRequired`, so a GLB embedding KTX2 cannot
    /// load), so the manifest lists texture variants only. Images embedded in GLBs never get a
    /// texture variant of their own either way.
    pub glb_variants: bool,
    /// Zstandard level of every KTX2 level ([`crate::ktx2_write::DEFAULT_ZSTD_LEVEL`] by
    /// default). Part of the variant bytes: a different level gives different CIDs.
    pub zstd_level: i32,
}

impl Default for BuildOptions {
    fn default() -> Self {
        BuildOptions {
            glb_variants: false,
            zstd_level: crate::ktx2_write::DEFAULT_ZSTD_LEVEL,
        }
    }
}

/// The env var that sets [`BuildOptions::zstd_level`].
pub const ZSTD_LEVEL_ENV: &str = "ABGEN_BEVY_ZSTD_LEVEL";

impl BuildOptions {
    /// `ABGEN_BEVY_GLB_VARIANTS=1` turns GLB variants on (default off);
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
            zstd_level,
        }
    }

    fn tex(self) -> TexOptions {
        TexOptions {
            zstd_level: self.zstd_level,
        }
    }
}

pub struct BuildOutput {
    pub manifest: Manifest,
    /// The manifest exactly as written to `bevy/manifest/{entity}.json`.
    pub manifest_bytes: Vec<u8>,
    /// Every variant the manifest lists, with the file holding its bytes.
    pub variants: Vec<(String, PathBuf)>,
    /// Stage times and outcomes of the standalone textures (not part of any hashed output).
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

/// Write `bytes` at `bevy/contents/{cid}` unless it is already there (content-addressed, so an
/// existing file is the same bytes). Returns the CID and path.
fn store_variant(
    out_root: &Path,
    bytes: &[u8],
    times: &mut StageTimes,
) -> Result<(String, PathBuf)> {
    let started = std::time::Instant::now();
    let out = store_variant_untimed(out_root, bytes);
    times.hash_write_ns += elapsed_ns(started);
    out
}

fn store_variant_untimed(out_root: &Path, bytes: &[u8]) -> Result<(String, PathBuf)> {
    let cid = cid_v1_raw_sha256(bytes);
    let dst = content_path(out_root, &cid).expect("a CID is a valid id");
    if !dst.is_file() {
        write_atomic(&dst, bytes)?;
    }
    Ok((cid, dst))
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

/// Build every variant of `scene` and its manifest under `out_root`, in the served layout.
/// `fetch` returns a content file's bytes by hash (`None` = unavailable: that file gets no
/// variant). Per-file failures are logged and leave that file without a variant; only I/O
/// errors on `out_root` fail the build. Variants are written before the manifest, so a
/// manifest on disk never names a variant that is not.
pub fn build_entity_variants(
    scene: &Scene,
    fetch: &(dyn Fn(&str) -> Option<Vec<u8>> + Sync),
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

    let model_bytes: HashMap<&str, Vec<u8>> = models
        .par_iter()
        .filter_map(|&(h, _)| fetch(h).map(|b| (h, b)))
        .collect();

    // Pass 1: how the models sample each external image.
    let mut uses: HashMap<String, Uses> = HashMap::new();
    for (hash, file) in &models {
        let Some(bytes) = model_bytes.get(hash) else {
            continue;
        };
        let resolve = |uri: &str| -> Option<Vec<u8>> {
            fetch(crate::naming::uri_content_hash(uri, file, &content_by_file)?.as_str())
        };
        let Some((per_image, uris)) = image_uses(bytes, model_ext(file), Some(&resolve)) else {
            continue;
        };
        for (u, uri) in per_image.into_iter().zip(uris) {
            let Some(uri) = uri else { continue };
            if let Some(h) = crate::naming::uri_content_hash(&uri, file, &content_by_file) {
                uses.entry(h.clone()).or_default().merge(u);
            }
        }
    }

    // Pass 2: standalone textures.
    let tex_opts = opts.tex();
    let (textures, file_stats): (Vec<Row>, Vec<FileStat>) = images
        .par_iter()
        .map(|&(hash, file)| -> Result<(Row, FileStat)> {
            let mut times = StageTimes::default();
            let mut stat = FileStat {
                hash: hash.to_string(),
                file: file.to_string(),
                source: super::stats::SourceKind::Other,
                original_size: 0,
                format: None,
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
                times,
            };
            let none = (hash.to_string(), None, None);
            let Some(bytes) = timed(&mut times.fetch_ns, || fetch(hash)) else {
                stat.reason = Some(NoVariant::Missing);
                stat.times = times;
                return Ok((none, stat));
            };
            stat.source = source_kind(&bytes);
            stat.original_size = bytes.len() as u64;
            let slot = uses.get(hash).copied().unwrap_or_default().slot();
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
                    stat.times = times;
                    return Ok((none, stat));
                }
                Err(e) => {
                    tracing::warn!(entity = %entity, hash = %hash, error = %format!("{e:#}"), "bevy texture variant skipped");
                    stat.reason = Some(NoVariant::DecodeError);
                    stat.times = times;
                    return Ok((none, stat));
                }
            };
            let (cid, path) = store_variant(out_root, &v.bytes, &mut times)?;
            stat.format = Some(v.format.manifest_name());
            stat.variant_cid = Some(cid.clone());
            stat.variant_size = Some(v.bytes.len() as u64);
            stat.width = Some(v.width);
            stat.height = Some(v.height);
            stat.mips = Some(v.mips);
            stat.bc1_psnr_db = v.bc1_psnr_db.map(|p| p as f32);
            stat.bc1_rejected = v.bc1_psnr_db.map(|_| v.bc1_rejected);
            stat.times = times;
            Ok((
                (
                    hash.to_string(),
                    Some(VariantEntry {
                        cid,
                        kind: "texture".into(),
                        format: v.format.manifest_name().into(),
                        width: Some(v.width),
                        height: Some(v.height),
                        mips: Some(v.mips),
                        size: v.bytes.len() as u64,
                    }),
                    Some(path),
                ),
                stat,
            ))
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .unzip();

    // Pass 3: models (opt-in, see `BuildOptions::glb_variants`).
    let model_variants: Vec<Row> = models
        .par_iter()
        .filter(|_| opts.glb_variants)
        .map(|&(hash, file)| -> Result<Row> {
            let none = (hash.to_string(), None, None);
            let Some(bytes) = model_bytes.get(hash) else {
                return Ok(none);
            };
            let resolve = |uri: &str| -> Option<Vec<u8>> {
                fetch(crate::naming::uri_content_hash(uri, file, &content_by_file)?.as_str())
            };
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                transform_model_with(bytes, model_ext(file), Some(&resolve), tex_opts)
            }));
            let v = match caught {
                Ok(Ok(Some(v))) => v,
                Ok(Ok(None)) => return Ok(none),
                Ok(Err(e)) => {
                    tracing::warn!(entity = %entity, file = %file, error = %format!("{e:#}"), "bevy model variant skipped");
                    return Ok(none);
                }
                Err(_) => {
                    tracing::warn!(entity = %entity, file = %file, "bevy model variant panicked; skipped");
                    return Ok(none);
                }
            };
            let (cid, path) = store_variant(out_root, &v.glb, &mut StageTimes::default())?;
            Ok((
                hash.to_string(),
                Some(VariantEntry {
                    cid,
                    kind: "model".into(),
                    format: "glb".into(),
                    width: None,
                    height: None,
                    mips: None,
                    size: v.glb.len() as u64,
                }),
                Some(path),
            ))
        })
        .collect::<Result<_>>()?;

    let mut files: BTreeMap<String, Vec<VariantEntry>> = BTreeMap::new();
    let mut variants: Vec<(String, PathBuf)> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for (hash, entry, path) in textures.into_iter().chain(model_variants) {
        if let (Some(entry), Some(path)) = (entry, path) {
            if seen.insert(entry.cid.clone()) {
                variants.push((entry.cid.clone(), path));
            }
            files.entry(hash).or_default().push(entry);
        }
    }
    let manifest = Manifest {
        v: MANIFEST_VERSION,
        entity: entity.to_string(),
        generator: generator(),
        files,
    };
    let manifest_bytes = serde_json::to_vec(&manifest).context("serialize manifest")?;
    write_atomic(&manifest_dst, &manifest_bytes)?;
    let mut stats = BuildStats::from_files(file_stats, opts.zstd_level, started.elapsed());
    stats.models = models.len() as u64;
    let mut unique: BTreeMap<&str, u64> = BTreeMap::new();
    for f in &stats.files {
        if let (Some(c), Some(n)) = (&f.variant_cid, f.variant_size) {
            unique.insert(c, n);
        }
    }
    let (unique_count, unique_bytes) = (unique.len() as u64, unique.values().sum::<u64>());
    stats.unique_variants = unique_count;
    stats.unique_variant_bytes = unique_bytes;
    Ok(BuildOutput {
        manifest,
        manifest_bytes,
        variants,
        stats,
    })
}

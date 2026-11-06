//! The per-entity builder: classify, encode, store by CID, write the manifest last.

use anyhow::{Context, Result};
use rayon::prelude::*;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use super::model::{image_uses, transform_model};
use super::texture::{encode_texture, Uses};
use super::{
    cid_v1_raw_sha256, content_path, generator, manifest_path, Manifest, VariantEntry,
    MANIFEST_VERSION,
};
use crate::catalyst::Scene;

/// One original's outcome: its hash, and its variant entry and file when it got one.
type Row = (String, Option<VariantEntry>, Option<PathBuf>);

pub struct BuildOutput {
    pub manifest: Manifest,
    /// The manifest exactly as written to `bevy/manifest/{entity}.json`.
    pub manifest_bytes: Vec<u8>,
    /// Every variant the manifest lists, with the file holding its bytes.
    pub variants: Vec<(String, PathBuf)>,
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
fn store_variant(out_root: &Path, bytes: &[u8]) -> Result<(String, PathBuf)> {
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
) -> Result<BuildOutput> {
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
        let e = by_hash.entry(c.hash.as_str()).or_insert((c.file.as_str(), kind));
        if c.file.as_str() < e.0 {
            *e = (c.file.as_str(), kind);
        }
    }
    let models: Vec<(&str, &str)> = by_hash
        .iter()
        .filter(|(_, (_, k))| *k == Kind::Model)
        .map(|(h, (f, _))| (*h, *f))
        .collect();
    let images: Vec<&str> = by_hash
        .iter()
        .filter(|(_, (_, k))| *k == Kind::Image)
        .map(|(h, _)| *h)
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
    let textures: Vec<Row> = images
        .par_iter()
        .map(|&hash| -> Result<Row> {
            let none = (hash.to_string(), None, None);
            let Some(bytes) = fetch(hash) else {
                return Ok(none);
            };
            let slot = uses.get(hash).copied().unwrap_or_default().slot();
            let v = match encode_texture(&bytes, slot) {
                Ok(Some(v)) => v,
                Ok(None) => return Ok(none),
                Err(e) => {
                    tracing::warn!(entity = %entity, hash = %hash, error = %format!("{e:#}"), "bevy texture variant skipped");
                    return Ok(none);
                }
            };
            let (cid, path) = store_variant(out_root, &v.dds)?;
            Ok((
                hash.to_string(),
                Some(VariantEntry {
                    cid,
                    kind: "texture".into(),
                    format: v.format.manifest_name().into(),
                    width: Some(v.width),
                    height: Some(v.height),
                    mips: Some(v.mips),
                    size: v.dds.len() as u64,
                }),
                Some(path),
            ))
        })
        .collect::<Result<_>>()?;

    // Pass 3: models.
    let model_variants: Vec<Row> = models
        .par_iter()
        .map(|&(hash, file)| -> Result<Row> {
            let none = (hash.to_string(), None, None);
            let Some(bytes) = model_bytes.get(hash) else {
                return Ok(none);
            };
            let resolve = |uri: &str| -> Option<Vec<u8>> {
                fetch(crate::naming::uri_content_hash(uri, file, &content_by_file)?.as_str())
            };
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                transform_model(bytes, model_ext(file), Some(&resolve))
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
            let (cid, path) = store_variant(out_root, &v.glb)?;
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
    Ok(BuildOutput {
        manifest,
        manifest_bytes,
        variants,
    })
}

use crate::dedup::{
    reconcile_divergent, record_claim, variant_key_for, FirstWritten, ReconcileStats,
};
use crate::{BundleSpec, ContentItem, EffectiveToggles, EntityEntry};
use abgen::builder::{build_bundle_multi, BuildOpts};
use abgen::local_store::LocalContentStore;
use abgen::{naming, Result};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

pub(crate) fn bundle_tmp_path(out_path: &Path) -> PathBuf {
    static TMP_SEQ: AtomicUsize = AtomicUsize::new(0);
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let name = out_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    out_path.with_file_name(format!("{name}.tmp.{}.{seq}", std::process::id()))
}

fn write_bundle_atomic(out_path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = bundle_tmp_path(out_path);
    std::fs::write(&tmp, data)
        .and_then(|_| std::fs::rename(&tmp, out_path))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
}

pub(crate) fn link_or_copy_atomic(src: &Path, dst: &Path) -> bool {
    let tmp = bundle_tmp_path(dst);
    let staged = std::fs::hard_link(src, &tmp).is_ok() || std::fs::copy(src, &tmp).is_ok();
    if staged && std::fs::rename(&tmp, dst).is_ok() {
        return true;
    }
    let _ = std::fs::remove_file(&tmp);
    false
}

pub(crate) fn build_one(
    store: &LocalContentStore,
    content_by_file: &HashMap<String, String>,
    spec: &BundleSpec,
    out_path: &std::path::Path,
    toggles: EffectiveToggles,
) -> Result<()> {
    build_group(store, content_by_file, &[spec], &[out_path], toggles)
}

fn build_group(
    store: &LocalContentStore,
    content_by_file: &HashMap<String, String>,
    specs: &[&BundleSpec],
    out_paths: &[&Path],
    toggles: EffectiveToggles,
) -> Result<()> {
    let spec = specs[0];
    let glb = store.fetch_mmap(&spec.cid)?;
    let effective_source = spec
        .source_file
        .clone()
        .unwrap_or_else(|| format!("{}.glb", spec.cid));
    let resolve_fn = |uri: &str| -> Option<Vec<u8>> {
        let h = naming::uri_content_hash(uri, &effective_source, content_by_file)?;
        store.fetch(h).ok()
    };
    let resolve: abgen::gltf::Resolve = if !content_by_file.is_empty() {
        Some(&resolve_fn)
    } else {
        None
    };
    let resolve_hash_fn = |uri: &str| -> Option<String> {
        naming::uri_content_hash(uri, &effective_source, content_by_file).cloned()
    };
    let resolve_hash: Option<abgen::builder::ResolveHash> =
        if !content_by_file.is_empty() && spec.source_file.is_some() {
            Some(&resolve_hash_fn)
        } else {
            None
        };
    let opts = BuildOpts {
        keep_forward_plus: true,
        source_file: Some(&effective_source),
        entity_type: spec.entity_type.as_deref(),
        resolve,
        resolve_hash,
        model_referenced: spec.model_referenced,
        metadata_dependencies: &spec.metadata_deps,
        expect_hash: spec.expect_hash.as_deref(),
        standalone_color_space: spec.standalone_color_space,
        standalone_normal: spec.standalone_normal,
        force_default_material: spec.force_default_material,
        magenta_missing: toggles.magenta_missing,
        collection_mode: toggles.collection_mode,
        real_textures: toggles.real_textures,
        v38_compat: toggles.v38_compat,
        v38_timestamp: toggles.v38_timestamp,
        lod: None,
    };
    let names: Vec<String> = specs.iter().map(|s| s.bundle_name.clone()).collect();
    let artifacts = build_bundle_multi(&glb[..], &names, &spec.cid, &opts)?;
    for (artifact, out_path) in artifacts.iter().zip(out_paths) {
        write_bundle_atomic(out_path, &artifact.data)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn write_cdn_manifest(
    out_root: &Path,
    entity_id: &str,
    platform: &str,
    bundles: &[BundleSpec],
    ab_version: &str,
    content_server_url: &str,
    date: &str,
) -> Result<usize> {
    let mut names: Vec<String> = bundles.iter().map(|b| b.bundle_name.clone()).collect();
    names.sort();
    names.dedup();
    let bundle_dir = out_root.join(entity_id).join(platform);
    let (built, missing): (Vec<String>, Vec<String>) = names
        .into_iter()
        .partition(|n| bundle_dir.join(n).is_file());
    if built.is_empty() && !missing.is_empty() {
        return Ok(missing.len());
    }

    abgen::manifest::write_corpus_manifest(&abgen::manifest::CorpusManifestSpec {
        out_root,
        entity_id,
        platform,
        built: &built,
        ab_version,
        content_server_url,
        exit_code: abgen::manifest::exit_code_for_failures(missing.len()),
        date,
    })?;
    Ok(missing.len())
}

pub(crate) struct BuildOutcome {
    pub(crate) built: usize,
    pub(crate) errs: usize,
    pub(crate) skipped: usize,
    pub(crate) manifest_errs: usize,
    pub(crate) manifest_incomplete: usize,
    pub(crate) reconcile: ReconcileStats,
}

pub(crate) struct BuildCounters<'a> {
    pub(crate) built: &'a AtomicUsize,
    pub(crate) errs: &'a AtomicUsize,
    pub(crate) skipped: &'a AtomicUsize,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_bundle_at(
    store: &LocalContentStore,
    content_by_file: &HashMap<String, String>,
    spec: &BundleSpec,
    ent_out: &Path,
    entity_id: &str,
    toggles: EffectiveToggles,
    skip_existing: bool,
    force: bool,
    first_written: Option<&FirstWritten>,
    c: &BuildCounters,
) {
    let ent_outs = [ent_out.to_path_buf()];
    build_bundle_multi_at(
        store,
        content_by_file,
        &[spec],
        &ent_outs,
        entity_id,
        toggles,
        skip_existing,
        force,
        &[first_written],
        c,
    );
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_bundle_multi_at(
    store: &LocalContentStore,
    content_by_file: &HashMap<String, String>,
    specs: &[&BundleSpec],
    ent_outs: &[PathBuf],
    entity_id: &str,
    toggles: EffectiveToggles,
    skip_existing: bool,
    force: bool,
    first_written: &[Option<&FirstWritten>],
    c: &BuildCounters,
) {
    let vkey = first_written
        .iter()
        .any(Option::is_some)
        .then(|| variant_key_for(store, content_by_file, specs[0], toggles))
        .flatten();
    let mut pending: Vec<usize> = Vec::new();
    for (i, spec) in specs.iter().enumerate() {
        let out_path = ent_outs[i].join(&spec.bundle_name);
        if skip_existing && !force {
            if let Ok(m) = std::fs::metadata(&out_path) {
                if m.is_file() && m.len() > 0 {
                    c.skipped.fetch_add(1, Ordering::Relaxed);
                    if let Some(fw) = first_written[i] {
                        record_claim(fw, &spec.bundle_name, &out_path, vkey.as_ref(), entity_id);
                    }
                    continue;
                }
            }
        }
        if let Some(fw) = first_written[i] {
            let prior = fw
                .lock()
                .unwrap()
                .get(&spec.bundle_name)
                .map(|e| e.path.clone());
            if let Some(src) = prior {
                if link_or_copy_atomic(&src, &out_path) {
                    c.built.fetch_add(1, Ordering::Relaxed);
                    record_claim(fw, &spec.bundle_name, &out_path, vkey.as_ref(), entity_id);
                } else {
                    c.errs.fetch_add(1, Ordering::Relaxed);
                    eprintln!("link {}/{}: failed", entity_id, spec.bundle_name);
                }
                continue;
            }
        }
        pending.push(i);
    }
    if pending.is_empty() {
        return;
    }
    let group_specs: Vec<&BundleSpec> = pending.iter().map(|&i| specs[i]).collect();
    let out_paths: Vec<PathBuf> = pending
        .iter()
        .map(|&i| ent_outs[i].join(&specs[i].bundle_name))
        .collect();
    let out_path_refs: Vec<&Path> = out_paths.iter().map(|p| p.as_path()).collect();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        build_group(
            store,
            content_by_file,
            &group_specs,
            &out_path_refs,
            toggles,
        )
    }));
    match result {
        Ok(Ok(_)) => {
            for (&i, out_path) in pending.iter().zip(out_paths.iter()) {
                if let Some(fw) = first_written[i] {
                    record_claim(
                        fw,
                        &specs[i].bundle_name,
                        out_path,
                        vkey.as_ref(),
                        entity_id,
                    );
                }
                c.built.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(Err(e)) => {
            c.errs.fetch_add(pending.len(), Ordering::Relaxed);
            eprintln!("err {}/{}: {e}", entity_id, group_specs[0].bundle_name);
        }
        Err(_) => {
            c.errs.fetch_add(pending.len(), Ordering::Relaxed);
            eprintln!(
                "panic {}/{} (skipped)",
                entity_id, group_specs[0].bundle_name
            );
        }
    }
}

fn sibling_spec(spec: &BundleSpec, primary: &str, platform: &str) -> BundleSpec {
    let old_suffix = format!("_{primary}");
    let swap = |s: &str| -> String {
        let swapped = s
            .strip_suffix(old_suffix.as_str())
            .map(|stem| format!("{stem}_{platform}"))
            .unwrap_or_else(|| s.to_string());
        if platform == "mac" {
            swapped.to_lowercase()
        } else {
            swapped
        }
    };
    BundleSpec {
        bundle_name: swap(&spec.bundle_name),
        metadata_deps: spec.metadata_deps.iter().map(|d| swap(d)).collect(),
        ..spec.clone()
    }
}

pub(crate) fn derive_one_entity(
    store: &LocalContentStore,
    ent_id: &str,
    platform: &str,
    uri_cache: &abgen::glbscan::UriCache,
    toggles: EffectiveToggles,
) -> Option<EntityEntry> {
    let entity = load_entity_json(store, ent_id)?;
    let entity_type = entity
        .get("type")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let content_items: Vec<ContentItem> = entity
        .get("content")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|c| {
                    let f = c.get("file").and_then(|v| v.as_str())?.to_string();
                    let h = c.get("hash").and_then(|v| v.as_str())?.to_string();
                    Some(ContentItem { file: f, hash: h })
                })
                .collect()
        })
        .unwrap_or_default();
    if content_items.is_empty() {
        return None;
    }
    let content_by_file: HashMap<String, String> = content_items
        .iter()
        .map(|c| (c.file.to_lowercase(), c.hash.clone()))
        .collect();
    let scan = abgen::glbscan::scan_entity(store, &content_by_file, uri_cache);
    let (model_refs, linear_refs, normal_refs) =
        (&scan.model_refs, &scan.linear_refs, &scan.normal_refs);

    let mut bundles: Vec<BundleSpec> = Vec::new();
    let mut local_seen: HashSet<String> = HashSet::new();
    for c in &content_items {
        let fl = c.file.to_lowercase();
        let is_glb = fl.ends_with(".glb") || fl.ends_with(".gltf");
        let is_image = IMAGE_EXTS.iter().any(|e| fl.ends_with(e));
        if !is_glb && !is_image {
            continue;
        }
        if !store.exists(&c.hash) {
            continue;
        }
        let case_hash = if platform == "mac" {
            c.hash.to_lowercase()
        } else {
            c.hash.clone()
        };
        let digest_naming =
            toggles.asset_reuse && is_glb && entity_type.as_deref() == Some("scene");
        let bundle_name = if digest_naming {
            let digest = store.fetch_mmap(&c.hash).ok().and_then(|bytes| {
                abgen::naming::deps_digest_for_glb(
                    &bytes,
                    &c.file,
                    &content_by_file,
                    toggles.magenta_missing,
                )
                .map_err(|e| eprintln!("skip {ent_id}/{}: deps digest: {e:#}", c.file))
                .ok()
            });
            match digest {
                Some(d) => format!("{case_hash}_{d}_{platform}"),
                None => continue,
            }
        } else {
            format!("{case_hash}_{platform}")
        };
        if !local_seen.insert(bundle_name.clone()) {
            continue;
        }
        let m_deps = if is_glb {
            scan.metadata_deps(store, &c.file, &c.hash, &content_by_file, platform)
        } else {
            Vec::new()
        };
        let model_ref = is_image && model_refs.contains(&c.hash);
        let standalone_color_space = if is_image {
            Some(if linear_refs.contains(&c.hash) { 0 } else { 1 })
        } else {
            None
        };
        let standalone_normal = is_image && normal_refs.contains(&c.hash);
        bundles.push(BundleSpec {
            cid: c.hash.clone(),
            bundle_name,
            source_file: Some(c.file.clone()),
            entity_type: entity_type.clone(),
            metadata_deps: m_deps,
            model_referenced: model_ref,
            expect_hash: None,
            standalone_color_space,
            standalone_normal,
            force_default_material: false,
        });
    }
    if bundles.is_empty() {
        return None;
    }
    Some(EntityEntry {
        entity_id: ent_id.to_string(),
        content: content_items,
        bundles,
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_fused_entity_ids(
    ids: &[String],
    store: &LocalContentStore,
    out_root: &Path,
    platforms: &[String],
    toggles: EffectiveToggles,
    ab_version: &str,
    content_server_url: &str,
    skip_existing: bool,
    force: bool,
) -> BuildOutcome {
    let uri_cache = abgen::glbscan::UriCache::new();
    let first_written: Vec<FirstWritten> = platforms
        .iter()
        .map(|_| Mutex::new(HashMap::new()))
        .collect();
    let built = AtomicUsize::new(0);
    let errs = AtomicUsize::new(0);
    let skipped = AtomicUsize::new(0);
    let manifest_errs = AtomicUsize::new(0);
    let manifest_incomplete = AtomicUsize::new(0);
    let processed = AtomicUsize::new(0);
    let build_date = abgen::live::build_scoped_date();
    let n_total = ids.len();
    let t0 = Instant::now();
    let counters = BuildCounters {
        built: &built,
        errs: &errs,
        skipped: &skipped,
    };
    let fw_refs: Vec<Option<&FirstWritten>> = first_written.iter().map(Some).collect();
    let last_print_ms = std::sync::atomic::AtomicU64::new(0);
    let heartbeat = || {
        let elapsed_ms = t0.elapsed().as_millis() as u64;
        let last = last_print_ms.load(Ordering::Relaxed);
        if elapsed_ms.saturating_sub(last) >= 2000
            && last_print_ms
                .compare_exchange(last, elapsed_ms, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            let b = built.load(Ordering::Relaxed);
            let s = skipped.load(Ordering::Relaxed);
            let e = errs.load(Ordering::Relaxed);
            eprintln!(
                "  build: {} bundles done (built={b} skipped={s} errs={e}, {:.0}s)",
                b + s + e,
                t0.elapsed().as_secs_f64()
            );
        }
    };
    let primary = &platforms[0];

    ids.par_iter().for_each(|ent_id| {
        let done = processed.fetch_add(1, Ordering::Relaxed) + 1;
        if done.is_multiple_of(5000) {
            let secs = t0.elapsed().as_secs_f64().max(0.001);
            eprintln!(
                "  fused: {done}/{n_total} entities ({:.0}/s, {:.0}s) | built={} skipped={} errs={}",
                done as f64 / secs,
                secs,
                built.load(Ordering::Relaxed),
                skipped.load(Ordering::Relaxed),
                errs.load(Ordering::Relaxed),
            );
        }
        let entry = match derive_one_entity(store, ent_id, primary, &uri_cache, toggles) {
            Some(e) => e,
            None => return,
        };
        let EntityEntry {
            entity_id,
            content,
            bundles,
        } = entry;
        let content_by_file: HashMap<String, String> = content
            .iter()
            .map(|c| (c.file.to_lowercase(), c.hash.clone()))
            .collect();
        let mut plat_bundles: Vec<Vec<BundleSpec>> = Vec::with_capacity(platforms.len());
        plat_bundles.push(bundles);
        for plat in &platforms[1..] {
            plat_bundles.push(
                plat_bundles[0]
                    .iter()
                    .map(|s| sibling_spec(s, primary, plat))
                    .collect(),
            );
        }
        let ent_outs: Vec<PathBuf> = platforms
            .iter()
            .map(|p| out_root.join(&entity_id).join(p))
            .collect();
        for ent_out in &ent_outs {
            if let Err(e) = std::fs::create_dir_all(ent_out) {
                eprintln!("mkdir {}: {e}", ent_out.display());
                errs.fetch_add(
                    plat_bundles.iter().map(|b| b.len()).sum::<usize>(),
                    Ordering::Relaxed,
                );
                return;
            }
        }
        let n_bundles = plat_bundles[0].len();
        (0..n_bundles).into_par_iter().for_each(|bi| {
            if platforms.len() == 1 {
                build_bundle_at(
                    store,
                    &content_by_file,
                    &plat_bundles[0][bi],
                    &ent_outs[0],
                    &entity_id,
                    toggles,
                    skip_existing,
                    force,
                    Some(&first_written[0]),
                    &counters,
                );
            } else {
                let specs: Vec<&BundleSpec> = plat_bundles.iter().map(|l| &l[bi]).collect();
                build_bundle_multi_at(
                    store,
                    &content_by_file,
                    &specs,
                    &ent_outs,
                    &entity_id,
                    toggles,
                    skip_existing,
                    force,
                    &fw_refs,
                    &counters,
                );
            }
            heartbeat();
        });
        for (pi, plat) in platforms.iter().enumerate() {
            match write_cdn_manifest(
                out_root,
                &entity_id,
                plat,
                &plat_bundles[pi],
                ab_version,
                content_server_url,
                &build_date,
            ) {
                Err(e) => {
                    manifest_errs.fetch_add(1, Ordering::Relaxed);
                    eprintln!("manifest {entity_id}: {e}");
                }
                Ok(0) => {}
                Ok(n) => {
                    manifest_incomplete.fetch_add(1, Ordering::Relaxed);
                    eprintln!("manifest {entity_id}: {n} failed bundle(s) omitted");
                }
            }
        }
    });

    let mut reconcile = ReconcileStats::default();
    for (fw, plat) in first_written.into_iter().zip(platforms.iter()) {
        let rs = reconcile_divergent(store, out_root, plat, toggles, fw);
        if platforms.len() > 1 {
            eprintln!(
                "reconcile[{plat}]: divergent={} rebuilt={} relinked={} errs={}",
                rs.divergent, rs.rebuilt, rs.relinked, rs.errs
            );
        }
        reconcile.divergent += rs.divergent;
        reconcile.rebuilt += rs.rebuilt;
        reconcile.relinked += rs.relinked;
        reconcile.errs += rs.errs;
    }

    BuildOutcome {
        built: built.into_inner(),
        errs: errs.into_inner(),
        skipped: skipped.into_inner(),
        manifest_errs: manifest_errs.into_inner(),
        manifest_incomplete: manifest_incomplete.into_inner(),
        reconcile,
    }
}

pub(crate) const IMAGE_EXTS: [&str; 3] = [".png", ".jpg", ".jpeg"];

pub(crate) fn load_entity_json(store: &LocalContentStore, cid: &str) -> Option<serde_json::Value> {
    let bytes = store.fetch(cid).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toggles(asset_reuse: bool, magenta_missing: bool) -> EffectiveToggles {
        EffectiveToggles {
            collection_mode: false,
            real_textures: false,
            v38_compat: false,
            v38_timestamp: 0,
            magenta_missing,
            asset_reuse,
        }
    }

    fn store_with_entity(tag: &str, content: serde_json::Value) -> LocalContentStore {
        let dir =
            std::env::temp_dir().join(format!("abgen-corpus-derive-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = LocalContentStore::new(&dir);
        let entity = serde_json::json!({"type": "scene", "content": content});
        store
            .write("bafyentity", entity.to_string().as_bytes())
            .unwrap();
        store
    }

    fn glb_names(entry: &EntityEntry) -> Vec<String> {
        entry
            .bundles
            .iter()
            .filter(|b| b.cid == "Qmglb")
            .map(|b| b.bundle_name.clone())
            .collect()
    }

    const GLTF_JSON: &str = r#"{"asset":{"version":"2.0"},
        "images":[{"uri":"t.png"}],"buffers":[{"uri":"a.bin"}]}"#;

    #[test]
    fn derive_names_glbs_canonically_in_asset_reuse_mode() {
        let store = store_with_entity(
            "canonical",
            serde_json::json!([
                {"file": "m.gltf", "hash": "Qmglb"},
                {"file": "a.bin", "hash": "Qmbin"},
                {"file": "t.png", "hash": "Qmtex"},
            ]),
        );
        store.write("Qmglb", GLTF_JSON.as_bytes()).unwrap();
        store.write("Qmbin", b"BIN").unwrap();
        store.write("Qmtex", b"PNG").unwrap();
        let cache = abgen::glbscan::UriCache::new();

        let legacy = derive_one_entity(
            &store,
            "bafyentity",
            "windows",
            &cache,
            toggles(false, false),
        )
        .unwrap();
        assert_eq!(glb_names(&legacy), vec!["Qmglb_windows".to_string()]);

        let reuse = derive_one_entity(
            &store,
            "bafyentity",
            "windows",
            &cache,
            toggles(true, false),
        )
        .unwrap();
        let digest = abgen::naming::compute_deps_digest(&[
            ("a.bin".to_string(), "Qmbin".to_string()),
            ("t.png".to_string(), "Qmtex".to_string()),
        ]);
        assert_eq!(glb_names(&reuse), vec![format!("Qmglb_{digest}_windows")]);
        assert!(reuse
            .bundles
            .iter()
            .any(|b| b.bundle_name == "Qmtex_windows"));
    }

    #[test]
    fn derive_skips_glb_with_missing_dep_unless_magenta_tolerant() {
        let store = store_with_entity(
            "missing-dep",
            serde_json::json!([
                {"file": "m.gltf", "hash": "Qmglb"},
                {"file": "t.png", "hash": "Qmtex"},
            ]),
        );
        store.write("Qmglb", GLTF_JSON.as_bytes()).unwrap();
        store.write("Qmtex", b"PNG").unwrap();
        let cache = abgen::glbscan::UriCache::new();

        let strict = derive_one_entity(
            &store,
            "bafyentity",
            "windows",
            &cache,
            toggles(true, false),
        )
        .unwrap();
        assert!(glb_names(&strict).is_empty());
        assert!(strict
            .bundles
            .iter()
            .any(|b| b.bundle_name == "Qmtex_windows"));

        let tolerant =
            derive_one_entity(&store, "bafyentity", "windows", &cache, toggles(true, true))
                .unwrap();
        let digest =
            abgen::naming::compute_deps_digest(&[("t.png".to_string(), "Qmtex".to_string())]);
        assert_eq!(
            glb_names(&tolerant),
            vec![format!("Qmglb_{digest}_windows")]
        );
    }
}

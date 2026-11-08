//! `--bevy-variants`: build only the Bevy variants lane (prototype) for the selected entities,
//! into `<out-dir>` in the served layout (`bevy/manifest/{entity}.json`,
//! `bevy/contents/{variantCid}`), so `ABGEN_OUT_ROOT=<out-dir> abgen` serves it as is.
//!
//! At the end it prints a JSON summary per entity (stage times summed over files, counts and
//! bytes by format and source, no-variant reasons) and writes the full stats, per-file rows
//! included, to `<out-dir>/bevy-variants-stats.json` (outside `bevy/`, so it is never served).

use abgen::local_store::LocalContentStore;
use abgen::Result;
#[cfg(feature = "bevy-variants")]
use anyhow::Context;
use std::path::Path;

#[cfg(feature = "bevy-variants")]
pub(crate) fn run_bevy_variants(
    ids: &[String],
    store: &LocalContentStore,
    out_root: &Path,
    glb_variants: bool,
    zstd_level: Option<i32>,
) -> Result<()> {
    let env = abgen::bevyvariants::BuildOptions::from_env();
    let opts = abgen::bevyvariants::BuildOptions {
        glb_variants: glb_variants || env.glb_variants,
        zstd_level: match zstd_level {
            Some(l) => abgen::ktx2_write::check_zstd_level(l)?,
            None => env.zstd_level,
        },
    };
    let started = std::time::Instant::now();
    let mut all_stats: Vec<serde_json::Value> = Vec::new();
    std::fs::create_dir_all(out_root)
        .with_context(|| format!("mkdir {}", out_root.display()))?;
    let mut errs = 0usize;
    let mut originals = 0usize;
    let mut variants = 0usize;
    for id in ids {
        let built = (|| -> Result<abgen::bevyvariants::BuildOutput> {
            let raw = store.fetch(id).with_context(|| format!("entity {id} not in the store"))?;
            let mut ent: serde_json::Value =
                serde_json::from_slice(&raw).with_context(|| format!("parse entity {id}"))?;
            // The stored entity file is the deployed JSON, addressed by its hash: it carries no
            // `id` of its own, so the id it is stored under is the entity id.
            if let Some(obj) = ent.as_object_mut() {
                obj.entry("id")
                    .or_insert_with(|| serde_json::Value::String(id.clone()));
            }
            let scene = abgen::catalyst::CatalystClient::parse_entity(&ent)?;
            let fetch = |hash: &str| store.fetch(hash).ok();
            abgen::bevyvariants::build_entity_variants(&scene, &fetch, out_root, opts)
        })();
        match built {
            Ok(o) => {
                let mut summary = o.stats.summary_json();
                if let Some(obj) = summary.as_object_mut() {
                    obj.insert("entity".into(), serde_json::Value::String(id.clone()));
                }
                println!("BEVY_STATS {summary}");
                let mut full = serde_json::to_value(&o.stats)?;
                if let Some(obj) = full.as_object_mut() {
                    obj.insert("entity".into(), serde_json::Value::String(id.clone()));
                    obj.insert("summary".into(), summary);
                }
                all_stats.push(full);
                originals += o.manifest.files.len();
                variants += o.variants.len();
                eprintln!(
                    "bevy: {id}: {} originals, {} variants -> {}",
                    o.manifest.files.len(),
                    o.variants.len(),
                    abgen::bevyvariants::manifest_path(out_root, id)
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                );
            }
            Err(e) => {
                errs += 1;
                eprintln!("bevy: {id}: {e:#}");
            }
        }
    }
    let stats_path = out_root.join("bevy-variants-stats.json");
    let doc = serde_json::json!({
        "zstd_level": opts.zstd_level,
        "libzstd": abgen::ktx2_write::LIBZSTD_VERSION,
        "threads": rayon::current_num_threads(),
        "wall_ms": started.elapsed().as_millis() as u64,
        "entities": all_stats,
    });
    std::fs::write(&stats_path, serde_json::to_vec_pretty(&doc)?)
        .with_context(|| format!("write {}", stats_path.display()))?;
    println!(
        "DONE bevy-variants entities={} originals={originals} variants={variants} errs={errs} zstd_level={} wall_ms={} stats={}",
        ids.len(),
        opts.zstd_level,
        started.elapsed().as_millis(),
        stats_path.display()
    );
    if errs > 0 {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(not(feature = "bevy-variants"))]
pub(crate) fn run_bevy_variants(
    _: &[String],
    _: &LocalContentStore,
    _: &Path,
    _: bool,
    _: Option<i32>,
) -> Result<()> {
    anyhow::bail!("--bevy-variants requires the bevy-variants feature (on by default via `server`)")
}

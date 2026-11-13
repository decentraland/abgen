//! `--bevy-variants`: build only the Bevy variants lane (prototype) for the selected entities,
//! into `<out-dir>` in the served layout (`bevy/manifest/{entity}.json`,
//! `bevy/{prefix}/{name}` with the `.json` record beside each), so
//! `ABGEN_OUT_ROOT=<out-dir> abgen` serves it as is. A variant already under `<out-dir>` (an
//! earlier run, another entity sharing the file) is reused from its record, not re-encoded.
//!
//! At the end it prints a JSON summary per entity (stage times summed over files, counts and
//! bytes by format and source, no-variant reasons) and writes the full stats, per-file rows
//! included, to `<out-dir>/bevy-variants-stats.json` (outside `bevy/`, so it is never served).

use abgen::local_store::LocalContentStore;
use abgen::Result;
#[cfg(feature = "bevy-variants")]
use anyhow::Context;
use std::path::Path;

/// The build options from the environment's (`BuildOptions::from_env`) and the flags: an
/// explicit flag wins over its env var; `--bevy-no-models` can only turn models off.
#[cfg(feature = "bevy-variants")]
fn bevy_options(
    env: abgen::bevyvariants::BuildOptions,
    glb_variants: Option<bool>,
    no_models: bool,
    precompress_br: Option<bool>,
    zstd_level: Option<i32>,
) -> Result<abgen::bevyvariants::BuildOptions> {
    Ok(abgen::bevyvariants::BuildOptions {
        // An explicit `--bevy-[no-]glb-variants` wins over `ABGEN_BEVY_GLB_VARIANTS`.
        glb_variants: glb_variants.unwrap_or(env.glb_variants),
        models: env.models && !no_models,
        // An explicit `--bevy-precompress` wins over `ABGEN_BEVY_PRECOMPRESS`.
        precompress_br: precompress_br.unwrap_or(env.precompress_br),
        zstd_level: match zstd_level {
            Some(l) => abgen::ktx2_write::check_zstd_level(l)?,
            None => env.zstd_level,
        },
        epoch: env.epoch,
    })
}

#[cfg(feature = "bevy-variants")]
pub(crate) fn run_bevy_variants(
    ids: &[String],
    store: &LocalContentStore,
    out_root: &Path,
    glb_variants: Option<bool>,
    no_models: bool,
    precompress_br: Option<bool>,
    zstd_level: Option<i32>,
) -> Result<()> {
    let opts = bevy_options(
        abgen::bevyvariants::BuildOptions::from_env(),
        glb_variants,
        no_models,
        precompress_br,
        zstd_level,
    )?;
    let started = std::time::Instant::now();
    let mut all_stats: Vec<serde_json::Value> = Vec::new();
    std::fs::create_dir_all(out_root).with_context(|| format!("mkdir {}", out_root.display()))?;
    let mut errs = 0usize;
    let mut originals = 0usize;
    let mut variants = 0usize;
    let mut reused = 0u64;
    let mut glbs = abgen::bevyvariants::stats::GlbSummary::default();
    // Entities with a file that could not be fetched or built: no manifest, so not done.
    let mut incomplete = 0usize;
    for id in ids {
        let built = (|| -> Result<abgen::bevyvariants::BuildOutput> {
            let raw = store
                .fetch(id)
                .with_context(|| format!("entity {id} not in the store"))?;
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
            // No store to ask: records under <out-dir> are the only reuse.
            let probe = |_: &str| None;
            let stored = |_: &str| None;
            let lane = abgen::bevyvariants::Lane {
                fetch: &fetch,
                probe: &probe,
                stored: &stored,
                local_records: true,
            };
            abgen::bevyvariants::build_entity_variants(&scene, &lane, out_root, opts)
        })();
        match built {
            Ok(o) => {
                let mut summary = o.stats.summary_json();
                if let Some(obj) = summary.as_object_mut() {
                    obj.insert("entity".into(), serde_json::Value::String(id.clone()));
                    if !o.incomplete.is_empty() {
                        obj.insert("incomplete".into(), serde_json::json!(o.incomplete));
                    }
                }
                println!("BEVY_STATS {summary}");
                let mut full = serde_json::to_value(&o.stats)?;
                if let Some(obj) = full.as_object_mut() {
                    obj.insert("entity".into(), serde_json::Value::String(id.clone()));
                    obj.insert("summary".into(), summary);
                }
                all_stats.push(full);
                originals += o.entries.len();
                variants += o.stats.unique_variants as usize;
                reused +=
                    o.stats.reused_textures + o.stats.reused_models + o.stats.glb_variants.reused;
                let g = &o.stats.glb_variants;
                glbs.considered += g.considered;
                glbs.variants += g.variants;
                glbs.variants_original_bytes += g.variants_original_bytes;
                glbs.variant_bytes += g.variant_bytes;
                glbs.br_count += g.br_count;
                glbs.br_bytes += g.br_bytes;
                glbs.gpu_original_bytes += g.gpu_original_bytes;
                glbs.gpu_variant_bytes += g.gpu_variant_bytes;
                if !o.incomplete.is_empty() {
                    incomplete += 1;
                    errs += 1;
                    eprintln!(
                        "bevy: {id}: {} files could not be fetched or built ({}); manifest not written",
                        o.incomplete.len(),
                        o.incomplete.join(", ")
                    );
                    continue;
                }
                eprintln!(
                    "bevy: {id}: {} originals, {} variants built, {} reused -> {}",
                    o.entries.len(),
                    o.stats.unique_variants,
                    o.stats.reused_textures + o.stats.reused_models,
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
        "epoch": opts.epoch,
        "models": opts.models,
        "glb_variants": opts.glb_variants,
        "precompress": if opts.precompress_br { "br" } else { "none" },
        "libzstd": abgen::ktx2_write::LIBZSTD_VERSION,
        "threads": rayon::current_num_threads(),
        "wall_ms": started.elapsed().as_millis() as u64,
        "entities": all_stats,
    });
    std::fs::write(&stats_path, serde_json::to_vec_pretty(&doc)?)
        .with_context(|| format!("write {}", stats_path.display()))?;
    println!(
        "DONE bevy-variants entities={} originals={originals} variants={variants} reused={reused} incomplete={incomplete} errs={errs} zstd_level={} wall_ms={} stats={}",
        ids.len(),
        opts.zstd_level,
        started.elapsed().as_millis(),
        stats_path.display()
    );
    if opts.glb_variants {
        let mb = |b: u64| (b as f64 / 1e5).round() / 10.0;
        println!(
            "DONE bevy-glb models={} variants={} download_mb={}->{} (br {} of them: {}) embedded_gpu_mb={}->{}",
            glbs.considered,
            glbs.variants,
            mb(glbs.variants_original_bytes),
            mb(glbs.variant_bytes),
            glbs.br_count,
            mb(glbs.br_bytes),
            mb(glbs.gpu_original_bytes),
            mb(glbs.gpu_variant_bytes),
        );
    }
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
    _: Option<bool>,
    _: bool,
    _: Option<bool>,
    _: Option<i32>,
) -> Result<()> {
    anyhow::bail!("--bevy-variants requires the bevy-variants feature (on by default via `server`)")
}

#[cfg(all(test, feature = "bevy-variants"))]
mod tests {
    use super::bevy_options;
    use abgen::bevyvariants::BuildOptions;

    #[test]
    fn flags_win_over_the_environment() {
        let on = BuildOptions::default();
        let off = BuildOptions {
            glb_variants: false,
            models: false,
            precompress_br: false,
            zstd_level: 9,
            epoch: Some(3),
        };
        // No flags: the environment's options as they are.
        assert_eq!(bevy_options(on, None, false, None, None).unwrap(), on);
        assert_eq!(bevy_options(off, None, false, None, None).unwrap(), off);
        // `--bevy-glb-variants` / `--bevy-no-glb-variants` win over ABGEN_BEVY_GLB_VARIANTS.
        assert!(
            bevy_options(off, Some(true), false, None, None)
                .unwrap()
                .glb_variants
        );
        assert!(
            !bevy_options(on, Some(false), false, None, None)
                .unwrap()
                .glb_variants
        );
        // `--bevy-precompress br|none` wins over ABGEN_BEVY_PRECOMPRESS.
        assert!(
            bevy_options(off, None, false, Some(true), None)
                .unwrap()
                .precompress_br
        );
        assert!(
            !bevy_options(on, None, false, Some(false), None)
                .unwrap()
                .precompress_br
        );
        // `--bevy-no-models` only turns models off; ABGEN_BEVY_MODELS=0 stays off.
        assert!(!bevy_options(on, None, true, None, None).unwrap().models);
        assert!(!bevy_options(off, None, false, None, None).unwrap().models);
        // `--bevy-zstd-level` wins and is checked; the epoch is the environment's alone.
        let o = bevy_options(off, None, false, None, Some(19)).unwrap();
        assert_eq!((o.zstd_level, o.epoch), (19, Some(3)));
        assert!(bevy_options(on, None, false, None, Some(0)).is_err());
    }
}

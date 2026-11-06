//! `--bevy-variants`: build only the Bevy variants lane (prototype) for the selected entities,
//! into `<out-dir>` in the served layout (`bevy/manifest/{entity}.json`,
//! `bevy/contents/{variantCid}`), so `ABGEN_OUT_ROOT=<out-dir> abgen` serves it as is.

use abgen::local_store::LocalContentStore;
use abgen::Result;
#[cfg(feature = "bevy-variants")]
use anyhow::Context;
use std::path::Path;

#[cfg(feature = "bevy-variants")]
pub(crate) fn run_bevy_variants(ids: &[String], store: &LocalContentStore, out_root: &Path) -> Result<()> {
    std::fs::create_dir_all(out_root)
        .with_context(|| format!("mkdir {}", out_root.display()))?;
    let mut errs = 0usize;
    let mut originals = 0usize;
    let mut variants = 0usize;
    for id in ids {
        let built = (|| -> Result<abgen::bevyvariants::BuildOutput> {
            let raw = store.fetch(id).with_context(|| format!("entity {id} not in the store"))?;
            let ent: serde_json::Value =
                serde_json::from_slice(&raw).with_context(|| format!("parse entity {id}"))?;
            let scene = abgen::catalyst::CatalystClient::parse_entity(&ent)?;
            let fetch = |hash: &str| store.fetch(hash).ok();
            abgen::bevyvariants::build_entity_variants(&scene, &fetch, out_root)
        })();
        match built {
            Ok(o) => {
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
    println!(
        "DONE bevy-variants entities={} originals={originals} variants={variants} errs={errs}",
        ids.len()
    );
    if errs > 0 {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(not(feature = "bevy-variants"))]
pub(crate) fn run_bevy_variants(_: &[String], _: &LocalContentStore, _: &Path) -> Result<()> {
    anyhow::bail!("--bevy-variants requires the bevy-variants feature (on by default via `server`)")
}

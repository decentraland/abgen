//! The Bevy variants lane as a step of a conversion job: for the job's entity, after its asset
//! bundles, build the GPU-ready variants and their manifest against the same store
//! (`abgen::live::Proxy::build_bevy_variants`): skip an entity whose manifest in the store is
//! current, probe each variant in Redis and the store, encode only the misses, upload them
//! and write the manifest last.
//!
//! It is its own lane, not a member of `PLATFORMS`: it publishes no bundle manifest and sends
//! no conversion-finished event, so the registry never hears of a `bevy` platform.

use crate::config::Config;
use abgen::live::Proxy;
use std::sync::Arc;

/// `None` when the lane is off (`ABGEN_BEVY_VARIANTS=0`). Otherwise its summary, for the
/// conversion summary's `bevy` key: `{"skipped": "current-manifest"}`, the build's counts, or
/// `{"error": …}`. A failure (or a panic in an encoder) is logged and reported there and never
/// fails the job, whose bundles are already published and notified; the entity's next
/// conversion is the next attempt.
pub fn follow_up(
    cfg: &Config,
    proxy: &Arc<Proxy>,
    entity_id: &str,
    force: bool,
) -> Option<serde_json::Value> {
    if !cfg.bevy_enabled {
        return None;
    }
    let started = std::time::Instant::now();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        proxy.build_bevy_variants(&cfg.out_root, entity_id, force)
    }))
    .unwrap_or_else(|_| Err(anyhow::anyhow!("bevy variants build panicked")));
    // Everything that matters is in the store: nothing under `bevy/` outlives the job, so a
    // warm container never reuses a local record instead of probing the store.
    if !cfg.keep_output {
        let _ = std::fs::remove_dir_all(cfg.out_root.join(abgen::bevyvariants::BEVY_PLATFORM));
    }
    let summary = summary(&result);
    let outcome = if summary.get("error").is_some() {
        "error"
    } else if summary.get("skipped").is_some() {
        "skipped"
    } else {
        "built"
    };
    if let Err(e) = &result {
        eprintln!("bevy: {entity_id}: variants build failed ({e:#}); the conversion stands");
    } else {
        eprintln!(
            "bevy: {entity_id}: {summary} in {:.1}s",
            started.elapsed().as_secs_f64()
        );
    }
    metrics::counter!("abgen_lambda_bevy_followup_total", "outcome" => outcome).increment(1);
    metrics::histogram!("abgen_lambda_bevy_duration_seconds", "outcome" => outcome)
        .record(started.elapsed().as_secs_f64());
    Some(summary)
}

fn summary(result: &anyhow::Result<abgen::bevyvariants::BevyBuild>) -> serde_json::Value {
    match result {
        Err(e) => serde_json::json!({ "error": format!("{e:#}") }),
        Ok(b) if b.skipped => serde_json::json!({ "skipped": "current-manifest" }),
        Ok(b) => serde_json::json!({
            "originals": b.originals,
            "reused": b.reused,
            "built": b.built,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_names_a_skip_the_counts_or_the_error() {
        use abgen::bevyvariants::BevyBuild;
        assert_eq!(
            summary(&Ok(BevyBuild {
                skipped: true,
                ..Default::default()
            })),
            serde_json::json!({ "skipped": "current-manifest" })
        );
        assert_eq!(
            summary(&Ok(BevyBuild {
                skipped: false,
                originals: 3,
                reused: 2,
                built: 1,
            })),
            serde_json::json!({ "originals": 3, "reused": 2, "built": 1 })
        );
        let err = summary(&Err(anyhow::anyhow!("uploads failed")));
        assert_eq!(err["error"], "uploads failed");
    }
}

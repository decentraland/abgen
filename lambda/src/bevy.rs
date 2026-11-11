//! The Bevy variants lane as a step of a conversion job: for the job's entity, after its asset
//! bundles, build the GPU-ready variants and their manifest against the same store
//! (`abgen::live::Proxy::build_bevy_variants`): skip an entity whose manifest in the store is
//! current, probe each variant in Redis and the store, encode only the misses, upload them
//! and write the manifest last.
//!
//! It is its own lane, not a member of `PLATFORMS`: it publishes no bundle manifest and sends
//! no conversion-finished event, so the registry never hears of a `bevy` platform.

use crate::config::Config;
use abgen::bevyvariants::BevyBuild;
use abgen::live::Proxy;
use std::sync::Arc;

fn clear_output(cfg: &Config) {
    let _ = std::fs::remove_dir_all(cfg.out_root.join(abgen::bevyvariants::BEVY_PLATFORM));
}

/// `None` when the lane is off (`ABGEN_BEVY_VARIANTS=0`). Otherwise its summary, for the
/// conversion summary's `bevy` key: `{"skipped": "current-manifest"}`, the build's counts,
/// `{"skipped": "incomplete", "missing": N, …counts}` when files could not be fetched or built
/// (what did build is uploaded, the manifest is not), or `{"error": …}`. A failure (or a panic
/// in an encoder) is logged and reported there and never fails the job, whose bundles are
/// already published and notified; the entity's next conversion is the next attempt.
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
    // The store is the only record of what exists. A warm container keeps `/tmp`, and a run
    // that was killed never reached its cleanup: start from an empty directory.
    clear_output(cfg);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        proxy.build_bevy_variants(&cfg.out_root, entity_id, force)
    }))
    .unwrap_or_else(|_| Err(anyhow::anyhow!("bevy variants build panicked")));
    if !cfg.keep_output {
        clear_output(cfg);
    }
    let summary = summary(&result);
    let outcome = match &result {
        Err(_) => "error",
        Ok(b) if b.skipped => "skipped",
        Ok(b) if b.incomplete > 0 => "incomplete",
        Ok(_) => "built",
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

fn summary(result: &anyhow::Result<BevyBuild>) -> serde_json::Value {
    match result {
        Err(e) => serde_json::json!({ "error": format!("{e:#}") }),
        Ok(b) if b.skipped => serde_json::json!({ "skipped": "current-manifest" }),
        Ok(b) if b.incomplete > 0 => serde_json::json!({
            "skipped": "incomplete",
            "missing": b.incomplete,
            "originals": b.originals,
            "reused": b.reused,
            "built": b.built,
        }),
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
    fn summary_names_a_skip_the_counts_an_incomplete_build_or_the_error() {
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
                incomplete: 0,
            })),
            serde_json::json!({ "originals": 3, "reused": 2, "built": 1 })
        );
        // Files that could not be fetched or built: no manifest, reported, not an error.
        assert_eq!(
            summary(&Ok(BevyBuild {
                skipped: false,
                originals: 3,
                reused: 2,
                built: 1,
                incomplete: 2,
            })),
            serde_json::json!({
                "skipped": "incomplete",
                "missing": 2,
                "originals": 3,
                "reused": 2,
                "built": 1,
            })
        );
        let err = summary(&Err(anyhow::anyhow!("uploads failed")));
        assert_eq!(err["error"], "uploads failed");
    }
}

//! Publishing conversion output to the CDN bucket.
//!
//! TODO(step 3): upload to S3 with the same keys, content types and headers
//! the Node consumer-server used:
//!   * bundles:  `{version}/{bundleName}`
//!   * manifest: `manifest/{entityId}_{platform}.json`
//! then remove the local corpus dir. Signing will be hand-rolled SigV4 over
//! ureq (credentials from the Lambda execution-role env), keeping the binary
//! async-free.
//!
//! Deliberately NOT uploaded: the `.br` brotli siblings prod stores next to
//! every file. They exist for legacy web clients; this pipeline's only
//! consumer is unity-explorer, which fetches raw names (verified 2026-08:
//! no `.br` fetches in unity-explorer or aang-renderer). Edge compression
//! can cover a future web consumer without re-adding them.
//!
//! Until then this stub reports what it *would* upload, and `--once` runs
//! leave the corpus under OUT_ROOT for inspection.

use crate::config::Config;
use crate::convert::EntityOutcome;
use anyhow::Result;

pub fn publish(cfg: &Config, outcome: &EntityOutcome) -> Result<serde_json::Value> {
    let total_files: usize = outcome.platforms.iter().map(|p| p.built.len()).sum();
    match &cfg.s3_bucket {
        Some(bucket) => {
            eprintln!(
                "output: TODO(step 3) would upload {total_files} bundle(s) + {} manifest(s) \
                 for {} to s3://{bucket}/{}/… — corpus left at {}",
                outcome.platforms.len(),
                outcome.entity_id,
                cfg.version,
                cfg.out_root.display(),
            );
            Ok(serde_json::json!({ "uploaded": false, "pendingStep": 3, "bucket": bucket }))
        }
        None => {
            eprintln!(
                "output: no S3_BUCKET configured — corpus left at {} ({} file(s))",
                cfg.out_root.display(),
                total_files,
            );
            Ok(serde_json::json!({ "uploaded": false, "local": cfg.out_root.display().to_string() }))
        }
    }
}

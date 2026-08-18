//! Publishing conversion output to the CDN bucket.
//!
//! TODO(step 3): brotli-compress and upload to S3 with the same keys,
//! content types and headers the Node consumer-server used:
//!   * bundles:  `{version}/{bundleName}` (+ `.br` siblings)
//!   * manifest: `manifest/{entityId}_{platform}.json`
//! then remove the local corpus dir. Signing will be hand-rolled SigV4 over
//! ureq (credentials from the Lambda execution-role env), keeping the binary
//! async-free.
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

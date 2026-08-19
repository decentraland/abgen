//! Notifying the asset-bundle-registry that a conversion finished.
//!
//! TODO(step 4): send one `AssetBundleConversionFinishedEvent` per platform
//! directly to the registry's SQS queue (`REGISTRY_QUEUE_URL`) via SigV4-signed
//! `SendMessage` — the registry's subscriptions use raw message delivery, so
//! the queue body is exactly the event JSON, no SNS envelope. The event shape
//! will be lifted verbatim from `@dcl/schemas` / the registry's handler
//! validation when this step lands.

use crate::config::Config;
use crate::convert::EntityOutcome;
use anyhow::Result;

pub fn send(cfg: &Config, outcome: &EntityOutcome) -> Result<bool> {
    match &cfg.registry_queue_url {
        Some(queue) => {
            eprintln!(
                "notify: TODO(step 4) would send {} finished-event(s) for {} to {queue}",
                outcome.platforms.len(),
                outcome.entity_id,
            );
            Ok(false)
        }
        None => {
            eprintln!("notify: no REGISTRY_QUEUE_URL configured — skipping");
            Ok(false)
        }
    }
}

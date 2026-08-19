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

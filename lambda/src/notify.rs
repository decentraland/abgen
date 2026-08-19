use crate::config::Config;
use anyhow::{Context, Result};

/// Prod's triage-fast-path status for already-converted entities.
pub const STATUS_ALREADY_CONVERTED: i32 = 13;

pub struct Finished<'a> {
    pub platform: &'a str,
    pub status_code: i32,
}

/// One `AssetBundleConversionFinishedEvent` per platform, byte-compatible
/// with what consumer-server publishes (adapters/sns.ts). Must target a
/// DEDICATED topic, never the shared event-driven-sns bus — the prod
/// registry's filter matches every `asset-bundle` event. Errors propagate so
/// SQS redelivers; the skip path also notifies, so a failed publish is
/// re-emitted on redelivery.
pub fn send_finished(
    cfg: &Config,
    entity_id: &str,
    content_server: &str,
    finished: &[Finished],
) -> Result<bool> {
    let Some(sns) = abgen::sns::Sns::global() else {
        eprintln!(
            "notify: ABGEN_SNS_TOPIC_ARN not set — skipping {} finished event(s) for {entity_id}",
            finished.len()
        );
        return Ok(false);
    };
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let is_world = content_server.contains("worlds-content-server");
    for f in finished {
        let event = serde_json::json!({
            "type": "asset-bundle",
            "subType": "converted",
            "key": format!("{entity_id}-{}", f.platform),
            "timestamp": timestamp,
            "metadata": {
                "platform": f.platform,
                "entityId": entity_id,
                "isLods": false,
                "isWorld": is_world,
                "statusCode": f.status_code,
                "version": cfg.version,
            },
        });
        sns.publish(
            &event.to_string(),
            &[("type", "asset-bundle"), ("subType", "converted")],
        )
        .with_context(|| format!("publish finished event for {entity_id} {}", f.platform))?;
    }
    eprintln!(
        "notify: published {} finished event(s) for {entity_id}",
        finished.len()
    );
    Ok(true)
}

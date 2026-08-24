use crate::config::Config;
use anyhow::{Context, Result};

/// Prod's triage-fast-path status for already-converted entities.
pub const STATUS_ALREADY_CONVERTED: i32 = 13;

/// Prod's UNEXPECTED_ERROR exit code — carried by failure tombstones.
pub const STATUS_UNEXPECTED_ERROR: i32 = 5;

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
    is_lods: bool,
    finished: &[Finished],
) -> Result<bool> {
    let Some(sns) = abgen::sns::Sns::global() else {
        // Sns::global() is also None when the ARN is set but AWS credential
        // resolution failed — say which, or an outage reads as "not configured".
        let arn_set = std::env::var("ABGEN_SNS_TOPIC_ARN").is_ok_and(|v| !v.is_empty());
        if arn_set {
            eprintln!(
                "notify: ABGEN_SNS_TOPIC_ARN is set but AWS credentials did not resolve — \
                 skipping {} finished event(s) for {entity_id} (events are lost)",
                finished.len()
            );
        } else {
            eprintln!(
                "notify: ABGEN_SNS_TOPIC_ARN not set — skipping {} finished event(s) for {entity_id}",
                finished.len()
            );
        }
        return Ok(false);
    };
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let is_world = content_server.contains("worlds-content-server");
    for f in finished {
        let event = finished_event(&cfg.version, entity_id, is_lods, is_world, timestamp, f);
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

/// The `AssetBundleConversionFinishedEvent` body (@dcl/schemas base.ts /
/// services.ts): naming-critical — the registry consumer parses these exact
/// camelCase fields, and `key` is `{entityId}-{platform}`. `isLods` mirrors
/// upstream's `!!job.lods` so registry consumers can tell the LOD lane apart.
fn finished_event(
    version: &str,
    entity_id: &str,
    is_lods: bool,
    is_world: bool,
    timestamp: u64,
    f: &Finished,
) -> serde_json::Value {
    serde_json::json!({
        "type": "asset-bundle",
        "subType": "converted",
        "key": format!("{entity_id}-{}", f.platform),
        "timestamp": timestamp,
        "metadata": {
            "platform": f.platform,
            "entityId": entity_id,
            "isLods": is_lods,
            "isWorld": is_world,
            "statusCode": f.status_code,
            "version": version,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finished_event_json_is_pinned() {
        let event = finished_event(
            "v49",
            "bafkreia1b2c3",
            false,
            true,
            1_724_000_000_123,
            &Finished {
                platform: "windows",
                status_code: STATUS_ALREADY_CONVERTED,
            },
        );
        assert_eq!(
            event.to_string(),
            "{\"type\":\"asset-bundle\",\"subType\":\"converted\",\
             \"key\":\"bafkreia1b2c3-windows\",\"timestamp\":1724000000123,\
             \"metadata\":{\"platform\":\"windows\",\"entityId\":\"bafkreia1b2c3\",\
             \"isLods\":false,\"isWorld\":true,\"statusCode\":13,\"version\":\"v49\"}}"
        );
    }

    #[test]
    fn lod_finished_event_json_is_pinned() {
        let event = finished_event(
            "v49",
            "bafkreia1b2c3",
            true,
            false,
            1_724_000_000_123,
            &Finished {
                platform: "windows",
                status_code: 0,
            },
        );
        assert_eq!(
            event.to_string(),
            "{\"type\":\"asset-bundle\",\"subType\":\"converted\",\
             \"key\":\"bafkreia1b2c3-windows\",\"timestamp\":1724000000123,\
             \"metadata\":{\"platform\":\"windows\",\"entityId\":\"bafkreia1b2c3\",\
             \"isLods\":true,\"isWorld\":false,\"statusCode\":0,\"version\":\"v49\"}}"
        );
    }

    #[test]
    fn finished_event_carries_the_conversion_exit_code() {
        let event = finished_event(
            "v49",
            "e",
            false,
            false,
            0,
            &Finished {
                platform: "mac",
                status_code: 1,
            },
        );
        assert_eq!(event["metadata"]["statusCode"], 1);
        assert_eq!(event["metadata"]["isWorld"], false);
        assert_eq!(event["metadata"]["isLods"], false);
        assert_eq!(event["key"], "e-mac");
    }
}

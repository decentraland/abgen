use anyhow::{Context, Result};

/// Prod's triage-fast-path status for already-converted entities.
pub const STATUS_ALREADY_CONVERTED: i32 = 13;

/// Prod's UNEXPECTED_ERROR exit code — carried by failure tombstones.
pub const STATUS_UNEXPECTED_ERROR: i32 = 5;

pub struct Finished<'a> {
    pub platform: &'a str,
    pub status_code: i32,
    /// The version lane this platform's bundles live under — what the manifest at
    /// `manifest/{entityId}_{platform}.json` names, never a config default. The registry
    /// stores it verbatim as `versions.assets.{platform}.version` on every succeeded event
    /// (status 13 included) and clients build bundle URLs from it, so a lane other than
    /// the manifest's sends every client to 404s.
    pub version: String,
}

impl<'a> Finished<'a> {
    /// A platform skipped as already converted, reporting the lane its manifest names.
    pub fn already_converted(platform: &'a str, lane: &str) -> Self {
        Finished {
            platform,
            status_code: STATUS_ALREADY_CONVERTED,
            version: lane.to_string(),
        }
    }
}

/// One `AssetBundleConversionFinishedEvent` per platform, byte-compatible
/// with what consumer-server publishes (adapters/sns.ts). Must target a
/// DEDICATED topic, never the shared event-driven-sns bus — the prod
/// registry's filter matches every `asset-bundle` event. Errors propagate so
/// SQS redelivers; the skip path also notifies, so a failed publish is
/// re-emitted on redelivery.
pub fn send_finished(entity_id: &str, content_server: &str, finished: &[Finished]) -> Result<bool> {
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
    for (f, event) in finished.iter().zip(finished_events(
        entity_id,
        content_server,
        timestamp,
        finished,
    )) {
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

/// The event bodies `send_finished` publishes, one per platform, in order — the seam the
/// registry-facing tests read, since publishing itself needs SNS.
pub fn finished_events(
    entity_id: &str,
    content_server: &str,
    timestamp: u64,
    finished: &[Finished],
) -> Vec<serde_json::Value> {
    let is_world = content_server.contains("worlds-content-server");
    finished
        .iter()
        .map(|f| finished_event(entity_id, is_world, timestamp, f))
        .collect()
}

/// The `AssetBundleConversionFinishedEvent` body (@dcl/schemas base.ts /
/// services.ts): naming-critical — the registry consumer parses these exact
/// camelCase fields, and `key` is `{entityId}-{platform}`. `isLods` is part of the
/// schema and always `false`: the LOD lane publishes nothing to the registry.
fn finished_event(
    entity_id: &str,
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
            "isLods": false,
            "isWorld": is_world,
            "statusCode": f.status_code,
            "version": f.version,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finished_event_json_is_pinned() {
        let event = finished_event(
            "bafkreia1b2c3",
            true,
            1_724_000_000_123,
            &Finished {
                platform: "windows",
                status_code: STATUS_ALREADY_CONVERTED,
                version: "v49".to_string(),
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
    fn each_platform_reports_the_lane_its_own_manifest_names() {
        // A wearable whose mac manifest predates the lane split (still under the scene
        // lane) and whose windows manifest was written after it: the events say so, one
        // lane each, and no config default appears in either.
        let finished = [
            Finished {
                platform: "mac",
                status_code: STATUS_ALREADY_CONVERTED,
                version: "v1003".to_string(),
            },
            Finished {
                platform: "windows",
                status_code: 0,
                version: "v1500".to_string(),
            },
        ];
        let events: Vec<serde_json::Value> = finished
            .iter()
            .map(|f| finished_event("e", false, 0, f))
            .collect();
        assert_eq!(events[0]["metadata"]["version"], "v1003");
        assert_eq!(events[1]["metadata"]["version"], "v1500");
    }

    #[test]
    fn finished_event_carries_the_conversion_exit_code() {
        let event = finished_event(
            "e",
            false,
            0,
            &Finished {
                platform: "mac",
                status_code: 1,
                version: "v49".to_string(),
            },
        );
        assert_eq!(event["metadata"]["statusCode"], 1);
        assert_eq!(event["metadata"]["isWorld"], false);
        assert_eq!(event["metadata"]["isLods"], false);
        assert_eq!(event["key"], "e-mac");
    }
}

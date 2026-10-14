//! Satellite map rebuild records. After the LOD lane rebuilds a Genesis City scene's LODs, one
//! immutable JSON object under [`PREFIX`] names the parcels the deployment covers, whose
//! satellite map images are now stale. The map capture machine lists the prefix to know what to
//! re-capture (see the "Satellite rebuild records" section of `lambda/README.md`).

use serde_json::{json, Value};

/// Key prefix of the rebuild records.
pub const PREFIX: &str = "satellite/rebuild/";

/// The record of a converted entity, or `None` when it is not a Genesis City scene: a world, a
/// wearable or emote, or a scene whose pointers are not `x,y` parcels.
pub fn record(
    entity_doc: &Value,
    entity_id: &str,
    content_server: &str,
    rebuilt_at_ms: u64,
) -> Option<Value> {
    if crate::output::entity_type(entity_doc) != "scene"
        || content_server.contains("worlds-content-server")
    {
        return None;
    }

    let pointers = entity_doc.get("pointers")?.as_array()?;
    let mut parcels = Vec::with_capacity(pointers.len());

    for pointer in pointers {
        let (x, y) = parse_parcel(pointer.as_str()?)?;
        parcels.push(format!("{x},{y}"));
    }

    if parcels.is_empty() {
        return None;
    }

    Some(json!({
        "entityId": entity_id,
        "contentServer": content_server,
        "deployedAt": entity_doc.get("timestamp").and_then(Value::as_u64),
        "rebuiltAt": rebuilt_at_ms,
        "parcels": parcels,
    }))
}

/// `satellite/rebuild/{rebuiltAt}-{entityId}.json`, the time zero-padded to 13 digits so the
/// keys list in rebuild order. Each rebuild writes a new key; none is ever overwritten.
pub fn key(rebuilt_at_ms: u64, entity_id: &str) -> String {
    format!("{PREFIX}{rebuilt_at_ms:013}-{entity_id}.json")
}

/// Writes the record of a LOD rebuild and returns its key. The caller has established that the
/// LOD lane built and uploaded fresh LODs (`lod::rebuilt`); `None` when the records are
/// disabled, nothing is published, or the entity is not a Genesis City scene. Fail-soft like
/// the scene sources: a failed put is logged and never fails the job.
pub fn publish(
    cfg: &crate::config::Config,
    proxy: &abgen::live::Proxy,
    entity_doc: &Value,
    entity_id: &str,
    content_server: &str,
) -> Option<String> {
    if !cfg.satellite_rebuild_records || !proxy.space_configured() {
        return None;
    }

    let rebuilt_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let record = record(entity_doc, entity_id, content_server, rebuilt_at_ms)?;
    let key = key(rebuilt_at_ms, entity_id);
    proxy.space_put_key(&key, record.to_string().as_bytes());
    Some(key)
}

fn parse_parcel(pointer: &str) -> Option<(i32, i32)> {
    let (x, y) = pointer.trim().split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CATALYST: &str = "https://peer.decentraland.org/content";

    fn scene(pointers: &[&str]) -> Value {
        json!({ "type": "scene", "pointers": pointers, "timestamp": 1_724_000_000_000u64 })
    }

    #[test]
    fn record_json_is_pinned() {
        let record = record(
            &scene(&["0,0", "-1,0"]),
            "bafkreia1b2c3",
            CATALYST,
            1_724_000_000_123,
        )
        .unwrap();

        assert_eq!(
            record.to_string(),
            "{\"entityId\":\"bafkreia1b2c3\",\"contentServer\":\"https://peer.decentraland.org/content\",\
             \"deployedAt\":1724000000000,\"rebuiltAt\":1724000000123,\"parcels\":[\"0,0\",\"-1,0\"]}"
        );
    }

    #[test]
    fn normalizes_parcel_spacing() {
        let record = record(&scene(&[" 10 , -3 "]), "e", CATALYST, 1).unwrap();
        assert_eq!(record["parcels"], json!(["10,-3"]));
    }

    #[test]
    fn keeps_a_missing_deployment_time_null() {
        let doc = json!({ "type": "scene", "pointers": ["1,1"] });
        assert_eq!(
            record(&doc, "e", CATALYST, 1).unwrap()["deployedAt"],
            Value::Null
        );
    }

    #[test]
    fn skips_worlds() {
        let doc = scene(&["0,0"]);
        assert!(record(
            &doc,
            "e",
            "https://worlds-content-server.decentraland.org",
            1
        )
        .is_none());
    }

    #[test]
    fn skips_pointers_that_are_not_parcels() {
        assert!(record(&scene(&["my-world.dcl.eth"]), "e", CATALYST, 1).is_none());
        assert!(record(
            &scene(&["0,0", "urn:decentraland:off-chain:x"]),
            "e",
            CATALYST,
            1
        )
        .is_none());
        assert!(record(&scene(&[]), "e", CATALYST, 1).is_none());
    }

    #[test]
    fn skips_wearables_and_emotes() {
        for kind in ["wearable", "emote"] {
            let doc = json!({ "type": kind, "pointers": ["0,0"] });
            assert!(record(&doc, "e", CATALYST, 1).is_none(), "{kind}");
        }
    }

    #[test]
    fn keys_list_in_rebuild_order() {
        assert_eq!(
            key(1_724_000_000_123, "bafkrei"),
            "satellite/rebuild/1724000000123-bafkrei.json"
        );
        assert_eq!(key(42, "e"), "satellite/rebuild/0000000000042-e.json");
        assert!(key(999, "z") < key(1_000, "a"));
    }
}

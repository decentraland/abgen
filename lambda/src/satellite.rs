//! Satellite map rebuild records: immutable JSON objects under [`PREFIX`] that name a scene
//! whose captured image is now stale. The map capture machine lists the prefix, re-captures
//! what each record names and deletes it (see the "Satellite rebuild records" section of
//! `lambda/README.md`).
//!
//! Two kinds of deployment write one. A Genesis City scene writes one when the LOD lane
//! rebuilds its LODs, which is when something the map shows changed (`lod::record_rebuild`).
//! A Worlds scene writes one on every successful conversion (`on_conversion`): worlds are
//! captured whole, so every deployment is a capture request.

use serde_json::{json, Value};

/// Key prefix of the rebuild records.
pub const PREFIX: &str = "SatelliteMaps/rebuild/";

/// Whether `content_server` is a Worlds server rather than a Genesis City catalyst.
pub fn is_world(content_server: &str) -> bool {
    content_server.contains("worlds-content-server")
}

/// The record of a scene entity, or `None` when there is nothing to capture: a wearable or
/// emote, or a Genesis City scene whose pointers are not all `x,y` parcels.
///
/// A Genesis City record names the parcels to re-capture. A Worlds record names the world
/// (`metadata.worldConfiguration.name`, `null` when the deployment carries none) and whatever
/// parcel pointers it has, which the capture machine may use or ignore.
pub fn record(
    entity_doc: &Value,
    entity_id: &str,
    content_server: &str,
    rebuilt_at_ms: u64,
) -> Option<Value> {
    if crate::output::entity_type(entity_doc) != "scene" {
        return None;
    }
    let world = is_world(content_server);

    let mut parcels = Vec::new();
    for pointer in entity_doc
        .get("pointers")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        match pointer.as_str().and_then(parse_parcel) {
            Some((x, y)) => parcels.push(format!("{x},{y}")),
            // A world's pointers are its own business; a city scene's must all be parcels.
            None if world => continue,
            None => return None,
        }
    }
    if parcels.is_empty() && !world {
        return None;
    }

    let deployed_at = entity_doc.get("timestamp").and_then(Value::as_u64);
    Some(if world {
        json!({
            "entityId": entity_id,
            "contentServer": content_server,
            "world": entity_doc.pointer("/metadata/worldConfiguration/name").cloned(),
            "deployedAt": deployed_at,
            "rebuiltAt": rebuilt_at_ms,
            "parcels": parcels,
        })
    } else {
        json!({
            "entityId": entity_id,
            "contentServer": content_server,
            "deployedAt": deployed_at,
            "rebuiltAt": rebuilt_at_ms,
            "parcels": parcels,
        })
    })
}

/// `SatelliteMaps/rebuild/{rebuiltAt}-{entityId}.json`, the time zero-padded to 13 digits so the
/// keys list in rebuild order. Each rebuild writes a new key; none is ever overwritten.
pub fn key(rebuilt_at_ms: u64, entity_id: &str) -> String {
    format!("{PREFIX}{rebuilt_at_ms:013}-{entity_id}.json")
}

/// The Worlds half of the trigger: a record for every Worlds conversion that succeeded on
/// every platform, written by the conversion job itself. Genesis City scenes return `None`
/// here; theirs follow the LOD lane (`lod::record_rebuild`).
pub fn on_conversion(
    cfg: &crate::config::Config,
    proxy: &abgen::live::Proxy,
    entity_doc: &Value,
    entity_id: &str,
    content_server: &str,
    exit_code: i32,
) -> Option<String> {
    if !is_world(content_server) || exit_code != 0 {
        return None;
    }
    publish(cfg, proxy, entity_doc, entity_id, content_server)
}

/// Writes the record and returns its key. The caller has decided the deployment needs a
/// capture; `None` when the records are disabled, nothing is published, or `record` has
/// nothing to say. Fail-soft like the scene sources: a failed put is logged and never fails
/// the job.
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
    const WORLDS: &str = "https://worlds-content-server.decentraland.org/content";

    fn scene(pointers: &[&str]) -> Value {
        json!({ "type": "scene", "pointers": pointers, "timestamp": 1_724_000_000_000u64 })
    }

    fn world(name: &str) -> Value {
        json!({
            "type": "scene", "pointers": ["0,0", "0,1"], "timestamp": 1_724_000_000_000u64,
            "metadata": { "worldConfiguration": { "name": name } }
        })
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
    fn world_record_json_is_pinned() {
        let record = record(
            &world("my-world.dcl.eth"),
            "bafkreiw0r1d",
            WORLDS,
            1_724_000_000_123,
        )
        .unwrap();

        assert_eq!(
            record.to_string(),
            "{\"entityId\":\"bafkreiw0r1d\",\
             \"contentServer\":\"https://worlds-content-server.decentraland.org/content\",\
             \"world\":\"my-world.dcl.eth\",\"deployedAt\":1724000000000,\"rebuiltAt\":1724000000123,\
             \"parcels\":[\"0,0\",\"0,1\"]}"
        );
    }

    #[test]
    fn a_world_records_without_a_name_or_parcels() {
        let nameless = json!({ "type": "scene", "pointers": ["0,0"] });
        let unnamed = record(&nameless, "e", WORLDS, 1).unwrap();
        assert_eq!(unnamed["world"], Value::Null);

        let odd_pointers = json!({ "type": "scene", "pointers": ["my-world.dcl.eth"] });
        let unparcelled = record(&odd_pointers, "e", WORLDS, 1).unwrap();
        assert_eq!(unparcelled["parcels"], json!([]));
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
    fn skips_city_pointers_that_are_not_parcels() {
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
            assert!(record(&doc, "e", WORLDS, 1).is_none(), "{kind} on worlds");
        }
    }

    #[test]
    fn keys_list_in_rebuild_order() {
        assert_eq!(
            key(1_724_000_000_123, "bafkrei"),
            "SatelliteMaps/rebuild/1724000000123-bafkrei.json"
        );
        assert_eq!(key(42, "e"), "SatelliteMaps/rebuild/0000000000042-e.json");
        assert!(key(999, "z") < key(1_000, "a"));
    }

    /// Every successful Worlds conversion writes one record from the conversion job; a failed
    /// one and a Genesis City one write none there (the city's comes from the LOD lane).
    #[test]
    fn every_world_deployment_writes_a_record_on_conversion() {
        let (host, seen) = crate::output::tests::serve(vec![]);
        let _space = crate::output::tests::point_space_at(&host);
        let cfg = crate::config::Config {
            satellite_rebuild_records: true,
            ..crate::lod::tests::cfg()
        };
        let proxy = crate::convert::make_proxy(&cfg, "https://c/content");
        let doc = world("my-world.dcl.eth");

        let key = on_conversion(&cfg, &proxy, &doc, "bafkreiw0r1d", WORLDS, 0)
            .expect("a converted world is a capture request");
        assert!(
            key.starts_with(PREFIX) && key.ends_with("-bafkreiw0r1d.json"),
            "{key}"
        );
        assert_eq!(seen.lock().unwrap().clone(), vec![format!("PUT /{key}")]);

        assert!(
            on_conversion(&cfg, &proxy, &doc, "bafkreiw0r1d", WORLDS, 1).is_none(),
            "a failed conversion has nothing to capture"
        );
        assert!(
            on_conversion(&cfg, &proxy, &scene(&["0,0"]), "bafkcity", CATALYST, 0).is_none(),
            "a city scene waits for its LOD rebuild"
        );
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "only the world reached the space"
        );
    }
}

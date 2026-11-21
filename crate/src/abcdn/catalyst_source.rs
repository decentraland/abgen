use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::io::Read;
use std::time::Duration;

use dcl_contents::errors::ApiError;
use dcl_contents::registry::{async_trait, EntitySource};
use dcl_contents::types::{ActiveEntity, ContentFile};

const PROXY_TIMEOUT_SECS: u64 = 10;

#[derive(Clone)]
pub struct CatalystEntitySource {
    base: String,
    worlds_url: Option<String>,
    agent: ureq::Agent,
}

impl CatalystEntitySource {
    pub fn new(base: &str, worlds_url: Option<String>) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(PROXY_TIMEOUT_SECS)))
            .build()
            .into();
        Self {
            base: base.trim_end_matches('/').to_string(),
            worlds_url,
            agent,
        }
    }

    fn post_active(&self, body: serde_json::Value) -> Result<Vec<ActiveEntity>, ApiError> {
        let url = format!("{}/entities/active", self.base);
        let resp = self
            .agent
            .post(&url)
            .header("User-Agent", crate::catalyst::UA)
            .header("Content-Type", "application/json")
            .send(body.to_string())
            .map_err(|e| upstream(&url, e))?;
        let mut buf: Vec<u8> = Vec::new();
        resp.into_body()
            .into_reader()
            .read_to_end(&mut buf)
            .map_err(|e| upstream(&url, e))?;
        let parsed: serde_json::Value =
            serde_json::from_slice(&buf).map_err(|e| upstream(&url, e))?;
        let arr = parsed
            .as_array()
            .ok_or_else(|| upstream(&url, "response is not an array"))?;
        Ok(arr.iter().filter_map(parse_active_entity).collect())
    }

    // Reuse Catalyst's deployment API for historical IDs and deployer identity.
    // Registry stores conversion state, not a second copy of content metadata.
    fn deployments(&self, filter: &str, values: &[String]) -> Result<Vec<ActiveEntity>, ApiError> {
        if values.is_empty() {
            return Ok(Vec::new());
        }
        let mut result = Vec::new();
        for chunk in values.chunks(1000) {
            let filters = chunk
                .iter()
                .map(|value| format!("{filter}={}", encode_query(value)))
                .collect::<Vec<_>>()
                .join("&");
            let mut path = format!("/deployments?{filters}&sortingField=entityTimestamp&sortingOrder=DESC&limit=1000&fields=pointers,content,metadata");
            let mut visited = std::collections::HashSet::new();
            loop {
                if !path.starts_with("/deployments?") || !visited.insert(path.clone()) {
                    return Err(ApiError::upstream("invalid Catalyst deployment pagination"));
                }
                let url = format!("{}{}", self.base, path);
                let response = self
                    .agent
                    .get(&url)
                    .call()
                    .map_err(|error| upstream(&url, error))?;
                let mut bytes = Vec::new();
                response
                    .into_body()
                    .into_reader()
                    .take((64 << 20) + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|error| upstream(&url, error))?;
                if bytes.len() > 64 << 20 {
                    return Err(ApiError::upstream("Catalyst deployment page too large"));
                }
                let body: serde_json::Value =
                    serde_json::from_slice(&bytes).map_err(|error| upstream(&url, error))?;
                let rows = body
                    .get("deployments")
                    .and_then(|value| value.as_array())
                    .ok_or_else(|| upstream(&url, "missing deployments array"))?;
                for row in rows {
                    result.push(
                        parse_deployment(row)
                            .ok_or_else(|| upstream(&url, "invalid deployment"))?,
                    );
                }
                match body
                    .pointer("/pagination/next")
                    .and_then(|next| next.as_str())
                {
                    Some(next) => {
                        // Catalyst emits query-relative links; also accept its
                        // endpoint-relative form without permitting another origin.
                        path = if next.starts_with('?') {
                            format!("/deployments{next}")
                        } else {
                            next.to_string()
                        };
                    }
                    None if body
                        .pointer("/pagination/moreData")
                        .and_then(|v| v.as_bool())
                        == Some(true) =>
                    {
                        return Err(upstream(&url, "missing next deployment page"))
                    }
                    None => break,
                }
            }
        }
        let mut seen = std::collections::HashSet::new();
        result.retain(|entity| seen.insert(entity.entity_id.clone()));
        result.sort_by(|a, b| {
            b.timestamp
                .cmp(&a.timestamp)
                .then(a.entity_id.cmp(&b.entity_id))
        });
        Ok(result)
    }

    fn fetch_world_entity(&self, id: &str) -> Result<Option<ActiveEntity>, ApiError> {
        let Some(base) = self.worlds_url.as_deref() else {
            return Ok(None);
        };
        let url = format!(
            "{}/contents/{}",
            base.trim_end_matches('/'),
            encode_query(id)
        );
        let response = match self.agent.get(&url).call() {
            Ok(response) => response,
            Err(ureq::Error::StatusCode(404)) => return Ok(None),
            Err(error) => return Err(upstream(&url, error)),
        };
        let mut bytes = Vec::new();
        response
            .into_body()
            .into_reader()
            .take((64 << 20) + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| upstream(&url, error))?;
        if bytes.len() > 64 << 20 {
            return Err(upstream(&url, "world entity too large"));
        }
        let mut body: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|error| upstream(&url, error))?;
        if let Some(object) = body.as_object_mut() {
            object
                .entry("id")
                .or_insert_with(|| serde_json::Value::String(id.to_string()));
        }
        let entity =
            parse_world_entity(&body).ok_or_else(|| upstream(&url, "invalid world entity"))?;
        if entity.entity_id != id {
            return Err(upstream(&url, "world entity ID mismatch"));
        }
        Ok(Some(entity))
    }

    fn fetch_union(&self, queries: &[String]) -> Result<Vec<ActiveEntity>, ApiError> {
        let lowered: Vec<String> = queries.iter().map(|q| q.to_lowercase()).collect();
        let by_pointer = self.post_active(serde_json::json!({ "pointers": lowered }))?;
        let by_id = self.post_active(serde_json::json!({ "ids": queries }))?;

        let mut by_entity: HashMap<String, ActiveEntity> = HashMap::new();
        for ent in by_pointer.into_iter().chain(by_id) {
            match by_entity.entry(ent.entity_id.clone()) {
                Entry::Occupied(mut cur) => {
                    if ent.timestamp > cur.get().timestamp {
                        cur.insert(ent);
                    }
                }
                Entry::Vacant(slot) => {
                    slot.insert(ent);
                }
            }
        }
        Ok(by_entity.into_values().collect())
    }
}

#[async_trait]
impl EntitySource for CatalystEntitySource {
    async fn resolve_pointers(&self, pointers: &[String]) -> Result<Vec<ActiveEntity>, ApiError> {
        if pointers.is_empty() {
            return Ok(Vec::new());
        }
        let src = self.clone();
        let queries = pointers.to_vec();
        run_blocking(move || src.fetch_union(&queries)).await
    }

    async fn resolve_one(&self, id: &str) -> Result<Option<ActiveEntity>, ApiError> {
        let src = self.clone();
        let id = id.to_string();
        run_blocking(move || {
            if let Some(entity) = src
                .deployments("entityId", std::slice::from_ref(&id))?
                .into_iter()
                .next()
            {
                return Ok(Some(entity));
            }
            src.fetch_world_entity(&id)
        })
        .await
    }

    async fn resolve_by_deployer(&self, deployer: &str) -> Result<Vec<ActiveEntity>, ApiError> {
        let src = self.clone();
        let deployer = deployer.to_lowercase();
        run_blocking(move || src.deployments("deployedBy", &[deployer])).await
    }

    async fn resolve_candidates(
        &self,
        pointers: &[String],
        world_name: Option<&str>,
    ) -> Result<Vec<ActiveEntity>, ApiError> {
        let mut entities = if let Some(world) = world_name {
            self.resolve_world(world).await?
        } else {
            let src = self.clone();
            let pointers = pointers.to_vec();
            run_blocking(move || src.deployments("pointer", &pointers)).await?
        };
        entities.retain(|entity| {
            entity
                .pointers
                .iter()
                .any(|p| pointers.iter().any(|q| p.eq_ignore_ascii_case(q)))
                && match (world_name, entity.world_name()) {
                    (None, None) => true,
                    (Some(wanted), Some(actual)) => wanted.eq_ignore_ascii_case(actual),
                    _ => false,
                }
        });
        Ok(entities)
    }

    async fn resolve_status(
        &self,
        id: &str,
        world_name: Option<&str>,
    ) -> Result<Option<ActiveEntity>, ApiError> {
        if let Some(entity) = self.resolve_one(id).await? {
            return Ok(Some(entity));
        }
        Ok(self
            .resolve_candidates(&[id.to_string()], world_name)
            .await?
            .into_iter()
            .max_by_key(|entity| entity.timestamp))
    }

    async fn resolve_profiles(&self, addresses: &[String]) -> Result<Vec<ActiveEntity>, ApiError> {
        if addresses.is_empty() {
            return Ok(Vec::new());
        }
        let src = self.clone();
        let lowered: Vec<String> = addresses.iter().map(|a| a.to_lowercase()).collect();
        run_blocking(move || {
            let mut ents = src.post_active(serde_json::json!({ "pointers": lowered }))?;
            ents.retain(|e| e.entity_type == "profile");
            Ok(ents)
        })
        .await
    }

    async fn resolve_world(&self, world_name: &str) -> Result<Vec<ActiveEntity>, ApiError> {
        let Some(worlds_url) = self.worlds_url.clone() else {
            return Err(ApiError::upstream("local Worlds source is not configured"));
        };
        let src = self.clone();
        let name = world_name.to_string();
        run_blocking(move || {
            let secs = crate::worlds::SERVE_FETCH_TIMEOUT_SECS;
            let scenes = match crate::worlds::resolve_world_bounded(&worlds_url, &name, secs) {
                Ok(scenes) => scenes,
                Err(e) if world_not_found(&e) => return Ok(Vec::new()),
                Err(e) => return Err(ApiError::upstream(format!("world {name}: {e:#}"))),
            };
            let mut out = Vec::new();
            for scene in scenes {
                // A local Worlds manifest names entity identities. Hydrate
                // them through that same configured authority, ignoring URN
                // baseUrl hints that may point outside this node.
                let entity = src.fetch_world_entity(&scene.entity_id)?.ok_or_else(|| {
                    ApiError::upstream(format!(
                        "world {name} references unavailable entity {}",
                        scene.entity_id
                    ))
                })?;
                out.push(entity);
            }
            Ok(out)
        })
        .await
    }
}

async fn run_blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::internal(format!("registry proxy worker panicked: {e}")))?
}

fn upstream(url: &str, detail: impl std::fmt::Display) -> ApiError {
    ApiError::upstream(format!("{url}: {detail}"))
}

fn world_not_found(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        matches!(
            c.downcast_ref::<ureq::Error>(),
            Some(ureq::Error::StatusCode(404))
        )
    })
}

fn parse_active_entity(v: &serde_json::Value) -> Option<ActiveEntity> {
    let entity_id = v.get("id")?.as_str()?.to_string();
    Some(ActiveEntity {
        deployment_id: 0,
        entity_id,
        entity_type: v
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string(),
        timestamp: v.get("timestamp").and_then(|t| t.as_i64()).unwrap_or(0),
        pointers: v
            .get("pointers")
            .and_then(|p| p.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|p| p.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        metadata: v
            .get("metadata")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        deployer_address: None,
        content: v
            .get("content")
            .and_then(|c| c.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|c| {
                        Some(ContentFile {
                            file: c.get("file")?.as_str()?.to_string(),
                            hash: c.get("hash")?.as_str()?.to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
    })
}

// Registry hydration must not turn a malformed scene into an empty or partial
// world, which would look like an undeployment to the native state machine.
fn parse_world_entity(value: &serde_json::Value) -> Option<ActiveEntity> {
    Some(ActiveEntity {
        deployment_id: 0,
        entity_id: value
            .get("id")?
            .as_str()
            .filter(|id| !id.is_empty())?
            .to_string(),
        entity_type: value
            .get("type")?
            .as_str()
            .filter(|kind| !kind.is_empty())?
            .to_string(),
        timestamp: value.get("timestamp")?.as_i64()?,
        pointers: value
            .get("pointers")?
            .as_array()?
            .iter()
            .map(|pointer| pointer.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()?,
        content: value
            .get("content")?
            .as_array()?
            .iter()
            .map(|file| {
                Some(ContentFile {
                    file: file.get("file")?.as_str()?.to_string(),
                    hash: file.get("hash")?.as_str()?.to_string(),
                })
            })
            .collect::<Option<Vec<_>>>()?,
        metadata: value
            .get("metadata")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        deployer_address: None,
    })
}

fn encode_query(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn parse_deployment(value: &serde_json::Value) -> Option<ActiveEntity> {
    // These fields are guaranteed by Catalyst's deployment response with
    // fields=pointers,content,metadata. Reject a malformed page instead of
    // fabricating timestamp zero or silently discarding broken content files.
    Some(ActiveEntity {
        deployment_id: 0,
        entity_id: value
            .get("entityId")?
            .as_str()
            .filter(|id| !id.is_empty())?
            .to_string(),
        entity_type: value.get("entityType")?.as_str()?.to_string(),
        timestamp: value.get("entityTimestamp")?.as_i64()?,
        pointers: value
            .get("pointers")?
            .as_array()?
            .iter()
            .map(|pointer| pointer.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()?,
        content: value
            .get("content")?
            .as_array()?
            .iter()
            .map(|file| {
                Some(ContentFile {
                    file: file.get("key")?.as_str()?.to_string(),
                    hash: file.get("hash")?.as_str()?.to_string(),
                })
            })
            .collect::<Option<Vec<_>>>()?,
        metadata: value
            .get("metadata")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        deployer_address: Some(value.get("deployedBy")?.as_str()?.to_lowercase()),
    })
}

#[cfg(test)]
mod tests {
    use super::{parse_active_entity, parse_deployment, CatalystEntitySource};
    use dcl_contents::registry::EntitySource;
    use serde_json::{json, Value};

    fn deployment(id: &str, timestamp: i64, world: Option<&str>) -> Value {
        json!({
            "entityId": id, "entityType":"scene", "entityTimestamp":timestamp,
            "deployedBy":"0xAbC", "pointers":["0,0"],
            "content":[{"key":"scene.json", "hash":"bafkcontent"}],
            "metadata":world.map(|name| json!({"worldConfiguration":{"name":name}})).unwrap_or_else(|| json!({})),
        })
    }

    fn query(filter: &str, encoded_value: &str) -> String {
        format!("/deployments?{filter}={encoded_value}&sortingField=entityTimestamp&sortingOrder=DESC&limit=1000&fields=pointers,content,metadata")
    }

    fn response(path: &str, value: Value) -> (String, u16, Vec<u8>) {
        (path.to_string(), 200, serde_json::to_vec(&value).unwrap())
    }

    #[test]
    fn deployment_projection_preserves_deployer_and_rejects_malformed_fields() {
        let value = deployment("bafknew", 123, None);
        let entity = parse_deployment(&value).unwrap();
        assert_eq!(entity.deployer_address.as_deref(), Some("0xabc"));
        assert_eq!(entity.timestamp, 123);
        assert_eq!(entity.content[0].file, "scene.json");
        assert_eq!(entity.content[0].hash, "bafkcontent");
        for (field, bad) in [
            ("entityId", json!(null)),
            ("entityTimestamp", json!("123")),
            ("entityTimestamp", json!({})),
            ("pointers", json!([1])),
            ("content", json!([{"key":"scene.json","hash":null}])),
            ("deployedBy", json!(42)),
        ] {
            let mut broken = value.clone();
            broken[field] = bad;
            assert!(
                parse_deployment(&broken).is_none(),
                "accepted malformed {field}"
            );
        }
    }

    #[tokio::test]
    async fn history_pages_preserve_fallback_candidates_deployer_and_genesis_scope() {
        let first = query("pointer", "0%2C0");
        let next = "?cursor=older";
        let (host, seen) = crate::live::stub::serve(vec![
            response(
                &first,
                json!({"deployments":[deployment("new",300,None), deployment("world",400,Some("world.dcl.eth"))], "pagination":{"moreData":true,"next":next}}),
            ),
            response(
                &format!("/deployments{next}"),
                json!({"deployments":[deployment("old",100,None), deployment("new",300,None)], "pagination":{"moreData":false}}),
            ),
        ]);
        let source = CatalystEntitySource::new(&format!("http://{host}"), None);
        let entities = source
            .resolve_candidates(&["0,0".to_string()], None)
            .await
            .unwrap();
        assert_eq!(
            entities
                .iter()
                .map(|e| e.entity_id.as_str())
                .collect::<Vec<_>>(),
            ["new", "old"]
        );
        assert!(entities
            .iter()
            .all(|e| e.deployer_address.as_deref() == Some("0xabc")));
        assert_eq!(entities[1].content[0].file, "scene.json");
        assert_eq!(
            *seen.lock().unwrap(),
            [format!("GET {first}"), format!("GET /deployments{next}")]
        );
    }

    #[tokio::test]
    async fn deployment_pagination_rejects_loops_external_urls_and_missing_next() {
        let first = query("deployedBy", "0xabc");
        for pagination in [
            json!({"moreData":true,"next":first}),
            json!({"moreData":true,"next":"http://127.0.0.1:9/deployments?cursor=other"}),
            json!({"moreData":true}),
        ] {
            let (host, seen) = crate::live::stub::serve(vec![response(
                &first,
                json!({"deployments":[], "pagination":pagination}),
            )]);
            let source = CatalystEntitySource::new(&format!("http://{host}"), None);
            assert!(source.resolve_by_deployer("0xAbC").await.is_err());
            assert_eq!(*seen.lock().unwrap(), [format!("GET {first}")]);
        }
    }

    #[tokio::test]
    async fn exact_entity_id_takes_precedence_over_a_newer_pointer_match() {
        let exact = query("entityId", "named-pointer");
        let pointer = query("pointer", "named-pointer");
        let (host, seen) = crate::live::stub::serve(vec![
            response(
                &exact,
                json!({"deployments":[deployment("named-pointer",100,None)],"pagination":{"moreData":false}}),
            ),
            response(
                &pointer,
                json!({"deployments":[deployment("new",300,None)],"pagination":{"moreData":false}}),
            ),
        ]);
        let source = CatalystEntitySource::new(&format!("http://{host}"), None);
        let entity = source
            .resolve_status("named-pointer", None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entity.entity_id, "named-pointer");
        assert_eq!(*seen.lock().unwrap(), [format!("GET {exact}")]);
    }

    #[tokio::test]
    async fn status_resolves_latest_pointer_when_exact_id_is_absent() {
        let exact = query("entityId", "0%2C0");
        let pointer = query("pointer", "0%2C0");
        let (host, seen) = crate::live::stub::serve(vec![
            response(
                &exact,
                json!({"deployments":[],"pagination":{"moreData":false}}),
            ),
            response(
                &pointer,
                json!({"deployments":[deployment("old",100,None),deployment("new",300,None)],"pagination":{"moreData":false}}),
            ),
        ]);
        let source = CatalystEntitySource::new(&format!("http://{host}"), None);
        let entity = source.resolve_status("0,0", None).await.unwrap().unwrap();
        assert_eq!(entity.entity_id, "new");
        assert_eq!(
            *seen.lock().unwrap(),
            [format!("GET {exact}"), format!("GET {pointer}")]
        );
    }

    #[tokio::test]
    async fn historical_world_id_uses_configured_world_contents_after_empty_catalyst_history() {
        let exact = query("entityId", "bafkworldold");
        let (host, seen) = crate::live::stub::serve(vec![
            response(
                &exact,
                json!({"deployments":[],"pagination":{"moreData":false}}),
            ),
            response(
                "/contents/bafkworldold",
                json!({
                    "type":"scene", "timestamp":123,
                    "pointers":["0,0"], "content":[], "metadata":{"worldConfiguration":{"name":"world.dcl.eth"}},
                }),
            ),
        ]);
        let base = format!("http://{host}");
        let source = CatalystEntitySource::new(&base, Some(base.clone()));
        let entity = source.resolve_one("bafkworldold").await.unwrap().unwrap();
        assert_eq!(entity.entity_id, "bafkworldold");
        assert_eq!(entity.world_name(), Some("world.dcl.eth"));
        assert_eq!(
            *seen.lock().unwrap(),
            [
                format!("GET {exact}"),
                "GET /contents/bafkworldold".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn world_candidates_match_world_and_pointer_without_genesis_fallback() {
        let scene = |id: &str, pointer: &str, world: Option<&str>| {
            json!({
                "id":id, "type":"scene", "timestamp":123, "pointers":[pointer], "content":[],
                "metadata":world.map(|name| json!({"worldConfiguration":{"name":name}})).unwrap_or_else(|| json!({})),
            })
        };
        let (host, seen) = crate::live::stub::serve(vec![
            response(
                "/world/world.dcl.eth/about",
                json!({"configurations":{"scenesUrn":[
                    "urn:decentraland:entity:matching", "urn:decentraland:entity:genesis", "urn:decentraland:entity:other-parcel"
                ]}}),
            ),
            response(
                "/contents/matching",
                scene("matching", "0,0", Some("World.dcl.eth")),
            ),
            response("/contents/genesis", scene("genesis", "0,0", None)),
            response(
                "/contents/other-parcel",
                scene("other-parcel", "1,1", Some("world.dcl.eth")),
            ),
        ]);
        let base = format!("http://{host}");
        let source = CatalystEntitySource::new(&base, Some(base.clone()));
        let entities = source
            .resolve_candidates(&["0,0".to_string()], Some("world.dcl.eth"))
            .await
            .unwrap();
        assert_eq!(
            entities
                .iter()
                .map(|e| e.entity_id.as_str())
                .collect::<Vec<_>>(),
            ["matching"]
        );
        assert!(source
            .resolve_candidates(&["0,0".to_string()], Some("missing.dcl.eth"))
            .await
            .unwrap()
            .is_empty());
        assert!(seen.lock().unwrap().iter().all(
            |request| !request.contains("deployments") && !request.contains("entities/active")
        ));
    }

    #[tokio::test]
    async fn world_scene_urn_hints_cannot_replace_the_configured_local_authority() {
        let (foreign_host, foreign_seen) = crate::live::stub::serve(vec![response(
            "/foreign/scene",
            json!({"unexpected":"foreign source"}),
        )]);
        let (host, seen) = crate::live::stub::serve(vec![
            response(
                "/world/local.dcl.eth/about",
                json!({"configurations":{"scenesUrn":[
                    format!("urn:decentraland:entity:scene?baseUrl=http://{foreign_host}/foreign/")
                ]}}),
            ),
            response(
                "/contents/scene",
                json!({
                    "type":"scene", "timestamp":123, "pointers":["0,0"], "content":[],
                    "metadata":{"worldConfiguration":{"name":"local.dcl.eth"}},
                }),
            ),
        ]);
        let base = format!("http://{host}");
        let source = CatalystEntitySource::new(&base, Some(base.clone()));
        let entities = source.resolve_world("local.dcl.eth").await.unwrap();
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].entity_id, "scene");
        assert_eq!(
            *seen.lock().unwrap(),
            ["GET /world/local.dcl.eth/about", "GET /contents/scene"]
        );
        assert!(foreign_seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn world_scene_failures_are_errors_instead_of_partial_undeployments() {
        let valid = json!({
            "type":"scene", "timestamp":123, "pointers":["0,0"], "content":[],
            "metadata":{"worldConfiguration":{"name":"local.dcl.eth"}},
        });
        let mut mismatch = valid.clone();
        mismatch["id"] = json!("different-id");
        let mut invalid_timestamp = valid.clone();
        invalid_timestamp["timestamp"] = json!("123");
        for (status, body) in [
            (404, Vec::new()),
            (503, Vec::new()),
            (200, b"not JSON".to_vec()),
            (200, serde_json::to_vec(&mismatch).unwrap()),
            (200, serde_json::to_vec(&invalid_timestamp).unwrap()),
        ] {
            let (host, seen) = crate::live::stub::serve(vec![
                response(
                    "/world/local.dcl.eth/about",
                    json!({"configurations":{"scenesUrn":[
                        "urn:decentraland:entity:good", "urn:decentraland:entity:bad"
                    ]}}),
                ),
                response("/contents/good", valid.clone()),
                ("/contents/bad".into(), status, body),
            ]);
            let base = format!("http://{host}");
            let source = CatalystEntitySource::new(&base, Some(base.clone()));
            assert!(
                source.resolve_world("local.dcl.eth").await.is_err(),
                "accepted incomplete world after HTTP {status}"
            );
            assert_eq!(seen.lock().unwrap().len(), 3);
        }
        let unconfigured = CatalystEntitySource::new("http://127.0.0.1:9", None);
        assert!(unconfigured.resolve_world("local.dcl.eth").await.is_err());
    }

    #[test]
    fn parse_active_entity_maps_catalyst_shape() {
        let v = serde_json::json!({
            "id": "bafkreitest",
            "type": "profile",
            "timestamp": 1782484179697i64,
            "pointers": ["0x24e5f44999c151f08609f8e27b2238c773c4d020"],
            "content": [{"file": "face.png", "hash": "bafkface"}],
            "metadata": {"avatars": []},
            "version": "v3",
        });
        let e = parse_active_entity(&v).unwrap();
        assert_eq!(e.entity_id, "bafkreitest");
        assert_eq!(e.entity_type, "profile");
        assert_eq!(e.timestamp, 1782484179697);
        assert_eq!(
            e.pointers,
            vec!["0x24e5f44999c151f08609f8e27b2238c773c4d020"]
        );
        assert_eq!(e.content.len(), 1);
        assert_eq!(e.content[0].file, "face.png");
        assert_eq!(e.content[0].hash, "bafkface");
        assert_eq!(e.deployment_id, 0);
        assert!(e.deployer_address.is_none());
        assert!(e.metadata.get("avatars").is_some());

        assert!(parse_active_entity(&serde_json::json!({"type": "scene"})).is_none());
    }
}

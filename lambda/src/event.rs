//! Event parsing: SQS record batches, catalyst `DeploymentToSqs` bodies, and
//! plain manual payloads all normalize into [`Job`]s.

use anyhow::{bail, Context, Result};
use serde_json::Value;

pub struct Job {
    pub entity_id: String,
    /// Content server root from the event (`contentServerUrls[0]`), normalized
    /// to strip a trailing `/` and a `/contents` suffix. `None` for manual
    /// payloads that omit it — the configured default applies.
    pub content_server_url: Option<String>,
    /// `DeploymentToSqs.lods` was present: a LOD-generation job, not an
    /// entity conversion.
    pub is_lods: bool,
    /// `"force": true` in the payload — convert even when the CDN already
    /// has a current manifest (manual requeues).
    pub force: bool,
}

/// Accepts, in order of detection:
/// * an SQS event: `{"Records": [{"body": "<json string>"}, …]}` where each
///   body is one of the shapes below,
/// * a `DeploymentToSqs`: `{"entity": {"entityId": "…"},
///   "contentServerUrls": ["…"], "lods"?: […]}`,
/// * a manual payload: `{"entityId": "…", "contentServerUrl"?: "…"}`.
pub fn jobs_from_event(event: &Value) -> Result<Vec<Job>> {
    if let Some(records) = event.get("Records").and_then(Value::as_array) {
        let mut jobs = Vec::with_capacity(records.len());
        for (i, record) in records.iter().enumerate() {
            let body = record
                .get("body")
                .and_then(Value::as_str)
                .with_context(|| format!("SQS record {i} has no string body"))?;
            let parsed: Value = serde_json::from_str(body)
                .with_context(|| format!("SQS record {i} body is not JSON"))?;
            jobs.push(job_from_value(&parsed).with_context(|| format!("SQS record {i}"))?);
        }
        return Ok(jobs);
    }
    Ok(vec![job_from_value(event)?])
}

fn job_from_value(v: &Value) -> Result<Job> {
    let force = v.get("force").and_then(Value::as_bool).unwrap_or(false);

    // Manual payload.
    if let Some(id) = v.get("entityId").and_then(Value::as_str) {
        return Ok(Job {
            entity_id: validated_entity_id(id)?,
            content_server_url: v
                .get("contentServerUrl")
                .and_then(Value::as_str)
                .map(normalize_content_server),
            is_lods: false,
            force,
        });
    }
    // DeploymentToSqs.
    if let Some(id) = v
        .pointer("/entity/entityId")
        .and_then(Value::as_str)
    {
        let is_lods = v.get("lods").map(|l| !l.is_null()).unwrap_or(false);
        let content_server_url = v
            .get("contentServerUrls")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(Value::as_str)
            .map(normalize_content_server);
        if !is_lods && content_server_url.is_none() {
            bail!("deployment for {id} has no contentServerUrls — not a conversion job");
        }
        return Ok(Job {
            entity_id: validated_entity_id(id)?,
            content_server_url,
            is_lods,
            force,
        });
    }
    bail!("unrecognized event shape: expected entityId or entity.entityId")
}

/// Mirrors the consumer-server's guard: content-addressed ids are strictly
/// alphanumeric, and the id ends up in shell-adjacent contexts (paths, URLs),
/// so reject anything else outright.
fn validated_entity_id(id: &str) -> Result<String> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        bail!(
            "invalid entityId {:?}",
            id.chars().take(80).collect::<String>()
        );
    }
    Ok(id.to_string())
}

/// abgen's catalyst client wants the content root (`…/content`) and appends
/// `/contents/{hash}` itself; events sometimes carry the `/contents` form.
fn normalize_content_server(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    trimmed
        .strip_suffix("/contents")
        .unwrap_or(trimmed)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_manual_payload() {
        let e = serde_json::json!({"entityId": "bafkabc123", "contentServerUrl": "https://peer.decentraland.org/content/contents/"});
        let jobs = jobs_from_event(&e).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].entity_id, "bafkabc123");
        assert_eq!(
            jobs[0].content_server_url.as_deref(),
            Some("https://peer.decentraland.org/content")
        );
        assert!(!jobs[0].is_lods);
    }

    #[test]
    fn parses_sqs_wrapped_deployment() {
        let body = serde_json::json!({
            "entity": {"entityId": "bafkdef456", "authChain": []},
            "contentServerUrls": ["https://peer.decentraland.org/content"]
        })
        .to_string();
        let e = serde_json::json!({"Records": [{"body": body}]});
        let jobs = jobs_from_event(&e).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].entity_id, "bafkdef456");
        assert!(!jobs[0].is_lods);
    }

    #[test]
    fn parses_force_flag() {
        let e = serde_json::json!({"entityId": "bafkforce1", "force": true});
        assert!(jobs_from_event(&e).unwrap()[0].force);
        let e = serde_json::json!({"entityId": "bafkforce2"});
        assert!(!jobs_from_event(&e).unwrap()[0].force);
    }

    #[test]
    fn flags_lods_jobs() {
        let e = serde_json::json!({
            "entity": {"entityId": "bafklod789"},
            "lods": ["https://x/lod0.glb"]
        });
        let jobs = jobs_from_event(&e).unwrap();
        assert!(jobs[0].is_lods);
    }

    #[test]
    fn rejects_bad_entity_ids() {
        let e = serde_json::json!({"entityId": "../../etc/passwd"});
        assert!(jobs_from_event(&e).is_err());
        let e = serde_json::json!({"entityId": ""});
        assert!(jobs_from_event(&e).is_err());
    }

    #[test]
    fn rejects_deployment_without_content_server() {
        let e = serde_json::json!({"entity": {"entityId": "bafknourl"}});
        assert!(jobs_from_event(&e).is_err());
    }
}

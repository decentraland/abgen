use anyhow::{bail, Context, Result};
use serde_json::Value;

pub struct Job {
    pub entity_id: String,
    pub content_server_url: Option<String>,
    pub is_lods: bool,
    pub force: bool,
}

pub struct Record {
    /// `None` when the record carries no usable `messageId`; such a record can
    /// never be named in a `batchItemFailures` entry.
    pub message_id: Option<String>,
    pub job: Result<Job>,
}

pub enum Event {
    /// Direct invoke (console, `--once`, manual payload): no partial-batch protocol.
    Direct(Job),
    Sqs(Vec<Record>),
}

pub fn parse_event(event: &Value) -> Result<Event> {
    if let Some(records) = event.get("Records").and_then(Value::as_array) {
        let parsed = records
            .iter()
            .enumerate()
            .map(|(i, record)| Record {
                message_id: record
                    .get("messageId")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(str::to_string),
                job: job_from_record(record).with_context(|| format!("SQS record {i}")),
            })
            .collect();
        return Ok(Event::Sqs(parsed));
    }
    Ok(Event::Direct(job_from_value(event)?))
}

fn job_from_record(record: &Value) -> Result<Job> {
    let body = record
        .get("body")
        .and_then(Value::as_str)
        .context("no string body")?;
    let parsed: Value = serde_json::from_str(body).context("body is not JSON")?;
    job_from_value(&parsed)
}

fn job_from_value(v: &Value) -> Result<Job> {
    let force = v.get("force").and_then(Value::as_bool).unwrap_or(false);

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
    if let Some(id) = v.pointer("/entity/entityId").and_then(Value::as_str) {
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

fn validated_entity_id(id: &str) -> Result<String> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        bail!(
            "invalid entityId {:?}",
            id.chars().take(80).collect::<String>()
        );
    }
    Ok(id.to_string())
}

/// Scheme/shape validation (https, no userinfo, non-empty host) runs
/// unconditionally — an event-supplied URL is attacker-adjacent input and must
/// never point the handler at plaintext or internal targets, allowlist or not.
/// The host allowlist additionally applies when configured.
pub fn validate_content_server(url: &str, allowed: Option<&[String]>) -> Result<()> {
    let Some(rest) = url.strip_prefix("https://") else {
        bail!("content server {url:?} rejected: https required");
    };
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let host = authority.split(':').next().unwrap_or("");
    if authority.contains('@') || host.is_empty() {
        bail!("content server {url:?} rejected: bad authority");
    }
    if let Some(allowed) = allowed {
        if !allowed.iter().any(|a| a == host) {
            bail!("content server host {host:?} is not in ALLOWED_CONTENT_SERVER_HOSTS");
        }
    }
    Ok(())
}

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

    fn jobs_from_event(event: &Value) -> Result<Vec<Job>> {
        match parse_event(event)? {
            Event::Direct(job) => Ok(vec![job]),
            Event::Sqs(records) => records.into_iter().map(|r| r.job).collect(),
        }
    }

    fn records(event: &Value) -> Vec<Record> {
        match parse_event(event).unwrap() {
            Event::Sqs(records) => records,
            Event::Direct(_) => panic!("expected an SQS batch"),
        }
    }

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
    fn keeps_per_record_message_ids_and_isolates_parse_errors() {
        let ok = serde_json::json!({"entityId": "bafkok0001"}).to_string();
        let e = serde_json::json!({"Records": [
            {"messageId": "m-1", "body": ok},
            {"messageId": "m-2", "body": "not json"},
            {"messageId": "", "body": "{}"},
            {"body": "{}"},
        ]});
        let records = records(&e);
        assert_eq!(records.len(), 4);
        assert_eq!(records[0].message_id.as_deref(), Some("m-1"));
        assert_eq!(records[0].job.as_ref().unwrap().entity_id, "bafkok0001");
        assert_eq!(records[1].message_id.as_deref(), Some("m-2"));
        assert!(records[1].job.is_err());
        assert!(records[2].message_id.is_none());
        assert!(records[3].message_id.is_none());
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

    #[test]
    fn content_server_allowlist() {
        let allowed = vec!["peer.decentraland.org".to_string()];
        let ok = |u: &str| validate_content_server(u, Some(&allowed)).is_ok();
        assert!(ok("https://peer.decentraland.org/content"));
        assert!(ok("https://PEER.decentraland.org"));
        assert!(ok("https://peer.decentraland.org:443/content"));
        assert!(!ok("http://peer.decentraland.org/content"));
        assert!(!ok("https://evil.example.com/content"));
        assert!(!ok("https://peer.decentraland.org.evil.com/content"));
        assert!(!ok("https://peer.decentraland.org@evil.com/content"));
        assert!(!ok("https://sub.peer.decentraland.org/content"));
    }

    #[test]
    fn content_server_shape_checks_apply_without_an_allowlist() {
        let ok = |u: &str| validate_content_server(u, None).is_ok();
        // No allowlist: any https host passes (documented fail-open)…
        assert!(ok("https://peer.decentraland.org/content"));
        assert!(ok("https://anything.example.com"));
        // …but scheme/shape validation still applies unconditionally.
        assert!(!ok("http://10.0.3.7:8500"));
        assert!(!ok("http://169.254.169.254/latest/meta-data"));
        assert!(!ok("https://user:pass@evil.example.com/content"));
        assert!(!ok("https://"));
        assert!(!ok("ftp://host"));
    }
}

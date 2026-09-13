//! The asset-bundle registry's view of what is currently published for a set of pointers.
//!
//! A LOD job arrives naming the *new* entity, while the registry still answers with the
//! deployment it superseded — that older entity is the one whose bundles may be reusable.
//! Only `POST /entities/active` is used; worlds pass their name as a query parameter.

use anyhow::{Context, Result};

/// What the registry holds for a pointer set, reduced to what the reuse decision needs.
pub struct Active {
    pub entity_id: String,
    /// Per-platform LOD status, e.g. `mac -> "complete"`.
    pub lods: serde_json::Map<String, serde_json::Value>,
    /// Digest of the deployment's content listing, from [`content_digest`].
    pub content_digest: String,
}

impl Active {
    /// True when every platform being built already has a finished LOD on this entity.
    /// A pending or failed platform means there is nothing published to copy.
    pub fn lods_complete_for(&self, platforms: &[String]) -> bool {
        platforms.iter().all(|p| {
            self.lods
                .get(p)
                .and_then(serde_json::Value::as_str)
                .is_some_and(|s| s.eq_ignore_ascii_case("complete"))
        })
    }
}

/// The entity the registry currently serves for `pointers`. `None` when the registry has
/// no entry, which is the normal answer for a scene deployed for the first time.
pub fn active_entity(
    agent: &ureq::Agent,
    base: &str,
    pointers: &[String],
    world_name: Option<&str>,
) -> Result<Option<Active>> {
    if pointers.is_empty() {
        return Ok(None);
    }
    let mut url = format!("{}/entities/active", base.trim_end_matches('/'));
    if let Some(name) = world_name {
        url.push_str("?world_name=");
        url.push_str(&query_escape(name));
    }
    let body = serde_json::json!({ "pointers": pointers });
    let mut resp = agent
        .post(&url)
        .send_json(&body)
        .with_context(|| format!("POST {url}"))?;
    let text = resp
        .body_mut()
        .read_to_string()
        .with_context(|| format!("read {url}"))?;
    let parsed: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("parse {url}"))?;
    let Some(first) = parsed.as_array().and_then(|a| a.first()) else {
        return Ok(None);
    };
    let Some(entity_id) = first.get("id").and_then(serde_json::Value::as_str) else {
        return Ok(None);
    };
    let lods = first
        .pointer("/bundles/lods")
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default();
    Ok(Some(Active {
        entity_id: entity_id.to_string(),
        lods,
        content_digest: content_digest(first.get("content")),
    }))
}

/// Digest of a deployment's content listing: every `file -> hash` pair, sorted.
///
/// Two deployments with the same digest ship byte-identical files, so they also ship the
/// same scene code and the same `main.crdt` the runtime starts from, and the LOD build has
/// nothing left to differ on. That makes it a sound shortcut past deriving the geometry at
/// all, and it costs nothing: the registry already returns the previous deployment's
/// listing, and the new one's arrives with the entity.
///
/// It is deliberately strict rather than clever. A redeploy that only edits `scene.json`
/// changes the digest and falls through to the geometry comparison, which is the slower
/// path but still avoids the build.
pub fn content_digest(content: Option<&serde_json::Value>) -> String {
    let Some(entries) = content.and_then(serde_json::Value::as_array) else {
        return String::new();
    };
    let mut pairs: Vec<(&str, &str)> = entries
        .iter()
        .filter_map(|e| {
            Some((
                e.get("file").and_then(serde_json::Value::as_str)?,
                e.get("hash").and_then(serde_json::Value::as_str)?,
            ))
        })
        .collect();
    if pairs.is_empty() {
        return String::new();
    }
    pairs.sort_unstable();
    let mut buf = String::new();
    for (file, hash) in pairs {
        buf.push_str(file);
        buf.push('\0');
        buf.push_str(hash);
        buf.push('\n');
    }
    abgen::hashes::sha256_hex(buf.as_bytes())
}

/// Percent-escape a query value. World names are DNS-like today (`name.dcl.eth`) and pass
/// through untouched, but the name reaches us from a deployment and is not ours to trust.
fn query_escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for b in raw.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active(lods: serde_json::Value) -> Active {
        Active {
            entity_id: "bafkprev".to_string(),
            lods: lods.as_object().cloned().unwrap_or_default(),
            content_digest: String::new(),
        }
    }

    #[test]
    fn content_digest_ignores_order_and_notices_any_change() {
        let a = serde_json::json!([
            {"file": "bin/index.js", "hash": "bafkcode"},
            {"file": "main.crdt", "hash": "bafkcrdt"},
        ]);
        let reordered = serde_json::json!([
            {"file": "main.crdt", "hash": "bafkcrdt"},
            {"file": "bin/index.js", "hash": "bafkcode"},
        ]);
        assert_eq!(content_digest(Some(&a)), content_digest(Some(&reordered)));

        let changed_code = serde_json::json!([
            {"file": "bin/index.js", "hash": "bafkcode2"},
            {"file": "main.crdt", "hash": "bafkcrdt"},
        ]);
        assert_ne!(content_digest(Some(&a)), content_digest(Some(&changed_code)));

        let extra = serde_json::json!([
            {"file": "bin/index.js", "hash": "bafkcode"},
            {"file": "main.crdt", "hash": "bafkcrdt"},
            {"file": "models/tree.glb", "hash": "bafktree"},
        ]);
        assert_ne!(content_digest(Some(&a)), content_digest(Some(&extra)));

        // An absent or empty listing never matches, not even another empty one.
        assert_eq!(content_digest(None), "");
        assert_eq!(content_digest(Some(&serde_json::json!([]))), "");
    }

    #[test]
    fn query_escape_passes_world_names_and_escapes_the_rest() {
        assert_eq!(query_escape("skychaser.dcl.eth"), "skychaser.dcl.eth");
        assert_eq!(query_escape("a b&c=d"), "a%20b%26c%3Dd");
    }

    #[test]
    fn lods_complete_only_when_every_built_platform_is_complete() {
        let both = active(serde_json::json!({"mac": "complete", "windows": "complete"}));
        assert!(both.lods_complete_for(&["mac".into(), "windows".into()]));

        let one_pending = active(serde_json::json!({"mac": "complete", "windows": "pending"}));
        assert!(!one_pending.lods_complete_for(&["mac".into(), "windows".into()]));
        assert!(one_pending.lods_complete_for(&["mac".into()]));

        // A platform the registry says nothing about has nothing published to copy.
        let missing = active(serde_json::json!({"mac": "complete"}));
        assert!(!missing.lods_complete_for(&["mac".into(), "webgl".into()]));
    }
}

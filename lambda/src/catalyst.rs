//! Small catalyst fetch helpers for the pieces the Lambda needs beyond what
//! abgen fetches internally: the entity document (for its `type`, `content`
//! map and `metadata.main`) and scene source files re-published to the CDN.

use anyhow::{bail, Result};
use std::time::Duration;

const RETRIES: usize = 3;

pub fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(60)))
        .build()
        .into()
}

pub fn get_bytes(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>> {
    let mut last: Option<String> = None;
    for attempt in 0..RETRIES {
        match agent.get(url).call() {
            Ok(resp) => {
                let mut buf = Vec::new();
                use std::io::Read;
                resp.into_body().into_reader().read_to_end(&mut buf)?;
                return Ok(buf);
            }
            Err(ureq::Error::StatusCode(code)) => {
                if code == 404 {
                    bail!("404 {url}");
                }
                last = Some(format!("HTTP {code}"));
            }
            Err(e) => last = Some(e.to_string()),
        }
        std::thread::sleep(Duration::from_millis(300 * (attempt as u64 + 1)));
    }
    bail!("GET {url} failed: {}", last.unwrap_or_default())
}

/// Fetches the entity document: `{content_server}/contents/{entity_id}`.
pub fn fetch_entity(
    agent: &ureq::Agent,
    content_server: &str,
    entity_id: &str,
) -> Result<serde_json::Value> {
    let url = format!(
        "{}/contents/{entity_id}",
        content_server.trim_end_matches('/')
    );
    let bytes = get_bytes(agent, &url)?;
    Ok(serde_json::from_slice(&bytes)?)
}

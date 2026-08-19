use anyhow::{bail, Result};
use std::time::Duration;

const RETRIES: usize = 3;

pub fn agent() -> ureq::Agent {
    // max_redirects(0): the ALLOWED_CONTENT_SERVER_HOSTS check validates
    // only the URL we start from; following a cross-host 302 would bypass
    // it. Catalysts serve /contents directly today.
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(60)))
        .max_redirects(0)
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

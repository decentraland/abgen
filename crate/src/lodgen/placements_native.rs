use anyhow::{anyhow, Result};

use super::{ISS_MANIFEST_BASE, ISS_SUFFIX};

pub fn fetch_iss(scene_id: &str) -> Result<Option<Vec<u8>>> {
    let url = format!("{ISS_MANIFEST_BASE}/{scene_id}{ISS_SUFFIX}");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(120)))
        .build()
        .into();
    match agent
        .get(&url)
        .header("User-Agent", crate::catalyst::UA)
        .call()
    {
        Ok(resp) => {
            let mut buf = Vec::new();
            use std::io::Read;
            resp.into_body().into_reader().read_to_end(&mut buf)?;
            Ok(Some(buf))
        }
        Err(ureq::Error::StatusCode(404)) => Ok(None),
        Err(e) => Err(anyhow!("GET {url}: {e}")),
    }
}

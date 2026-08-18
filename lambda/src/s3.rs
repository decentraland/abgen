//! Blocking S3 `PutObject` over ureq with SigV4 signing.

use crate::aws;
use anyhow::{bail, Result};
use std::time::Duration;

const RETRIES: usize = 3;

pub struct S3Client {
    agent: ureq::Agent,
    host: String,
    base_url: String,
    /// `""` for virtual-hosted addressing; `/{bucket}` for path-style
    /// (custom endpoints — minio/localstack).
    path_prefix: String,
    region: String,
    creds: aws::Credentials,
    acl: Option<String>,
}

impl S3Client {
    pub fn new(
        bucket: &str,
        region: &str,
        endpoint_override: Option<&str>,
        acl: Option<&str>,
    ) -> Result<Self> {
        let creds = aws::Credentials::from_env()?;
        let (host, base_url, path_prefix) = match endpoint_override {
            Some(ep) => {
                let ep = ep.trim_end_matches('/');
                let host = ep
                    .strip_prefix("https://")
                    .or_else(|| ep.strip_prefix("http://"))
                    .unwrap_or(ep)
                    .to_string();
                (host, ep.to_string(), format!("/{bucket}"))
            }
            None => {
                let host = format!("{bucket}.s3.{region}.amazonaws.com");
                (host.clone(), format!("https://{host}"), String::new())
            }
        };
        Ok(S3Client {
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(300)))
                .build()
                .into(),
            host,
            base_url,
            path_prefix,
            region: region.to_string(),
            creds,
            acl: acl.map(str::to_string),
        })
    }

    pub fn put_object(
        &self,
        key: &str,
        body: &[u8],
        content_type: &str,
        cache_control: &str,
    ) -> Result<()> {
        let payload_sha = abgen::hashes::sha256_hex(body);
        let canonical_uri = format!("{}/{}", self.path_prefix, aws::uri_encode(key, false));
        let url = format!("{}{}", self.base_url, canonical_uri);

        let mut last: Option<String> = None;
        for attempt in 0..RETRIES {
            // Sign inside the loop: retries after a slow attempt must not
            // reuse a stale x-amz-date (requests older than 5 min are
            // rejected).
            let (amz_date, date) = aws::amz_date_now();
            let mut extra: Vec<(String, String)> = Vec::new();
            if let Some(acl) = &self.acl {
                extra.push(("x-amz-acl".to_string(), acl.clone()));
            }
            let auth = aws::authorization_header(
                &self.creds,
                &aws::SignParams {
                    method: "PUT",
                    host: &self.host,
                    canonical_uri: &canonical_uri,
                    canonical_query: "",
                    extra_headers: &extra,
                    payload_sha256_hex: &payload_sha,
                    service: "s3",
                    region: &self.region,
                    amz_date: &amz_date,
                    date: &date,
                },
            );

            let mut req = self
                .agent
                .put(&url)
                .header("Authorization", &auth)
                .header("x-amz-date", &amz_date)
                .header("x-amz-content-sha256", &payload_sha)
                .header("Content-Type", content_type)
                .header("Cache-Control", cache_control);
            if let Some(token) = &self.creds.session_token {
                req = req.header("x-amz-security-token", token);
            }
            if let Some(acl) = &self.acl {
                req = req.header("x-amz-acl", acl);
            }

            match req.send(body) {
                Ok(_) => return Ok(()),
                Err(ureq::Error::StatusCode(code)) => {
                    last = Some(format!("HTTP {code}"));
                    // 4xx (auth, missing bucket, blocked ACL) will not heal on
                    // retry — surface it immediately.
                    if (400..500).contains(&code) && code != 429 {
                        bail!("PUT s3://…/{key}: HTTP {code}");
                    }
                }
                Err(e) => last = Some(e.to_string()),
            }
            std::thread::sleep(Duration::from_millis(250 * (attempt as u64 + 1)));
        }
        bail!("PUT s3://…/{key} failed after {RETRIES} attempts: {}", last.unwrap_or_default())
    }
}

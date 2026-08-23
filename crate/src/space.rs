use crate::dates::{civil_from_days, days_from_civil};
use crate::Result;
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
pub(crate) struct ResolvedCreds {
    pub(crate) access_key: String,
    pub(crate) secret_key: String,
    pub(crate) session_token: Option<String>,
}

pub(crate) struct CachedCreds {
    creds: ResolvedCreds,
    expires_epoch: u64,
}

pub(crate) enum CredsSource {
    Static(ResolvedCreds),
    Container {
        url: String,
        auth_token: Option<String>,
        cache: Mutex<Option<CachedCreds>>,
    },
}

pub struct Space {
    pub scheme: String,

    pub host: String,

    pub region: String,

    pub bucket: Option<String>,

    pub path_style: bool,
    pub read_only: bool,
    creds: CredsSource,
}

pub(crate) fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(60)))
            .build()
            .into()
    })
}

pub(crate) fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

fn uri_encode_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for b in key.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
pub(crate) fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex(&h.finalize())
}
pub(crate) fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = HmacSha256::new_from_slice(key).expect("hmac key");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) fn timestamps() -> (String, String) {
    let secs = now_epoch();
    let days = (secs / 86_400) as i64;
    let sod = (secs % 86_400) as i64;
    let (h, mi, s) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    let (y, mo, d) = civil_from_days(days);
    let date = format!("{y:04}{mo:02}{d:02}");
    (date.clone(), format!("{date}T{h:02}{mi:02}{s:02}Z"))
}

fn parse_iso8601_epoch(s: &str) -> Option<u64> {
    if s.len() < 19 {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r).and_then(|t| t.parse::<i64>().ok());
    let y = num(0..4)?;
    let mo = num(5..7)?;
    let d = num(8..10)?;
    let h = num(11..13)?;
    let mi = num(14..16)?;
    let sec = num(17..19)?;
    let t = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec;
    u64::try_from(t).ok()
}

pub struct ObjectHeaders {
    pub content_type: &'static str,
    pub cache_control: &'static str,
    /// `Some("br")` for `.br` keys — `@dcl/cdn-uploader` stamps
    /// `ContentEncoding: 'br'` on every brotli variant it writes, and clients
    /// (and the abcdn edge, `serve.rs`) rely on the header to decode.
    pub content_encoding: Option<&'static str>,
}

/// Verbatim from what the production writers put on the same keys:
/// content-addressed keys are immutable forever; `manifest/…` is rewritten in
/// place on every rebuild so the origin must never let it be cached.
///
/// Two immutable spellings exist upstream, byte for byte: bundle-style keys go
/// through `@dcl/cdn-uploader`, whose `cacheHeader()` joins directives with a
/// bare comma, while scene source files are uploaded directly by
/// `scenes/component.ts` with a comma-space string. The `.br` variant adds
/// `no-transform` exactly where cdn-uploader does.
///
/// `lods-unity/manifests/…` is NOT a consumer-server manifest: its production
/// writer is lod-generator-unity's storage adapter, which uploads every file
/// with `CACHE_CONTROL_ONE_YEAR = 'public, max-age=31536000'` (the ISS
/// descriptor file names embed the content-addressed entity id). The abcdn
/// edge deliberately serves its JIT-regenerated copies no-cache; the S3
/// objects match the production writer.
const IMMUTABLE_BUNDLE: &str = "public,max-age=31536000,immutable";
const IMMUTABLE_BUNDLE_BR: &str = "public,no-transform,max-age=31536000,immutable";
const IMMUTABLE_SOURCE: &str = "public, max-age=31536000, immutable";
const NO_CACHE: &str = "private, max-age=0, no-cache";
const PUBLIC_ONE_YEAR: &str = "public, max-age=31536000";

/// Single source of truth for the metadata every upload carries.
pub fn object_headers(key: &str) -> ObjectHeaders {
    let base = key.strip_suffix(".br").unwrap_or(key);
    let is_br = base.len() != key.len();
    let lower = base.to_ascii_lowercase();
    let content_type = if lower.ends_with(".json") {
        "application/json"
    } else if lower.ends_with(".js") {
        "application/javascript"
    } else if lower.ends_with(".manifest") {
        "text/cache-manifest"
    } else if lower.ends_with(".pack") || lower.ends_with(".crdt") {
        "application/octet-stream"
    } else {
        "application/wasm"
    };
    // cdn-uploader lane = the bundle output dir (wasm + .manifest files) and
    // every `.br` sibling; the direct-upload lane is scene sources (.js/.json/
    // .crdt) which upstream never brotli-compresses.
    let uploader_lane = matches!(content_type, "application/wasm" | "text/cache-manifest");
    let cache_control = if key.starts_with("manifest/") {
        NO_CACHE
    } else if key.starts_with("lods-unity/manifests/") {
        PUBLIC_ONE_YEAR
    } else if is_br {
        IMMUTABLE_BUNDLE_BR
    } else if uploader_lane {
        IMMUTABLE_BUNDLE
    } else {
        IMMUTABLE_SOURCE
    };
    ObjectHeaders {
        content_type,
        cache_control,
        content_encoding: is_br.then_some("br"),
    }
}

fn warn_once_403(key: &str) {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(key = %key, "space GET 403, treating as miss; if unexpected check credentials and bucket policy (warned once)");
    }
}

impl CredsSource {
    /// Static env vars first, then the ECS container credential endpoint;
    /// shared by S3 here and SNS in [`crate::sns`].
    pub(crate) fn from_env() -> Option<CredsSource> {
        let first = |vars: &[&str]| {
            vars.iter()
                .find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()))
        };
        let static_creds = match (
            first(&["ABGEN_S3_ACCESS_KEY", "AWS_ACCESS_KEY_ID"]),
            first(&["ABGEN_S3_SECRET_KEY", "AWS_SECRET_ACCESS_KEY"]),
        ) {
            (Some(access_key), Some(secret_key)) => Some(ResolvedCreds {
                access_key,
                secret_key,
                session_token: first(&["ABGEN_S3_SESSION_TOKEN", "AWS_SESSION_TOKEN"]),
            }),
            _ => None,
        };
        Some(match static_creds {
            Some(c) => CredsSource::Static(c),
            None => {
                let url = first(&["AWS_CONTAINER_CREDENTIALS_FULL_URI"]).or_else(|| {
                    first(&["AWS_CONTAINER_CREDENTIALS_RELATIVE_URI"])
                        .map(|u| format!("http://169.254.170.2{u}"))
                })?;
                CredsSource::Container {
                    url,
                    auth_token: first(&["AWS_CONTAINER_AUTHORIZATION_TOKEN"]),
                    cache: Mutex::new(None),
                }
            }
        })
    }
}

impl Space {
    pub fn from_env() -> Option<Space> {
        let first = |vars: &[&str]| {
            vars.iter()
                .find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()))
        };
        let creds = CredsSource::from_env()?;

        let endpoint = first(&["ABGEN_S3_ENDPOINT"])?;
        let (scheme, host) = match endpoint.split_once("://") {
            Some((s, h)) => (s.to_string(), h.trim_end_matches('/').to_string()),
            None => (
                "https".to_string(),
                endpoint.trim_end_matches('/').to_string(),
            ),
        };

        let region =
            first(&["ABGEN_S3_REGION", "AWS_REGION"]).unwrap_or_else(|| "us-east-1".to_string());
        let bucket = std::env::var("ABGEN_S3_BUCKET")
            .ok()
            .filter(|s| !s.is_empty());
        let path_style = crate::clihelp::env_bool("ABGEN_S3_PATH_STYLE", false);
        let read_only = crate::clihelp::env_bool("ABGEN_S3_READ_ONLY", false);

        Some(Space {
            scheme,
            host,
            region,
            bucket,
            path_style,
            read_only,
            creds,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_static_creds(
        scheme: &str,
        host: &str,
        region: &str,
        bucket: Option<&str>,
        path_style: bool,
        read_only: bool,
        access_key: &str,
        secret_key: &str,
    ) -> Space {
        Space {
            scheme: scheme.to_string(),
            host: host.to_string(),
            region: region.to_string(),
            bucket: bucket.map(str::to_string),
            path_style,
            read_only,
            creds: CredsSource::Static(ResolvedCreds {
                access_key: access_key.to_string(),
                secret_key: secret_key.to_string(),
                session_token: None,
            }),
        }
    }

    pub fn creds_source(&self) -> &'static str {
        match &self.creds {
            CredsSource::Static(c) if c.session_token.is_some() => "static-env+session-token",
            CredsSource::Static(_) => "static-env",
            CredsSource::Container { .. } => "ecs-container-role",
        }
    }

    fn creds(&self) -> Result<ResolvedCreds> {
        self.creds.resolve()
    }
}

impl CredsSource {
    pub(crate) fn resolve(&self) -> Result<ResolvedCreds> {
        match self {
            CredsSource::Static(c) => Ok(c.clone()),
            CredsSource::Container {
                url,
                auth_token,
                cache,
            } => {
                let now = now_epoch();
                {
                    let guard = cache.lock().unwrap_or_else(|p| p.into_inner());
                    if let Some(c) = guard.as_ref() {
                        if now + 300 < c.expires_epoch {
                            return Ok(c.creds.clone());
                        }
                    }
                }
                let mut req = agent().get(url);
                if let Some(t) = auth_token {
                    req = req.header("Authorization", t);
                }
                let resp = req
                    .call()
                    .map_err(|e| crate::anyhow!("container credentials GET: {e}"))?;
                let mut buf = Vec::new();
                std::io::Read::read_to_end(&mut resp.into_body().into_reader(), &mut buf)?;
                let v: serde_json::Value = serde_json::from_slice(&buf)
                    .map_err(|e| crate::anyhow!("container credentials parse: {e}"))?;
                let field = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
                let creds = ResolvedCreds {
                    access_key: field("AccessKeyId")
                        .ok_or_else(|| crate::anyhow!("container credentials: no AccessKeyId"))?,
                    secret_key: field("SecretAccessKey").ok_or_else(|| {
                        crate::anyhow!("container credentials: no SecretAccessKey")
                    })?,
                    session_token: field("Token"),
                };
                let expires_epoch = field("Expiration")
                    .and_then(|e| parse_iso8601_epoch(&e))
                    .unwrap_or(now + 900);
                *cache.lock().unwrap_or_else(|p| p.into_inner()) = Some(CachedCreds {
                    creds: creds.clone(),
                    expires_epoch,
                });
                Ok(creds)
            }
        }
    }
}

impl Space {
    fn path(&self, key: &str) -> String {
        let encoded = uri_encode_key(key);
        match (self.path_style, &self.bucket) {
            (true, Some(b)) => format!("/{b}/{encoded}"),
            _ => format!("/{encoded}"),
        }
    }

    fn authorize(
        &self,
        c: &ResolvedCreds,
        method: &str,
        key: &str,
        payload_hash: &str,
        amz_date: &str,
        date: &str,
    ) -> String {
        let canonical_uri = self.path(key);
        let (signed_headers, canonical_headers) = match &c.session_token {
            Some(token) => (
                "host;x-amz-content-sha256;x-amz-date;x-amz-security-token",
                format!(
                    "host:{}\nx-amz-content-sha256:{}\nx-amz-date:{}\nx-amz-security-token:{}\n",
                    self.host, payload_hash, amz_date, token
                ),
            ),
            None => (
                "host;x-amz-content-sha256;x-amz-date",
                format!(
                    "host:{}\nx-amz-content-sha256:{}\nx-amz-date:{}\n",
                    self.host, payload_hash, amz_date
                ),
            ),
        };
        let canonical_request = format!(
            "{method}\n{canonical_uri}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
        );
        let scope = format!("{date}/{}/s3/aws4_request", self.region);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            sha256_hex(canonical_request.as_bytes())
        );
        let k_date = hmac(format!("AWS4{}", c.secret_key).as_bytes(), date.as_bytes());
        let k_region = hmac(&k_date, self.region.as_bytes());
        let k_service = hmac(&k_region, b"s3");
        let k_signing = hmac(&k_service, b"aws4_request");
        let signature = hex(&hmac(&k_signing, string_to_sign.as_bytes()));
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            c.access_key
        )
    }

    fn call_get(
        &self,
        key: &str,
    ) -> Result<std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error>> {
        let c = self.creds()?;
        let payload_hash = sha256_hex(b"");
        let (date, amz) = timestamps();
        let auth = self.authorize(&c, "GET", key, &payload_hash, &amz, &date);
        let url = format!("{}://{}{}", self.scheme, self.host, self.path(key));
        let mut req = agent()
            .get(&url)
            .header("x-amz-date", &amz)
            .header("x-amz-content-sha256", &payload_hash)
            .header("Authorization", &auth);
        if let Some(token) = &c.session_token {
            req = req.header("x-amz-security-token", token);
        }
        Ok(req.call())
    }

    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        match self.call_get(key)? {
            Ok(r) => {
                let mut buf = Vec::new();
                std::io::Read::read_to_end(&mut r.into_body().into_reader(), &mut buf)?;
                Ok(Some(buf))
            }
            Err(ureq::Error::StatusCode(404)) => Ok(None),
            Err(ureq::Error::StatusCode(403)) => {
                warn_once_403(key);
                Ok(None)
            }
            Err(e) => Err(crate::anyhow!("space GET {key}: {e}")),
        }
    }

    pub fn head(&self, key: &str) -> Result<bool> {
        let c = self.creds()?;
        let payload_hash = sha256_hex(b"");
        let (date, amz) = timestamps();
        let auth = self.authorize(&c, "HEAD", key, &payload_hash, &amz, &date);
        let url = format!("{}://{}{}", self.scheme, self.host, self.path(key));
        let mut req = agent()
            .head(&url)
            .header("x-amz-date", &amz)
            .header("x-amz-content-sha256", &payload_hash)
            .header("Authorization", &auth);
        if let Some(token) = &c.session_token {
            req = req.header("x-amz-security-token", token);
        }
        match req.call() {
            Ok(r) => Ok(r.status().as_u16() == 200),
            Err(ureq::Error::StatusCode(404)) => Ok(false),
            Err(ureq::Error::StatusCode(403)) => {
                warn_once_403(key);
                Ok(false)
            }
            Err(e) => Err(crate::anyhow!("space HEAD {key}: {e}")),
        }
    }

    pub fn get_status(&self, key: &str) -> Result<u16> {
        match self.call_get(key)? {
            Ok(r) => Ok(r.status().as_u16()),
            Err(ureq::Error::StatusCode(code)) => Ok(code),
            Err(e) => Err(crate::anyhow!("space GET {key}: {e}")),
        }
    }

    pub fn put(&self, key: &str, body: &[u8]) -> Result<()> {
        if self.read_only {
            return Err(crate::anyhow!(
                "space is read-only (ABGEN_S3_READ_ONLY): refusing PUT {key}"
            ));
        }
        let headers = object_headers(key);
        let c = self.creds()?;
        let payload_hash = sha256_hex(body);
        let (date, amz) = timestamps();
        let auth = self.authorize(&c, "PUT", key, &payload_hash, &amz, &date);
        let url = format!("{}://{}{}", self.scheme, self.host, self.path(key));
        let mut req = agent()
            .put(&url)
            .header("x-amz-date", &amz)
            .header("x-amz-content-sha256", &payload_hash)
            .header("Authorization", &auth)
            .header("Content-Type", headers.content_type)
            .header("Cache-Control", headers.cache_control);
        if let Some(encoding) = headers.content_encoding {
            req = req.header("Content-Encoding", encoding);
        }
        if let Some(token) = &c.session_token {
            req = req.header("x-amz-security-token", token);
        }
        req.send(body)
            .map_err(|e| crate::anyhow!("space PUT {key}: {e}"))?;
        Ok(())
    }
}

/// Read-only S3 source for content-addressed catalyst/worlds bytes, spliced
/// into [`crate::catalyst::CatalystClient::fetch_content`] ahead of the HTTP
/// primary. Bucket layout is unverified (see space.rs module docs on
/// `ABGEN_CONTENT_S3_*`); the prefix is configurable so operators can point
/// it at whatever the real mirror turns out to be.
pub struct S3ContentSource {
    space: Space,
    prefix: String,
}

fn normalize_prefix(raw: &str) -> String {
    let trimmed = raw.trim().trim_matches('/');
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}/")
    }
}

/// Pure endpoint resolution, split out of [`S3ContentSource::from_env`] so it
/// can be unit tested without touching process env vars: with no explicit
/// endpoint, default to the regional AWS host with path-style forced on
/// (matches prod's `ABGEN_S3_*` pattern); an explicit endpoint keeps whatever
/// path-style the caller asks for (minio/mirror stacks).
fn resolve_endpoint(
    explicit_endpoint: Option<&str>,
    region: &str,
    path_style_if_explicit: bool,
) -> (String, String, bool) {
    match explicit_endpoint {
        Some(endpoint) => {
            let (scheme, host) = match endpoint.split_once("://") {
                Some((s, h)) => (s.to_string(), h.trim_end_matches('/').to_string()),
                None => (
                    "https".to_string(),
                    endpoint.trim_end_matches('/').to_string(),
                ),
            };
            (scheme, host, path_style_if_explicit)
        }
        None => (
            "https".to_string(),
            format!("s3.{region}.amazonaws.com"),
            true,
        ),
    }
}

impl S3ContentSource {
    /// `None` unless `ABGEN_CONTENT_S3_BUCKET` is set — the feature is off by
    /// default. The inner [`Space`] is always constructed `read_only: true`
    /// regardless of `ABGEN_S3_READ_ONLY`: this source must never PUT.
    pub fn from_env() -> Option<S3ContentSource> {
        let first = |vars: &[&str]| {
            vars.iter()
                .find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()))
        };
        let bucket = std::env::var("ABGEN_CONTENT_S3_BUCKET")
            .ok()
            .filter(|s| !s.is_empty())?;
        let creds = CredsSource::from_env()?;
        let region = first(&["ABGEN_CONTENT_S3_REGION", "ABGEN_S3_REGION", "AWS_REGION"])
            .unwrap_or_else(|| "us-east-1".to_string());
        let path_style_if_explicit = crate::clihelp::env_bool("ABGEN_S3_PATH_STYLE", false);
        let (scheme, host, path_style) = resolve_endpoint(
            first(&["ABGEN_CONTENT_S3_ENDPOINT"]).as_deref(),
            &region,
            path_style_if_explicit,
        );
        let prefix = normalize_prefix(
            std::env::var("ABGEN_CONTENT_S3_PREFIX")
                .ok()
                .as_deref()
                .unwrap_or(""),
        );
        Some(S3ContentSource {
            space: Space {
                scheme,
                host,
                region,
                bucket: Some(bucket),
                path_style,
                read_only: true,
                creds,
            },
            prefix,
        })
    }

    /// Test/tooling injection point, mirroring [`Space::with_static_creds`].
    /// `read_only` is always forced `true` on the inner [`Space`], matching
    /// [`S3ContentSource::from_env`].
    #[allow(clippy::too_many_arguments)]
    pub fn with_static_creds(
        scheme: &str,
        host: &str,
        region: &str,
        bucket: Option<&str>,
        path_style: bool,
        prefix: &str,
        access_key: &str,
        secret_key: &str,
    ) -> S3ContentSource {
        S3ContentSource {
            space: Space::with_static_creds(
                scheme, host, region, bucket, path_style, true, access_key, secret_key,
            ),
            prefix: normalize_prefix(prefix),
        }
    }

    fn key(&self, hash: &str) -> String {
        format!("{}{hash}", self.prefix)
    }

    /// `Ok(None)` on a clean miss (404/403, folded by [`Space::get`]) so the
    /// caller falls through to HTTP; `Ok(Some(bytes))` with an empty payload
    /// is treated the same as a miss.
    pub fn fetch(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        let key = self.key(hash);
        let t = std::time::Instant::now();
        let r = self.space.get(&key);
        let result = match &r {
            Ok(Some(b)) if !b.is_empty() => "hit",
            Ok(_) => "miss",
            Err(_) => "error",
        };
        metrics::histogram!("abgen_content_s3_request_duration_seconds", "result" => result)
            .record(t.elapsed().as_secs_f64());
        metrics::counter!("abgen_content_s3_requests_total", "result" => result).increment(1);
        if let Ok(Some(b)) = &r {
            if !b.is_empty() {
                metrics::counter!("abgen_content_s3_bytes_total").increment(b.len() as u64);
            }
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_space(read_only: bool, session_token: Option<&str>) -> Space {
        Space {
            scheme: "https".to_string(),
            host: "bucket.example.com".to_string(),
            region: "us-east-1".to_string(),
            bucket: Some("bucket".to_string()),
            path_style: false,
            read_only,
            creds: CredsSource::Static(ResolvedCreds {
                access_key: "AKIATEST".to_string(),
                secret_key: "secret".to_string(),
                session_token: session_token.map(str::to_string),
            }),
        }
    }

    #[test]
    fn session_token_changes_signed_headers() {
        let with = test_space(false, Some("tok123"));
        let without = test_space(false, None);
        let hash = sha256_hex(b"");
        let cw = match &with.creds {
            CredsSource::Static(c) => c.clone(),
            _ => unreachable!(),
        };
        let co = match &without.creds {
            CredsSource::Static(c) => c.clone(),
            _ => unreachable!(),
        };
        let a1 = with.authorize(&cw, "GET", "k", &hash, "20260101T000000Z", "20260101");
        let a2 = without.authorize(&co, "GET", "k", &hash, "20260101T000000Z", "20260101");
        assert!(a1.contains("x-amz-security-token"));
        assert!(!a2.contains("x-amz-security-token"));
        assert_ne!(a1, a2);
    }

    #[test]
    fn read_only_put_refuses_without_network() {
        let s = test_space(true, None);
        let err = s.put("k", b"x").unwrap_err();
        assert!(err.to_string().contains("read-only"));
    }

    fn capture_one_request(
        listener: std::net::TcpListener,
        out: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) -> std::thread::JoinHandle<()> {
        use std::io::{BufRead, BufReader, Read, Write};
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut head: Vec<String> = Vec::new();
            let mut content_len = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    break;
                }
                let t = line.trim_end().to_string();
                if t.is_empty() {
                    break;
                }
                if let Some(v) = t.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_len = v.trim().parse().unwrap_or(0);
                }
                head.push(t);
            }
            if content_len > 0 {
                let mut body = vec![0u8; content_len];
                let _ = reader.read_exact(&mut body);
            }
            out.lock().unwrap().push(head.join("\n"));
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            let _ = stream.flush();
        })
    }

    #[test]
    fn put_signs_every_amz_header_it_sends() {
        for token in [None, Some("tok123")] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let handle = capture_one_request(listener, captured.clone());
            let mut s = test_space(false, token);
            s.scheme = "http".to_string();
            s.host = addr.to_string();
            s.put("v41/cid/Qmhash_windows", b"body").unwrap();
            handle.join().unwrap();
            let head = captured.lock().unwrap().join("\n");
            let amz_sent: Vec<String> = head
                .lines()
                .filter_map(|l| {
                    let (name, _) = l.split_once(':')?;
                    let n = name.trim().to_ascii_lowercase();
                    n.starts_with("x-amz-").then_some(n)
                })
                .collect();
            assert!(!amz_sent.is_empty(), "{head}");
            assert!(!amz_sent.contains(&"x-amz-acl".to_string()), "{head}");
            let auth = head
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                .unwrap();
            let signed: Vec<&str> = auth
                .split("SignedHeaders=")
                .nth(1)
                .unwrap()
                .split(',')
                .next()
                .unwrap()
                .split(';')
                .collect();
            for h in &amz_sent {
                assert!(signed.contains(&h.as_str()), "unsigned {h} in {auth}");
            }
        }
    }

    #[test]
    fn object_headers_match_production_ab_cdn() {
        for (key, ct, cc, ce) in [
            (
                "v41/bafkScene/Qmhash_windows",
                "application/wasm",
                IMMUTABLE_BUNDLE,
                None,
            ),
            (
                "v41/assets/Qmhash_mac",
                "application/wasm",
                IMMUTABLE_BUNDLE,
                None,
            ),
            (
                "v41/dcl/scene_ignore_windows",
                "application/wasm",
                IMMUTABLE_BUNDLE,
                None,
            ),
            (
                "LOD/1/bafkscene_1_windows",
                "application/wasm",
                IMMUTABLE_BUNDLE,
                None,
            ),
            (
                "v41/bafkScene/Qmhash_windows.br",
                "application/wasm",
                IMMUTABLE_BUNDLE_BR,
                Some("br"),
            ),
            (
                "LOD/0/bafkscene_0_mac.br",
                "application/wasm",
                IMMUTABLE_BUNDLE_BR,
                Some("br"),
            ),
            (
                "manifest/bafkEntity_windows.json",
                "application/json",
                NO_CACHE,
                None,
            ),
            // Production writer of this family is lod-generator-unity's
            // storage adapter: CACHE_CONTROL_ONE_YEAR = 'public, max-age=31536000'.
            (
                "lods-unity/manifests/bafkscene_InitialSceneState.json",
                "application/json",
                PUBLIC_ONE_YEAR,
                None,
            ),
            (
                "lods-unity/manifests/bafkscene_InitialSceneState.json.br",
                "application/json",
                PUBLIC_ONE_YEAR,
                Some("br"),
            ),
            (
                "v41/bafkScene/scene.json",
                "application/json",
                IMMUTABLE_SOURCE,
                None,
            ),
            (
                "v41/bafkScene/bin/game.js",
                "application/javascript",
                IMMUTABLE_SOURCE,
                None,
            ),
            (
                "v41/bafkScene/main.crdt",
                "application/octet-stream",
                IMMUTABLE_SOURCE,
                None,
            ),
            (
                "bvwebgpu/p0/bafkScene.pack",
                "application/octet-stream",
                IMMUTABLE_SOURCE,
                None,
            ),
            (
                "bvwebgpu/p0/bafkScene.pack.br",
                "application/octet-stream",
                IMMUTABLE_BUNDLE_BR,
                Some("br"),
            ),
            (
                "v41/bafkScene/Qmhash_windows.manifest",
                "text/cache-manifest",
                IMMUTABLE_BUNDLE,
                None,
            ),
        ] {
            let h = object_headers(key);
            assert_eq!(h.content_type, ct, "content type for {key}");
            assert_eq!(h.cache_control, cc, "cache control for {key}");
            assert_eq!(h.content_encoding, ce, "content encoding for {key}");
        }
    }

    #[test]
    fn put_sends_key_derived_content_type_and_cache_control() {
        for (key, ct, cc, ce) in [
            (
                "v41/cid/Qmhash_windows",
                "application/wasm",
                IMMUTABLE_BUNDLE,
                None,
            ),
            (
                "manifest/cid_windows.json",
                "application/json",
                NO_CACHE,
                None,
            ),
            // cdn-uploader stamps ContentEncoding: 'br' on every .br variant.
            (
                "LOD/1/bafkscene_1_windows.br",
                "application/wasm",
                IMMUTABLE_BUNDLE_BR,
                Some("br"),
            ),
        ] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let handle = capture_one_request(listener, captured.clone());
            let mut s = test_space(false, None);
            s.scheme = "http".to_string();
            s.host = addr.to_string();
            s.put(key, b"body").unwrap();
            handle.join().unwrap();
            let head = captured.lock().unwrap().join("\n");
            let value = |name: &str| {
                head.lines()
                    .find(|l| l.to_ascii_lowercase().starts_with(&format!("{name}:")))
                    .and_then(|l| l.split_once(':'))
                    .map(|(_, v)| v.trim().to_string())
            };
            assert_eq!(value("content-type").as_deref(), Some(ct), "{head}");
            assert_eq!(value("cache-control").as_deref(), Some(cc), "{head}");
            assert_eq!(value("content-encoding").as_deref(), ce, "{head}");
        }
    }

    #[test]
    fn uri_encoding_noop_for_existing_key_shapes() {
        for key in [
            "manifest/bafkEntity_windows.json",
            "v41/bafkScene/Qmhash_mac.br",
            "LOD/1/bafkscene_1_windows",
            "lods-unity/manifests/bafkscene_InitialSceneState.json",
            "v41/dcl/scene_ignore_windows",
        ] {
            assert_eq!(uri_encode_key(key), key);
        }
        let s = test_space(false, None);
        assert_eq!(
            s.path("v41/bafkScene/Qmhash_mac"),
            "/v41/bafkScene/Qmhash_mac"
        );
    }

    #[test]
    fn uri_encoding_escapes_spaces_in_canonical_and_url() {
        let key = "v41/dcl/universal render pipeline/lit_ignore_windows";
        assert_eq!(
            uri_encode_key(key),
            "v41/dcl/universal%20render%20pipeline/lit_ignore_windows"
        );
        let s = test_space(false, None);
        assert_eq!(
            s.path(key),
            "/v41/dcl/universal%20render%20pipeline/lit_ignore_windows"
        );
        let ps = Space::with_static_creds(
            "https",
            "s3.example.com",
            "us-east-1",
            Some("bkt"),
            true,
            false,
            "AKIATEST",
            "secret",
        );
        assert_eq!(
            ps.path(key),
            "/bkt/v41/dcl/universal%20render%20pipeline/lit_ignore_windows"
        );
        let c = match &ps.creds {
            CredsSource::Static(c) => c.clone(),
            _ => unreachable!(),
        };
        let hash = sha256_hex(b"");
        let a1 = ps.authorize(&c, "GET", key, &hash, "20260101T000000Z", "20260101");
        let a2 = ps.authorize(
            &c,
            "GET",
            "v41/dcl/other shader",
            &hash,
            "20260101T000000Z",
            "20260101",
        );
        assert_ne!(a1, a2);
    }

    #[test]
    fn iso8601_epoch_roundtrips_with_civil_days() {
        assert_eq!(parse_iso8601_epoch("1970-01-01T00:00:00Z"), Some(0));
        for days in [0i64, 19_723, 20_644, 25_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        let e = parse_iso8601_epoch("2026-07-10T01:02:03Z").unwrap();
        assert_eq!(e % 86_400, 3600 + 2 * 60 + 3);
        let (y, m, d) = civil_from_days((e / 86_400) as i64);
        assert_eq!((y, m, d), (2026, 7, 10));
    }

    #[test]
    fn content_source_prefix_normalization() {
        for (raw, want) in [
            ("", ""),
            ("   ", ""),
            ("contents", "contents/"),
            ("contents/", "contents/"),
            ("/contents", "contents/"),
            ("/contents/", "contents/"),
            ("a/b", "a/b/"),
        ] {
            assert_eq!(normalize_prefix(raw), want, "prefix {raw:?}");
        }
    }

    #[test]
    fn content_source_resolve_endpoint_defaults_and_overrides() {
        assert_eq!(
            resolve_endpoint(None, "us-east-1", false),
            (
                "https".to_string(),
                "s3.us-east-1.amazonaws.com".to_string(),
                true
            ),
            "no explicit endpoint: regional AWS host, path-style forced on"
        );
        assert_eq!(
            resolve_endpoint(None, "eu-west-1", true),
            (
                "https".to_string(),
                "s3.eu-west-1.amazonaws.com".to_string(),
                true
            ),
            "path_style_if_explicit is irrelevant without an explicit endpoint"
        );
        assert_eq!(
            resolve_endpoint(Some("http://minio:9000"), "us-east-1", true),
            ("http".to_string(), "minio:9000".to_string(), true)
        );
        assert_eq!(
            resolve_endpoint(Some("minio:9000/"), "us-east-1", false),
            ("https".to_string(), "minio:9000".to_string(), false),
            "no scheme in explicit endpoint defaults to https; trailing slash trimmed"
        );
    }

    /// Every sub-case below keeps `ABGEN_CONTENT_S3_ENDPOINT` pinned at a
    /// loopback address whenever `ABGEN_CONTENT_S3_BUCKET` is set: this test
    /// mutates real, process-wide env var names that `CatalystClient::new`'s
    /// process-wide `OnceLock` also reads (via `S3ContentSource::from_env`)
    /// on whatever thread first constructs a client anywhere in this test
    /// binary. If that race ever lands mid-test, the worst case must be an
    /// instant loopback connection-refused (safe, harmless) rather than a
    /// real outbound call to `s3.<region>.amazonaws.com` with test creds,
    /// which could hang other, unrelated tests.
    #[test]
    fn content_source_from_env_bucket_and_creds_gate_and_wire_fields() {
        let vars = [
            "ABGEN_CONTENT_S3_BUCKET",
            "ABGEN_CONTENT_S3_PREFIX",
            "ABGEN_CONTENT_S3_REGION",
            "ABGEN_CONTENT_S3_ENDPOINT",
            "ABGEN_S3_REGION",
            "ABGEN_S3_PATH_STYLE",
            "ABGEN_S3_ACCESS_KEY",
            "ABGEN_S3_SECRET_KEY",
            "AWS_REGION",
        ];
        let saved: Vec<(&str, Option<String>)> =
            vars.iter().map(|v| (*v, std::env::var(v).ok())).collect();
        for v in vars {
            std::env::remove_var(v);
        }

        assert!(
            S3ContentSource::from_env().is_none(),
            "no bucket -> feature disabled"
        );

        std::env::set_var("ABGEN_CONTENT_S3_ENDPOINT", "http://127.0.0.1:1");
        std::env::set_var("ABGEN_CONTENT_S3_BUCKET", "test-bucket");
        assert!(
            S3ContentSource::from_env().is_none(),
            "bucket without creds -> feature disabled"
        );

        std::env::set_var("ABGEN_S3_ACCESS_KEY", "AKIATEST");
        std::env::set_var("ABGEN_S3_SECRET_KEY", "secret");
        let s = S3ContentSource::from_env().expect("bucket + creds -> enabled");
        assert_eq!(s.space.bucket.as_deref(), Some("test-bucket"));
        assert_eq!(s.space.scheme, "http");
        assert_eq!(s.space.host, "127.0.0.1:1");
        assert!(
            s.space.read_only,
            "content source must never be able to PUT"
        );
        assert_eq!(s.space.region, "us-east-1", "default region");
        assert_eq!(s.prefix, "", "default prefix is the bare-hash layout");
        assert!(
            !s.space.path_style,
            "explicit endpoint + unset ABGEN_S3_PATH_STYLE stays virtual-hosted"
        );

        std::env::set_var("ABGEN_S3_PATH_STYLE", "true");
        assert!(S3ContentSource::from_env().unwrap().space.path_style);
        std::env::remove_var("ABGEN_S3_PATH_STYLE");

        std::env::set_var("ABGEN_CONTENT_S3_PREFIX", "contents/");
        assert_eq!(S3ContentSource::from_env().unwrap().prefix, "contents/");
        std::env::set_var("ABGEN_CONTENT_S3_PREFIX", "/contents");
        assert_eq!(S3ContentSource::from_env().unwrap().prefix, "contents/");
        std::env::remove_var("ABGEN_CONTENT_S3_PREFIX");

        std::env::set_var("AWS_REGION", "sa-east-1");
        assert_eq!(
            S3ContentSource::from_env().unwrap().space.region,
            "sa-east-1"
        );
        std::env::set_var("ABGEN_S3_REGION", "eu-west-1");
        assert_eq!(
            S3ContentSource::from_env().unwrap().space.region,
            "eu-west-1",
            "ABGEN_S3_REGION beats AWS_REGION"
        );
        std::env::set_var("ABGEN_CONTENT_S3_REGION", "ap-south-1");
        assert_eq!(
            S3ContentSource::from_env().unwrap().space.region,
            "ap-south-1",
            "ABGEN_CONTENT_S3_REGION beats ABGEN_S3_REGION"
        );

        for (k, v) in saved {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
    }
}

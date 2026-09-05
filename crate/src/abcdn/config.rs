use anyhow::{Context, Result};
use std::env;

pub const DEFAULT_ABGEN_OUT_ROOT: &str = "./data/ab-generator/out";

pub struct Config {
    pub http_host: String,
    pub http_port: u16,
    pub abgen_out_root: String,

    pub content_url: String,

    pub content_disk: Option<String>,

    pub live_cache_dir: String,

    pub live_version: String,

    pub manifest_content_server_url: String,

    pub abgen_root: Option<String>,
    pub content_database_url: Option<String>,

    pub jit_content_digest: bool,

    pub upstream_ab_cdn: Option<String>,

    pub upstream_ab_registry: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let content_url = env::var("ABGEN_CATALYST_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| {
                tracing::warn!(
                    "ABGEN_CATALYST_URL unset or blank — assuming a local catalyst at \
                     http://127.0.0.1:5141/content; JIT conversion will fail if nothing \
                     listens there (set it to e.g. https://peer.decentraland.org/content)"
                );
                "http://127.0.0.1:5141/content".to_string()
            });

        let upstream_ab_cdn = env::var("ABGEN_UPSTREAM_AB_CDN")
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty());

        let jit_content_digest = match env::var("ABGEN_JIT_CONTENT_DIGEST") {
            Ok(_) => crate::clihelp::env_bool("ABGEN_JIT_CONTENT_DIGEST", false),
            Err(_) => is_loopback_url(&content_url),
        };

        Ok(Self {
            http_host: env::var("ABGEN_HTTP_HOST")
                .or_else(|_| env::var("HTTP_SERVER_HOST"))
                .unwrap_or_else(|_| "127.0.0.1".to_string()),
            http_port: get_port("ABGEN_HTTP_PORT", get_port("HTTP_SERVER_PORT", 5147)?)?,
            abgen_out_root: env::var("ABGEN_OUT_ROOT")
                .unwrap_or_else(|_| DEFAULT_ABGEN_OUT_ROOT.to_string()),
            content_url,
            content_disk: env::var("ABGEN_CONTENT_DISK")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            live_cache_dir: env::var("ABGEN_CACHE_DIR")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "./abgen-serve-cache".to_string()),
            live_version: env::var("ABGEN_VERSION")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "v49".to_string()),
            manifest_content_server_url: env::var("ABGEN_MANIFEST_CONTENT_SERVER_URL")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| crate::manifest::DEFAULT_CONTENT_SERVER_URL.to_string()),
            abgen_root: env::var("ABGEN_ROOT")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            content_database_url: content_connection_string(),
            jit_content_digest,
            upstream_ab_cdn: upstream_ab_cdn.clone(),
            upstream_ab_registry: match env::var("ABGEN_UPSTREAM_AB_REGISTRY")
                .ok()
                .map(|s| s.trim().trim_end_matches('/').to_string())
            {
                Some(s) if s.eq_ignore_ascii_case("off") => None,
                Some(s) if !s.is_empty() => Some(s),
                _ => upstream_ab_cdn.as_deref().and_then(registry_for_ab_cdn),
            },
        })
    }
}

/// The registry that describes an ab-cdn's content sits beside it on the same domain
/// (`https://ab-cdn.<domain>` -> `https://asset-bundle-registry.<domain>`), so any CDN of that
/// shape gets its registry derived. A gateway path, a loopback sidecar or a bucket host has none
/// to infer and needs ABGEN_UPSTREAM_AB_REGISTRY set explicitly.
fn registry_for_ab_cdn(cdn: &str) -> Option<String> {
    let (scheme, authority) = cdn.split_once("://")?;
    if authority.contains('/') {
        return None;
    }
    let domain = authority.to_ascii_lowercase();
    let domain = domain.strip_prefix("ab-cdn.")?;
    if domain.is_empty() {
        return None;
    }
    Some(format!("{scheme}://asset-bundle-registry.{domain}"))
}
fn content_connection_string() -> Option<String> {
    if let Ok(url) = env::var("CONTENT_PG_CONNECTION_STRING") {
        if !url.trim().is_empty() {
            return Some(url);
        }
    }
    let user = env::var("POSTGRES_CONTENT_USER")
        .ok()
        .filter(|s| !s.is_empty())?;
    let host = env::var("POSTGRES_HOST").unwrap_or_else(|_| "./data/run".into());
    let port = env::var("POSTGRES_PORT").unwrap_or_else(|_| "6432".into());
    let password = env::var("POSTGRES_CONTENT_PASSWORD").unwrap_or_default();
    let db = env::var("POSTGRES_CONTENT_DB").unwrap_or_else(|_| "content".into());
    Some(format!(
        "postgresql:///{}?host={}&port={}&user={}&password={}",
        pct(&db),
        pct(&host),
        port,
        pct(&user),
        pct(&password),
    ))
}
fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn get_port(key: &str, default: u16) -> Result<u16> {
    match env::var(key) {
        Ok(s) => s.parse::<u16>().with_context(|| format!("invalid {}", key)),
        Err(_) => Ok(default),
    }
}

fn is_loopback_url(url: &str) -> bool {
    let rest = match url.split_once("://") {
        Some((_, rest)) => rest,
        None => url,
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    let host = authority
        .strip_prefix('[')
        .map(|h| h.split(']').next().unwrap_or(h))
        .unwrap_or_else(|| authority.split(':').next().unwrap_or(authority));
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{is_loopback_url, registry_for_ab_cdn};

    #[test]
    fn registry_is_derived_beside_any_ab_cdn() {
        assert_eq!(
            registry_for_ab_cdn("https://ab-cdn.decentraland.org").as_deref(),
            Some("https://asset-bundle-registry.decentraland.org")
        );
        assert_eq!(
            registry_for_ab_cdn("https://ab-cdn.decentraland.zone").as_deref(),
            Some("https://asset-bundle-registry.decentraland.zone")
        );
        assert_eq!(
            registry_for_ab_cdn("https://ab-cdn.example.org").as_deref(),
            Some("https://asset-bundle-registry.example.org")
        );
        assert_eq!(
            registry_for_ab_cdn("https://AB-CDN.Example.org").as_deref(),
            Some("https://asset-bundle-registry.example.org")
        );
        assert_eq!(
            registry_for_ab_cdn("http://ab-cdn.localhost:5133").as_deref(),
            Some("http://asset-bundle-registry.localhost:5133")
        );
    }

    #[test]
    fn no_registry_is_inferred_for_other_cdn_shapes() {
        assert_eq!(
            registry_for_ab_cdn("https://gateway.decentraland.org/ab-cdn"),
            None
        );
        assert_eq!(registry_for_ab_cdn("http://127.0.0.1:5147"), None);
        assert_eq!(
            registry_for_ab_cdn("https://ab-cdn.decentraland.org/v49"),
            None
        );
        assert_eq!(registry_for_ab_cdn("ab-cdn.decentraland.org"), None);
        assert_eq!(registry_for_ab_cdn("https://ab-cdn."), None);
    }

    #[test]
    fn loopback_detection_for_revalidation_default() {
        assert!(is_loopback_url("http://127.0.0.1:8000/content"));
        assert!(is_loopback_url("http://localhost:8000/content"));
        assert!(is_loopback_url("http://[::1]:8000/content"));
        assert!(!is_loopback_url("https://peer.decentraland.org/content"));
        assert!(!is_loopback_url(
            "https://worlds-content-server.decentraland.org"
        ));
        assert!(!is_loopback_url("http://127.evil.example.com/content"));
    }
}

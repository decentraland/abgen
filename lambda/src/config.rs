//! Environment-driven configuration, read once at startup.

use std::path::PathBuf;

pub struct Config {
    /// Platforms built per entity, in order. First one pays for the texture
    /// encodes; the rest hit the cache. `PLATFORMS`, default `windows,mac`.
    pub platforms: Vec<String>,
    /// Asset-bundle version tag baked into manifests and S3 key prefixes.
    /// `AB_VERSION`, default `v49`.
    pub version: String,
    /// abgen's content/download cache. On Lambda point this at `/tmp` storage
    /// so warm containers reuse downloads. `ABGEN_CACHE_DIR`.
    pub cache_dir: String,
    /// Content server used when the event does not carry one.
    /// `CONTENT_SERVER_URL`, default the foundation catalyst.
    pub default_content_server: String,
    /// Where conversion output lands locally before upload (and where it stays
    /// in `--once` runs for inspection). `OUT_ROOT`.
    pub out_root: PathBuf,
    /// Keep the local corpus after publishing (`KEEP_OUTPUT=1`; always on for
    /// `--once` runs so the output can be inspected).
    ///
    /// S3 itself is configured through abgen's space env: `ABGEN_S3_ENDPOINT`
    /// (required to enable), `ABGEN_S3_BUCKET`, `ABGEN_S3_REGION`,
    /// `ABGEN_S3_PATH_STYLE`, `ABGEN_S3_READ_ONLY`; credentials from the
    /// standard AWS env / container role.
    pub keep_output: bool,
    /// asset-bundle-registry queue. `REGISTRY_QUEUE_URL`. TODO(step 4).
    pub registry_queue_url: Option<String>,
    /// SSRF guard (`ALLOWED_CONTENT_SERVER_HOSTS`, comma-separated): when
    /// set, a job's content server must be https with an exactly-matching
    /// host, or the job is rejected. Unset (local runs) = unrestricted;
    /// deployments should always set it (the Pulumi stack does).
    pub allowed_content_server_hosts: Option<Vec<String>>,
}

impl Config {
    pub fn from_env() -> Self {
        let platforms = std::env::var("PLATFORMS")
            .unwrap_or_else(|_| "windows,mac".to_string())
            .split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>();
        Config {
            platforms,
            version: std::env::var("AB_VERSION").unwrap_or_else(|_| "v49".to_string()),
            cache_dir: std::env::var("ABGEN_CACHE_DIR").unwrap_or_else(|_| {
                std::env::temp_dir()
                    .join("abgen-cache")
                    .to_string_lossy()
                    .into_owned()
            }),
            default_content_server: std::env::var("CONTENT_SERVER_URL")
                .unwrap_or_else(|_| "https://peer.decentraland.org/content".to_string()),
            out_root: std::env::var("OUT_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(|_| std::env::temp_dir().join("abgen-lambda-out")),
            keep_output: std::env::var("KEEP_OUTPUT")
                .map(|v| v == "1")
                .unwrap_or(false),
            registry_queue_url: std::env::var("REGISTRY_QUEUE_URL")
                .ok()
                .filter(|v| !v.is_empty()),
            allowed_content_server_hosts: std::env::var("ALLOWED_CONTENT_SERVER_HOSTS")
                .ok()
                .map(|raw| {
                    raw.split(',')
                        .map(|h| h.trim().to_ascii_lowercase())
                        .filter(|h| !h.is_empty())
                        .collect::<Vec<_>>()
                })
                .filter(|v| !v.is_empty()),
        }
    }
}

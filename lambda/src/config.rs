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
    /// Target CDN bucket. `S3_BUCKET`. TODO(step 3): used by output::publish.
    pub s3_bucket: Option<String>,
    /// asset-bundle-registry queue. `REGISTRY_QUEUE_URL`. TODO(step 4).
    pub registry_queue_url: Option<String>,
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
            s3_bucket: std::env::var("S3_BUCKET").ok().filter(|v| !v.is_empty()),
            registry_queue_url: std::env::var("REGISTRY_QUEUE_URL")
                .ok()
                .filter(|v| !v.is_empty()),
        }
    }
}

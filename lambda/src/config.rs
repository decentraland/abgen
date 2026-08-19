use std::path::PathBuf;

pub struct Config {
    pub platforms: Vec<String>,
    pub version: String,
    pub cache_dir: String,
    pub default_content_server: String,
    pub out_root: PathBuf,
    pub keep_output: bool,
    pub allowed_content_server_hosts: Option<Vec<String>>,
    pub http_secret: Option<String>,
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
            allowed_content_server_hosts: std::env::var("ALLOWED_CONTENT_SERVER_HOSTS")
                .ok()
                .map(|raw| {
                    raw.split(',')
                        .map(|h| h.trim().to_ascii_lowercase())
                        .filter(|h| !h.is_empty())
                        .collect::<Vec<_>>()
                })
                .filter(|v| !v.is_empty()),
            http_secret: std::env::var("ABGEN_HTTP_SECRET")
                .ok()
                .filter(|v| !v.is_empty()),
        }
    }
}

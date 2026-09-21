use std::path::PathBuf;

pub struct Config {
    pub platforms: Vec<String>,
    /// Key prefix for scene bundles (`AB_VERSION`).
    pub version: String,
    /// Key prefix for wearables and emotes (`WEARABLE_AB_VERSION`), defaulting to
    /// [`Config::version`] so a deployment that does not set it behaves exactly as it did
    /// when the two lanes shared one prefix.
    pub wearable_version: String,
    pub cache_dir: String,
    pub default_content_server: String,
    pub out_root: PathBuf,
    pub keep_output: bool,
    pub allowed_content_server_hosts: Option<Vec<String>>,
    pub http_secret: Option<String>,
    pub lods_enabled: bool,
    /// Must match the SQS redrive `maxReceiveCount`: a job failing on this
    /// receive publishes a failure tombstone instead of erroring into the DLQ.
    pub max_receive_count: u32,
    /// LOD levels the LOD lane builds and publishes (`LOD_LEVELS`).
    pub lod_levels: Vec<u32>,
}

impl Config {
    /// Reads the environment, refusing a version either lane cannot stamp into a manifest.
    pub fn from_env() -> Result<Self, String> {
        let platforms = std::env::var("PLATFORMS")
            .unwrap_or_else(|_| "windows,mac".to_string())
            .split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>();
        let lod_levels = match parse_levels(std::env::var("LOD_LEVELS").ok().as_deref()) {
            Ok(levels) => levels,
            Err(e) => {
                eprintln!("config: {e}; keeping the default LOD_LEVELS=1");
                default_levels()
            }
        };
        let version = std::env::var("AB_VERSION").unwrap_or_else(|_| "v49".to_string());
        validate_version("AB_VERSION", &version)?;
        let wearable_version = match std::env::var("WEARABLE_AB_VERSION")
            .ok()
            .filter(|v| !v.is_empty())
        {
            Some(v) => {
                validate_version("WEARABLE_AB_VERSION", &v)?;
                v
            }
            // Unset collapses the lanes onto AB_VERSION, already validated above.
            None => version.clone(),
        };
        Config {
            platforms,
            version: version.clone(),
            wearable_version,
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
            lods_enabled: abgen::clihelp::env_bool("ENABLE_LODS", false),
            max_receive_count: max_receive_count_from_env(),
            lod_levels,
        })
    }
}

/// Whether a key-prefix version is one the explorer can actually read.
///
/// The client parses the number out of the version and gates every capability on the
/// result — `AssetBundleManifestVersion.TryParseVersionNumber` takes a leading `v` and hands
/// the rest to `int.TryParse`, and hash-in-path (v25), deps digests (v49) and ISS (v49) are
/// each `TryParseVersionNumber(..) && version >= N`. A value that does not parse raises
/// nothing there: it makes all three answer `false` at once, so a current manifest is read
/// as if it were pre-v25 and every bundle URL is built under a layout the bundles are not
/// published at. That is silent in the client and invisible in the pipeline, so the honest
/// place to catch it is here, before a single manifest is stamped with it.
///
/// Stricter than the client on purpose: `int.TryParse` would accept surrounding whitespace
/// and a sign, and a producer has no business emitting either.
pub fn validate_version(var: &str, value: &str) -> Result<(), String> {
    let readable = value
        .strip_prefix('v')
        .filter(|digits| !digits.is_empty())
        .is_some_and(|digits| {
            digits.bytes().all(|b| b.is_ascii_digit()) && digits.parse::<i32>().is_ok()
        });
    if readable {
        return Ok(());
    }
    Err(format!(
        "{var}={value:?} is not a version the client can read: it must be `v` followed by an          integer (e.g. `v1003`). Anything else parses as nothing in the explorer and silently          disables hash-in-path, deps digests and ISS for every entity built under it."
    ))
}

pub fn default_levels() -> Vec<u32> {
    vec![1]
}

/// `LOD_LEVELS`: comma-separated subset of `0,1` in the order given, deduped.
/// `None`/blank -> the default `[1]`.
pub fn parse_levels(raw: Option<&str>) -> Result<Vec<u32>, String> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(default_levels());
    };
    let mut out: Vec<u32> = Vec::new();
    for tok in raw.split(',') {
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }
        let level: u32 = tok
            .parse()
            .map_err(|_| format!("LOD_LEVELS={raw:?}: {tok:?} is not a LOD level"))?;
        if level > 1 {
            return Err(format!(
                "LOD_LEVELS={raw:?}: level {level} refused (only levels 0 and 1 are generated)"
            ));
        }
        if !out.contains(&level) {
            out.push(level);
        }
    }
    if out.is_empty() {
        return Err(format!("LOD_LEVELS={raw:?}: no level given"));
    }
    Ok(out)
}

fn max_receive_count_from_env() -> u32 {
    let n = std::env::var("ABGEN_MAX_RECEIVE_COUNT")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(3);
    if n == 1 {
        eprintln!(
            "config: ABGEN_MAX_RECEIVE_COUNT=1 — every first failure tombstones \
             immediately, no SQS retries (misconfiguration?)"
        );
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_levels_is_1() {
        assert_eq!(default_levels(), vec![1]);
        assert_eq!(parse_levels(None).unwrap(), vec![1]);
        assert_eq!(parse_levels(Some("")).unwrap(), vec![1]);
        assert_eq!(parse_levels(Some("  ")).unwrap(), vec![1]);
    }

    #[test]
    fn lod_levels_parsing() {
        assert_eq!(parse_levels(Some("1")).unwrap(), vec![1]);
        assert_eq!(parse_levels(Some("0,1")).unwrap(), vec![0, 1]);
        assert_eq!(parse_levels(Some(" 1 , 0 ")).unwrap(), vec![1, 0]);
        assert_eq!(parse_levels(Some("1,1,0")).unwrap(), vec![1, 0]);
        assert!(parse_levels(Some("2")).is_err());
        assert!(parse_levels(Some("0,x")).is_err());
        assert!(parse_levels(Some(",")).is_err());
    }

    #[test]
    fn a_readable_version_is_v_then_an_integer() {
        for good in ["v0", "v15", "v49", "v1003", "v1500", "v2147483647"] {
            assert!(
                validate_version("AB_VERSION", good).is_ok(),
                "{good} should be readable"
            );
        }
    }

    #[test]
    fn a_version_the_client_cannot_parse_is_refused() {
        // `v1004w` is the one that prompted this: the explorer's TryParseVersionNumber hands
        // "1004w" to int.TryParse, which fails, and every capability check then answers false
        // instead of erroring — hash-in-path included, which is the layout wearables are
        // actually published under.
        for bad in [
            "v1004w", "v1003-rc1", "1003", "v", "", "V1003", "v 1003", "v+1003", "v-1", "vv1",
            // Past i32: parses nowhere the client can follow.
            "v2147483648",
        ] {
            let err = validate_version("WEARABLE_AB_VERSION", bad)
                .expect_err(&format!("{bad:?} should be refused"));
            assert!(err.contains("WEARABLE_AB_VERSION"), "{err}");
            assert!(err.contains(&format!("{bad:?}")), "{err}");
        }
    }
}

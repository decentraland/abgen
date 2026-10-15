use crate::config::Config;
use anyhow::{Context, Result};
use std::path::PathBuf;

pub struct PlatformOutcome {
    pub platform: String,
    pub built: Vec<String>,
    pub exit_code: i32,
}

pub struct EntityOutcome {
    pub entity_id: String,
    pub content_server: String,
    pub cid_dir: PathBuf,
    pub platforms: Vec<PlatformOutcome>,
    pub cache_hits: u64,
    pub cache_misses: u64,
}

impl EntityOutcome {
    pub fn exit_code(&self) -> i32 {
        self.platforms
            .iter()
            .map(|p| p.exit_code)
            .max()
            .unwrap_or(0)
    }

    /// Whether every platform produced bundles (see [`succeeded`]). Checked per platform,
    /// not on [`EntityOutcome::exit_code`]: that is the maximum, and a tolerated 12 on one
    /// platform would hide a 5 on another.
    pub fn succeeded(&self) -> bool {
        self.platforms.iter().all(|p| succeeded(p.exit_code))
    }
}

/// Whether a platform's exit code means its bundles stand: 0, or upstream's "conversion
/// errors tolerated" 12 (some assets failed to convert, the rest were bundled and
/// published). Anything else is a failure: nothing downstream of the conversion should run.
pub fn succeeded(exit_code: i32) -> bool {
    exit_code == 0 || exit_code == abgen::manifest::EXIT_CONVERSION_ERRORS_TOLERATED
}

#[cfg(test)]
mod exit_tests {
    use super::*;

    fn outcome(codes: &[i32]) -> EntityOutcome {
        EntityOutcome {
            entity_id: "e".to_string(),
            content_server: "cs".to_string(),
            cid_dir: std::path::PathBuf::new(),
            platforms: codes
                .iter()
                .enumerate()
                .map(|(i, &exit_code)| PlatformOutcome {
                    platform: format!("p{i}"),
                    built: Vec::new(),
                    exit_code,
                })
                .collect(),
            cache_hits: 0,
            cache_misses: 0,
        }
    }

    #[test]
    fn tolerated_errors_are_a_success_and_everything_else_is_not() {
        assert!(succeeded(0));
        assert!(succeeded(12));
        for code in [1, 2, 5, 13] {
            assert!(!succeeded(code), "{code}");
        }
    }

    #[test]
    fn every_platform_must_have_produced_bundles() {
        assert!(outcome(&[0, 0]).succeeded());
        assert!(outcome(&[0, 12]).succeeded());
        assert!(!outcome(&[0, 5]).succeeded());
        // The maximum would say 12 and hide the failure.
        assert_eq!(outcome(&[12, 5]).exit_code(), 12);
        assert!(!outcome(&[12, 5]).succeeded());
        assert!(outcome(&[]).succeeded(), "nothing pending, nothing failed");
    }
}

pub fn make_proxy(cfg: &Config, content_server: &str) -> std::sync::Arc<abgen::live::Proxy> {
    abgen::live::Proxy::new(abgen::live::ProxyConfig {
        catalyst_url: content_server.to_string(),
        cache_dir: cfg.cache_dir.clone(),
        version: cfg.version.clone(),
        wearable_version: cfg.wearable_version.clone(),
        deps_digest: true,
        use_space: true,
        ..Default::default()
    })
}

struct TexCacheClearGuard;

impl Drop for TexCacheClearGuard {
    fn drop(&mut self) {
        abgen::texencode_cache::clear();
        abgen::decode_cache::clear();
    }
}

pub fn convert_entity(
    cfg: &Config,
    proxy: &std::sync::Arc<abgen::live::Proxy>,
    entity_id: &str,
    content_server: &str,
    platforms: &[String],
) -> Result<EntityOutcome> {
    let (h0, m0, _, _) = abgen::texencode_cache::stats();
    let (dh0, dm0, _, _) = abgen::decode_cache::stats();
    let _clear_guard = TexCacheClearGuard;

    std::fs::create_dir_all(&cfg.out_root)
        .with_context(|| format!("mkdir {}", cfg.out_root.display()))?;
    let cid_dir = cfg
        .out_root
        .join(&*abgen::naming::fs_safe_component(entity_id));

    let mut results = Vec::with_capacity(platforms.len());
    for platform in platforms {
        let started = std::time::Instant::now();
        let built = proxy
            .build_entity_into_corpus(&cfg.out_root, entity_id, platform, content_server)
            .with_context(|| format!("convert {entity_id} for {platform}"))?;
        let elapsed = started.elapsed().as_secs_f64();
        eprintln!(
            "convert: {entity_id} {platform}: {} bundle(s) in {elapsed:.1}s",
            built.len(),
        );
        metrics::histogram!(
            "abgen_lambda_convert_duration_seconds", "platform" => platform.clone()
        )
        .record(elapsed);
        let manifest_path = cid_dir.join(format!("{platform}.manifest.json"));
        let exit_code = read_exit_code(&manifest_path).unwrap_or(0);
        results.push(PlatformOutcome {
            platform: platform.clone(),
            built,
            exit_code,
        });
    }

    let (h1, m1, _, _) = abgen::texencode_cache::stats();
    let (dh1, dm1, _, _) = abgen::decode_cache::stats();
    eprintln!(
        "convert: {entity_id}: decodecache hits={} misses={}",
        dh1.saturating_sub(dh0),
        dm1.saturating_sub(dm0)
    );

    Ok(EntityOutcome {
        entity_id: entity_id.to_string(),
        content_server: content_server.to_string(),
        cid_dir,
        platforms: results,
        cache_hits: h1.saturating_sub(h0),
        cache_misses: m1.saturating_sub(m0),
    })
}

fn read_exit_code(manifest_path: &std::path::Path) -> Option<i32> {
    let raw = std::fs::read_to_string(manifest_path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&raw).ok()?;
    json.get("exitCode")
        .and_then(serde_json::Value::as_i64)
        .map(|v| v as i32)
}

/// Serializes tests that mutate process-global space env (ABGEN_S3_*, AWS_*)
/// against tests whose assertions depend on the space being unconfigured.
#[cfg(test)]
pub(crate) static TEST_SPACE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

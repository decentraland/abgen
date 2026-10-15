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
}

/// What a platform's exit code can be once a conversion has produced an outcome: 0, or
/// upstream's "conversion errors tolerated" 12 (some assets failed to convert, the rest were
/// bundled and published; `abgen::manifest::exit_code_for_failures` writes nothing else). A
/// conversion that fails for real never produces an outcome: it errors out of the job, which
/// retries and finally tombstones (exit code 5, written on that path alone). So everything
/// downstream of a conversion, the finished events aside, runs only for bundles that stand,
/// and nothing downstream needs to look at this code to know it.
#[cfg(test)]
mod exit_code_tests {
    #[test]
    fn the_converter_only_ever_writes_a_standing_code() {
        for failures in [0, 1, 352] {
            let code = abgen::manifest::exit_code_for_failures(failures);
            assert!(
                code == 0 || code == abgen::manifest::EXIT_CONVERSION_ERRORS_TOLERATED,
                "{failures} failures -> {code}"
            );
        }
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

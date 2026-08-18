//! The conversion core: one entity, every configured platform, one process.
//!
//! Dual-emit economics: the expensive work (BC7/DXT texture encoding) is
//! platform-independent and cached process-wide (`abgen::texencode_cache`,
//! enabled in `init`), so the first platform pays for the encodes and the
//! rest reuse them. Downloads are shared through abgen's content cache dir.

use crate::config::Config;
use anyhow::{Context, Result};
use std::path::PathBuf;

pub struct PlatformOutcome {
    pub platform: String,
    /// Bundle file names abgen reports as built (aliases excluded), relative
    /// to `dir`.
    pub built: Vec<String>,
    /// abgen's corpus exit code: 0 clean, 12 some assets failed but tolerated.
    pub exit_code: i32,
    /// `{out_root}/{cid}/{platform}/` — where the bundle files sit.
    pub dir: PathBuf,
    /// `{out_root}/{cid}/{platform}.manifest.json`.
    pub manifest_path: PathBuf,
}

pub struct EntityOutcome {
    pub entity_id: String,
    pub content_server: String,
    /// `{out_root}/{cid}/` — removed after publishing unless keep_output.
    pub cid_dir: PathBuf,
    pub platforms: Vec<PlatformOutcome>,
    /// Texture-encode cache hits/misses across this entity's platforms — the
    /// dual-emit saving made visible (hits ≈ second platform's encodes).
    pub cache_hits: u64,
    pub cache_misses: u64,
}

impl EntityOutcome {
    /// Consumer-server convention: 0 clean, 12 tolerated per-asset failures.
    pub fn exit_code(&self) -> i32 {
        self.platforms
            .iter()
            .map(|p| p.exit_code)
            .max()
            .unwrap_or(0)
    }
}

pub fn convert_entity(
    cfg: &Config,
    entity_id: &str,
    content_server: &str,
    platforms: &[String],
) -> Result<EntityOutcome> {
    use abgen::live::{Proxy, ProxyConfig};

    let (h0, m0, _, _) = abgen::texencode_cache::stats();

    let proxy = Proxy::new(ProxyConfig {
        catalyst_url: content_server.to_string(),
        cache_dir: cfg.cache_dir.clone(),
        version: cfg.version.clone(),
        // Drives `{hash}_{digest}_{platform}` naming for scene glbs; the
        // digests abgen computes here are the ones we upload and announce, so
        // the pipeline is self-consistent end to end.
        asset_reuse: true,
        ..Default::default()
    });

    std::fs::create_dir_all(&cfg.out_root)
        .with_context(|| format!("mkdir {}", cfg.out_root.display()))?;
    let cid_dir = cfg
        .out_root
        .join(&*abgen::naming::fs_safe_component(entity_id));

    let mut results = Vec::with_capacity(platforms.len());
    for platform in platforms {
        let built = proxy
            .build_entity_into_corpus(&cfg.out_root, entity_id, platform, content_server)
            .with_context(|| format!("convert {entity_id} for {platform}"))?;
        let manifest_path = cid_dir.join(format!("{platform}.manifest.json"));
        let exit_code = read_exit_code(&manifest_path).unwrap_or(0);
        results.push(PlatformOutcome {
            platform: platform.clone(),
            built,
            exit_code,
            dir: cid_dir.join(platform),
            manifest_path,
        });
    }

    let (h1, m1, _, _) = abgen::texencode_cache::stats();
    // Keep the cache across warm invocations only up to its byte budget;
    // entities rarely share textures, so drop this entity's encodes now
    // rather than evicting someone else's later.
    abgen::texencode_cache::clear();

    Ok(EntityOutcome {
        entity_id: entity_id.to_string(),
        content_server: content_server.to_string(),
        cid_dir,
        platforms: results,
        cache_hits: h1.saturating_sub(h0),
        cache_misses: m1.saturating_sub(m0),
    })
}

/// abgen records its exit code in the corpus manifest it writes.
fn read_exit_code(manifest_path: &std::path::Path) -> Option<i32> {
    let raw = std::fs::read_to_string(manifest_path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&raw).ok()?;
    json.get("exitCode")
        .and_then(serde_json::Value::as_i64)
        .map(|v| v as i32)
}

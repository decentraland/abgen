//! `abgen-lod bevy-far-lod`: build the Bevy copy of already-published far LODs, for scenes
//! whose LODs were built before the copy existed (new builds write it themselves).

use abgen::bevyvariants::farlod::{self, FarLodStats};
use anyhow::{anyhow, bail, Context, Result};
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::Args;

const DEFAULT_BASE: &str = "https://abgen-cdn.decentraland.org";

#[derive(serde::Serialize)]
struct Row {
    scene_id: String,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stats: Option<FarLodStats>,
}

#[derive(Default, serde::Serialize)]
struct Totals {
    scenes: usize,
    built: usize,
    missing: usize,
    failed: usize,
    plain_bytes: u64,
    bevy_bytes: u64,
    /// The `.br` when there is one, else the Bevy GLB: what a Brotli-capable client downloads.
    download_bytes: u64,
    plain_texture_gpu_bytes: u64,
    texture_gpu_bytes: u64,
    texture_gpu_by_format: BTreeMap<String, u64>,
    images_by_format: BTreeMap<String, u64>,
    alpha_dropped: u64,
    bc1_rejected: u64,
    mesh_vertex_bytes_f32: u64,
    mesh_index_bytes: u64,
    gpu_bytes: u64,
    encode_ms: u64,
}

fn read_ids(file: &Path) -> Result<Vec<String>> {
    let text = std::fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect())
}

fn fetch(url: &str) -> Result<Option<Vec<u8>>> {
    use std::io::Read;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(60)))
        .build()
        .into();
    match agent.get(url).call() {
        Ok(resp) => {
            let mut buf = Vec::new();
            resp.into_body()
                .into_reader()
                .read_to_end(&mut buf)
                .with_context(|| format!("read {url}"))?;
            Ok(Some(buf))
        }
        Err(ureq::Error::StatusCode(404 | 403)) => Ok(None),
        Err(e) => Err(anyhow!("GET {url}: {e}")),
    }
}

pub fn run(argv: &[String]) -> Result<i32> {
    let mut ids: Vec<String> = Vec::new();
    let mut out: Option<PathBuf> = None;
    let mut base = DEFAULT_BASE.to_string();
    let mut glb_dir: Option<PathBuf> = None;
    let mut level = 1u32;
    let mut jobs = 8usize;
    let mut upload = false;
    let mut opts = farlod::options();
    let mut a = Args::new(argv);
    while let Some(arg) = a.next() {
        match arg.as_str() {
            "--scene" => ids.extend(
                a.val()?
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
            ),
            "--ids" => ids.extend(read_ids(Path::new(a.val()?))?),
            "--out" => out = Some(PathBuf::from(a.val()?)),
            "--base-url" => base = a.val()?.trim_end_matches('/').to_string(),
            "--glb-dir" => glb_dir = Some(PathBuf::from(a.val()?)),
            "--level" => level = a.val()?.parse().context("--level")?,
            "-j" | "--jobs" => jobs = a.val()?.parse().context("-j")?,
            "--max-side" => {
                opts.max_side = a.val()?.parse().context("--max-side")?;
                if !opts.max_side.is_power_of_two() || opts.max_side < 4 {
                    bail!("--max-side must be a power of two of at least 4");
                }
            }
            "--no-mips" => opts.mips = false,
            "--keep-alpha" => opts.drop_unused_alpha = false,
            "--upload" => upload = true,
            other => bail!("bevy-far-lod: unknown argument {other:?}"),
        }
    }
    let Some(out) = out else {
        bail!("bevy-far-lod needs --out");
    };
    if ids.is_empty() {
        bail!("bevy-far-lod needs --scene or --ids");
    }
    let space = if upload {
        Some(abgen::space::Space::from_env().ok_or_else(|| {
            anyhow!("--upload needs ABGEN_S3_ENDPOINT, ABGEN_S3_BUCKET and credentials")
        })?)
    } else {
        None
    };
    // Written so that `out/LOD` + `farlod::GLB_DIR` is the key layout.
    let root = out.join(abgen::lods::PUBLISH_ROOT);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(jobs.max(1))
        .build()?;
    let rows: Vec<Row> = pool.install(|| {
        ids.par_iter()
            .map(|id| {
                let sid = id.to_lowercase();
                let name = abgen::lods::published_glb_name(&sid, level);
                let plain = match &glb_dir {
                    Some(dir) => {
                        let p = dir.join(&name);
                        if p.exists() {
                            std::fs::read(&p)
                                .map(Some)
                                .with_context(|| format!("read {}", p.display()))
                        } else {
                            Ok(None)
                        }
                    }
                    None => fetch(&format!("{base}/{}/{name}", abgen::lods::GLB_KEY_DIR)),
                };
                let row = |status, error: Option<String>, stats| Row {
                    scene_id: sid.clone(),
                    status,
                    error,
                    stats,
                };
                let plain = match plain {
                    Ok(Some(b)) => b,
                    Ok(None) => return row("missing", None, None),
                    Err(e) => return row("failed", Some(format!("{e:#}")), None),
                };
                let far = match farlod::build(&plain, &sid, level, opts) {
                    Ok(Some(far)) => far,
                    Ok(None) => {
                        return row(
                            "failed",
                            Some("embedded KTX2 does not transcode".into()),
                            None,
                        )
                    }
                    Err(e) => return row("failed", Some(format!("{e:#}")), None),
                };
                let written = farlod::write(&root, &far).and_then(|path| {
                    let Some(space) = &space else { return Ok(()) };
                    let dir = path.parent().unwrap_or(&root);
                    let mut files = vec![path.clone()];
                    if far.br.is_some() {
                        files.push(abgen::bevyvariants::glbbr::br_sidecar(&path));
                    }
                    files.push(dir.join(farlod::stats_name(&sid, level)));
                    for f in files {
                        let name = f.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                        let key = format!("{}/{name}", farlod::KEY_DIR);
                        let body = std::fs::read(&f)?;
                        space
                            .put(&key, &body)
                            .with_context(|| format!("put {key}"))?;
                    }
                    Ok(())
                });
                match written {
                    Ok(()) => row("built", None, Some(far.stats)),
                    Err(e) => row("failed", Some(format!("{e:#}")), Some(far.stats)),
                }
            })
            .collect()
    });

    let mut t = Totals {
        scenes: rows.len(),
        ..Default::default()
    };
    for r in &rows {
        match r.status {
            "built" => t.built += 1,
            "missing" => t.missing += 1,
            _ => {
                t.failed += 1;
                eprintln!(
                    "FAIL {}: {}",
                    r.scene_id,
                    r.error.as_deref().unwrap_or_default()
                );
            }
        }
        let Some(s) = &r.stats else { continue };
        t.plain_bytes += s.plain_bytes;
        t.bevy_bytes += s.bevy_bytes;
        t.download_bytes += s.br_bytes.unwrap_or(s.bevy_bytes);
        t.plain_texture_gpu_bytes += s.plain_texture_gpu_bytes;
        t.texture_gpu_bytes += s.texture_gpu_bytes;
        for (k, v) in &s.texture_gpu_by_format {
            *t.texture_gpu_by_format.entry(k.clone()).or_default() += v;
        }
        for img in &s.images {
            *t.images_by_format
                .entry(img.format.unwrap_or("rgba8").to_string())
                .or_default() += 1;
            t.alpha_dropped += u64::from(img.alpha_dropped);
            t.bc1_rejected += u64::from(img.bc1_rejected);
        }
        t.mesh_vertex_bytes_f32 += s.mesh.vertex_bytes_f32;
        t.mesh_index_bytes += s.mesh.index_bytes;
        t.gpu_bytes += s.gpu_bytes;
        t.encode_ms += s.encode_ms;
    }
    std::fs::create_dir_all(&out)?;
    let report = serde_json::json!({
        "options": {
            "level": level,
            "max_side": opts.max_side,
            "mips": opts.mips,
            "drop_unused_alpha": opts.drop_unused_alpha,
            "zstd_level": opts.zstd_level,
        },
        "totals": &t,
        "scenes": &rows,
    });
    let report_path = out.join("bevy-far-lod-report.json");
    std::fs::write(&report_path, serde_json::to_vec_pretty(&report)?)?;
    println!("{}", serde_json::to_string_pretty(&t)?);
    eprintln!("report: {}", report_path.display());
    Ok(if t.failed > 0 { 1 } else { 0 })
}

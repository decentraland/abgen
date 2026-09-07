use abgen::lodgen::{gate_failures, GenerateParams};
use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use std::collections::{BTreeSet, HashSet, VecDeque};
use std::io::Read;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_CATALYST: &str = "https://catalyst.dcl.one/content";
const DEFAULT_WORLDS: &str = "https://worlds-content-server.decentraland.org";
const DEFAULT_ATTEMPTS: u32 = 3;
const DEFAULT_SNAPSHOT_PASSES: usize = 8;
const RISK_SCENES: [&str; 4] = [
    "bafkreiceqm43l33evsc43jtotf2fs27efizwxn76cdnd3ypd6mcsdnpf6a",
    "bafkreib3pp3kds7ftnvnaebbr2qzm2nnl4i4yuuc5b572ueuaeaj5rnnga",
    "bafkreiavfwe6n4eec6xnxxqkgmzj5fozfywcjxyoffzjwuzbtlvzumaeye",
    "bafkreifed6j4zxjdv72sxyupsz3kj4mf6hogxaccvwcdmfvribogg3waxa",
];

#[derive(Clone)]
struct Job {
    entity_id: String,
    source: String,
    catalyst: String,
}

impl Job {
    fn key(&self) -> String {
        format!("{}\0{}", self.catalyst, self.entity_id)
    }
}

struct Options {
    catalyst: String,
    worlds_url: String,
    out: PathBuf,
    report: PathBuf,
    cache: PathBuf,
    jobs: usize,
    max_attempts: u32,
    snapshot_passes: usize,
    city_min: i32,
    city_max: i32,
    city: bool,
    worlds: bool,
    world_names: Vec<String>,
    entity_ids: Vec<String>,
    platforms: Vec<String>,
}

#[derive(Serialize)]
struct ArtifactRecord {
    kind: String,
    level: Option<u32>,
    platform: Option<String>,
    relative_path: String,
    bytes: usize,
    sha256: String,
}

#[derive(Serialize)]
struct SimplifyRecord {
    level: u32,
    policy: String,
    tris_before: usize,
    tris_after: usize,
    ratios: Vec<f64>,
    target_errors: Vec<f64>,
    passthrough: bool,
    unsimplified: bool,
}

#[derive(Serialize)]
struct PlacementRecord {
    count: usize,
    rotated: usize,
    non_uniform_scale: usize,
    mirrored: usize,
    extreme_scale: usize,
}

#[derive(Serialize)]
struct GateRecord {
    label: String,
    ok: bool,
    detail: String,
}

#[derive(Serialize)]
struct SceneRecord {
    entity_id: String,
    source: String,
    catalyst: String,
    ok: bool,
    attempts: u32,
    retry_errors: Vec<String>,
    encoder_backend: Option<String>,
    elapsed_ms: u128,
    placement_source: Option<String>,
    placements: Option<PlacementRecord>,
    material_count: usize,
    texture_count: usize,
    source_tris: Option<usize>,
    bundle_bytes: usize,
    io: serde_json::Map<String, serde_json::Value>,
    timing_ms: serde_json::Map<String, serde_json::Value>,
    simplify: Vec<SimplifyRecord>,
    artifacts: Vec<ArtifactRecord>,
    gates: Vec<GateRecord>,
    error: Option<String>,
}

#[derive(Serialize)]
struct Summary {
    discovered: usize,
    processed: usize,
    passed: usize,
    failed: usize,
    elapsed_ms: u128,
    peak_rss_kib: u64,
    output_bytes: usize,
    network_requests: u64,
    network_bytes: u64,
    cache_hits: u64,
    cache_bytes: u64,
    scenes_per_second: f64,
}

#[derive(Serialize)]
struct TextureEncoderRecord {
    backend: String,
    qualified: bool,
    reason: Option<String>,
}

#[derive(Serialize)]
struct Report {
    schema_version: u32,
    started_unix_ms: u128,
    catalyst: String,
    worlds_url: String,
    platforms: Vec<String>,
    workers: usize,
    texture_encoder: TextureEncoderRecord,
    max_attempts: u32,
    snapshot_limit: usize,
    snapshot_sha256: String,
    snapshot_passes: usize,
    snapshot_stable: bool,
    summary: Summary,
    explorer_candidates: Vec<String>,
    scenes: Vec<SceneRecord>,
}

fn value(argv: &[String], i: &mut usize) -> Result<String> {
    *i += 1;
    argv.get(*i)
        .cloned()
        .ok_or_else(|| anyhow!("{} needs a value", argv[*i - 1]))
}

fn parse(argv: &[String]) -> Result<Options> {
    let mut catalyst = DEFAULT_CATALYST.to_string();
    let mut worlds_url =
        std::env::var(abgen::worlds::WORLDS_URL_ENV).unwrap_or_else(|_| DEFAULT_WORLDS.to_string());
    let mut out = PathBuf::from("lod-qualification");
    let mut report = None;
    let mut cache = None;
    let mut jobs = abgen::clihelp::default_file_concurrency().max(1);
    let mut max_attempts = DEFAULT_ATTEMPTS;
    let mut snapshot_passes = DEFAULT_SNAPSHOT_PASSES;
    let mut city_min = -150;
    let mut city_max = 150;
    let mut city = true;
    let mut worlds = true;
    let mut world_names = Vec::new();
    let mut entity_ids = Vec::new();
    let mut platforms = vec!["windows".to_string(), "mac".to_string()];
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "-h" | "--help" => abgen::clihelp::print_help(super::usage_text()),
            "--catalyst" => catalyst = value(argv, &mut i)?,
            "--worlds-url" => worlds_url = value(argv, &mut i)?,
            "--out" => out = PathBuf::from(value(argv, &mut i)?),
            "--report" => report = Some(PathBuf::from(value(argv, &mut i)?)),
            "--cache" => cache = Some(PathBuf::from(value(argv, &mut i)?)),
            "-j" | "--jobs" => jobs = value(argv, &mut i)?.parse().context("--jobs")?,
            "--attempts" => max_attempts = value(argv, &mut i)?.parse().context("--attempts")?,
            "--snapshot-passes" => {
                snapshot_passes = value(argv, &mut i)?.parse().context("--snapshot-passes")?
            }
            "--city-min" => city_min = value(argv, &mut i)?.parse().context("--city-min")?,
            "--city-max" => city_max = value(argv, &mut i)?.parse().context("--city-max")?,
            "--no-city" => city = false,
            "--no-worlds" => worlds = false,
            "--world" => world_names.extend(
                value(argv, &mut i)?
                    .split(',')
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(str::to_string),
            ),
            "--entity-ids" => {
                let path = value(argv, &mut i)?;
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("read entity ids {path}"))?;
                entity_ids.extend(
                    text.lines()
                        .map(str::trim)
                        .filter(|v| !v.is_empty() && !v.starts_with('#'))
                        .map(str::to_string),
                );
            }
            "--platform" => {
                platforms = value(argv, &mut i)?
                    .split(',')
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(str::to_string)
                    .collect();
            }
            other => bail!("unknown qualify-corpus argument {other:?}"),
        }
        i += 1;
    }
    if jobs == 0 {
        bail!("--jobs must be greater than zero");
    }
    if max_attempts == 0 {
        bail!("--attempts must be greater than zero");
    }
    if snapshot_passes == 0 {
        bail!("--snapshot-passes must be greater than zero");
    }
    if city_min > city_max {
        bail!("--city-min must not exceed --city-max");
    }
    if !city && !worlds && entity_ids.is_empty() {
        bail!("qualification has no source: enable city/worlds or pass --entity-ids");
    }
    if platforms.is_empty() {
        bail!("--platform needs at least one platform");
    }
    for platform in &platforms {
        abgen::lods::validate_lod_platform(platform)?;
    }
    let report = report.unwrap_or_else(|| out.join("qualification.json"));
    let cache = cache.unwrap_or_else(|| out.join(".cache"));
    Ok(Options {
        catalyst,
        worlds_url,
        out,
        report,
        cache,
        jobs,
        max_attempts,
        snapshot_passes,
        city_min,
        city_max,
        city,
        worlds,
        world_names,
        entity_ids,
        platforms,
    })
}

fn transient_error(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}").to_ascii_lowercase();
    [
        "timed out",
        "timeout",
        "connection reset",
        "connection refused",
        "temporarily unavailable",
        "http 408",
        "http 429",
        "http 500",
        "http 502",
        "http 503",
        "http 504",
        "status code 408",
        "status code 429",
        "status code 500",
        "status code 502",
        "status code 503",
        "status code 504",
        "http status: 408",
        "http status: 429",
        "http status: 500",
        "http status: 502",
        "http status: 503",
        "http status: 504",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

fn retry_delay(attempt: u32) -> Duration {
    Duration::from_millis(250 * (1u64 << attempt.min(3)))
}

fn get_json_once(url: &str) -> Result<serde_json::Value> {
    let response = ureq::get(url)
        .config()
        .timeout_global(Some(Duration::from_secs(120)))
        .build()
        .call()
        .with_context(|| format!("GET {url}"))?;
    let mut bytes = Vec::new();
    response
        .into_body()
        .into_reader()
        .take(512 * 1024 * 1024)
        .read_to_end(&mut bytes)?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {url}"))
}

fn get_json_with_sleep<F>(url: &str, mut sleep: F) -> Result<serde_json::Value>
where
    F: FnMut(Duration),
{
    let mut errors = Vec::new();
    for attempt in 0..DEFAULT_ATTEMPTS {
        match get_json_once(url) {
            Ok(value) => return Ok(value),
            Err(error) => {
                let transient = transient_error(&error);
                errors.push(format!("attempt {}: {error:#}", attempt + 1));
                if !transient || attempt + 1 == DEFAULT_ATTEMPTS {
                    bail!("{}", errors.join("; "));
                }
                sleep(retry_delay(attempt));
            }
        }
    }
    unreachable!()
}

fn get_json(url: &str) -> Result<serde_json::Value> {
    get_json_with_sleep(url, std::thread::sleep)
}

fn component(raw: &str) -> String {
    let mut out = String::new();
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn discover_city(opts: &Options) -> Result<Vec<Job>> {
    if !opts.city {
        return Ok(Vec::new());
    }
    let client = abgen::catalyst::CatalystClient::from_args(&opts.catalyst, None);
    let mut pointers = Vec::new();
    for x in opts.city_min..=opts.city_max {
        for y in opts.city_min..=opts.city_max {
            pointers.push(format!("{x},{y}"));
        }
    }
    let mut jobs = Vec::new();
    let mut seen = HashSet::new();
    for chunk in pointers.chunks(2048) {
        for scene in client.resolve_entities(chunk)? {
            if scene.entity_type == "scene" && seen.insert(scene.entity_id.clone()) {
                jobs.push(Job {
                    entity_id: scene.entity_id,
                    source: "city".to_string(),
                    catalyst: opts.catalyst.clone(),
                });
            }
        }
    }
    Ok(jobs)
}

fn world_names(opts: &Options) -> Result<Vec<String>> {
    if !opts.worlds {
        return Ok(Vec::new());
    }
    if !opts.world_names.is_empty() {
        return Ok(opts.world_names.clone());
    }
    let mut names = Vec::new();
    let mut offset = 0usize;
    loop {
        let url = format!(
            "{}/worlds?limit=100&offset={offset}&has_deployed_scenes=true&sort=name&order=asc",
            opts.worlds_url.trim_end_matches('/')
        );
        let page = get_json(&url)?;
        let rows = page
            .get("worlds")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("{url} returned no worlds array"))?;
        names.extend(
            rows.iter()
                .filter_map(|v| v.get("name").and_then(|v| v.as_str()))
                .map(str::to_string),
        );
        let total = page.get("total").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        offset += rows.len();
        if rows.is_empty() || offset >= total {
            break;
        }
    }
    Ok(names)
}

fn discover_worlds(opts: &Options) -> Result<Vec<Job>> {
    if opts.worlds && opts.world_names.is_empty() {
        let url = format!("{}/index", opts.worlds_url.trim_end_matches('/'));
        if let Ok(index) = get_json(&url) {
            let rows = index
                .get("data")
                .or_else(|| index.get("index"))
                .and_then(|v| v.as_array())
                .ok_or_else(|| anyhow!("{url} returned no data/index array"))?;
            let mut jobs = Vec::new();
            let mut seen = HashSet::new();
            for world in rows {
                let name = world.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let scenes = world
                    .get("scenes")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| anyhow!("{url} world {name:?} returned no scenes array"))?;
                for scene in scenes {
                    let Some(entity_id) = scene.get("id").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    if seen.insert(entity_id.to_string()) {
                        jobs.push(Job {
                            entity_id: entity_id.to_string(),
                            source: format!("world:{name}"),
                            catalyst: opts.worlds_url.clone(),
                        });
                    }
                }
            }
            jobs.sort_by_key(Job::key);
            return Ok(jobs);
        }
        eprintln!("worlds: /index unavailable; falling back to paginated discovery");
    }

    let mut jobs = Vec::new();
    let mut seen = HashSet::new();
    for name in world_names(opts)? {
        let mut offset = 0usize;
        loop {
            let url = format!(
                "{}/world/{}/scenes?limit=100&offset={offset}",
                opts.worlds_url.trim_end_matches('/'),
                component(&name)
            );
            let page = get_json(&url)?;
            let rows = page
                .get("scenes")
                .and_then(|v| v.as_array())
                .ok_or_else(|| anyhow!("{url} returned no scenes array"))?;
            for row in rows {
                let Some(entity_id) = row.get("entityId").and_then(|v| v.as_str()) else {
                    continue;
                };
                let key = format!("{}\0{entity_id}", opts.worlds_url);
                if seen.insert(key) {
                    jobs.push(Job {
                        entity_id: entity_id.to_string(),
                        source: format!("world:{name}"),
                        catalyst: opts.worlds_url.clone(),
                    });
                }
            }
            let total = page.get("total").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            offset += rows.len();
            if rows.is_empty() || offset >= total {
                break;
            }
        }
    }
    Ok(jobs)
}

fn discover(opts: &Options) -> Result<Vec<Job>> {
    let mut jobs = discover_city(opts)?;
    jobs.extend(discover_worlds(opts)?);
    jobs.extend(opts.entity_ids.iter().map(|entity_id| Job {
        entity_id: entity_id.clone(),
        source: "explicit".to_string(),
        catalyst: opts.catalyst.clone(),
    }));
    let mut seen = HashSet::new();
    jobs.retain(|job| seen.insert(job.key()));
    jobs.sort_by_key(Job::key);
    Ok(jobs)
}

fn numeric_fields(log: &[String], prefix: &str) -> serde_json::Map<String, serde_json::Value> {
    let mut out = serde_json::Map::new();
    let Some(line) = log.iter().find(|line| line.starts_with(prefix)) else {
        return out;
    };
    for field in line.trim_start_matches(prefix).split_ascii_whitespace() {
        let Some((name, value)) = field.split_once('=') else {
            continue;
        };
        if let Ok(value) = value.parse::<u64>() {
            out.insert(name.to_string(), value.into());
        }
    }
    out
}

fn gate_count(gates: &[abgen::lodgen::GateCheck], suffix: &str) -> usize {
    gates
        .iter()
        .filter(|gate| gate.label.ends_with(suffix))
        .filter_map(|gate| gate.detail.split_ascii_whitespace().next()?.parse().ok())
        .max()
        .unwrap_or(0)
}

fn deployment_identity_matches(expected: &str, resolved: &str) -> bool {
    expected == resolved
}

fn artifact(
    scene_dir: &std::path::Path,
    path: &std::path::Path,
    kind: &str,
    level: Option<u32>,
    platform: Option<&str>,
) -> Result<ArtifactRecord> {
    let bytes = std::fs::read(path).with_context(|| format!("read artifact {}", path.display()))?;
    let relative_path = path
        .strip_prefix(scene_dir)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    Ok(ArtifactRecord {
        kind: kind.to_string(),
        level,
        platform: platform.map(str::to_string),
        relative_path,
        bytes: bytes.len(),
        sha256: abgen::hashes::sha256_hex(&bytes),
    })
}

fn collect_artifacts(
    job: &Job,
    opts: &Options,
    outcome: &abgen::lodgen::GenerateOutcome,
) -> Result<Vec<ArtifactRecord>> {
    let scene_dir = opts
        .out
        .join(component(&job.source))
        .join(&outcome.scene_id);
    let mut artifacts = Vec::new();
    for level in &outcome.levels {
        for platform in &opts.platforms {
            let rel = abgen::lodgen::expected_rel_path(&outcome.scene_id, level.level, platform);
            artifacts.push(artifact(
                &scene_dir,
                &scene_dir.join(rel),
                "bundle",
                Some(level.level),
                Some(platform),
            )?);
        }
        if level.level >= 1 {
            let rel = format!(
                "{}/{}",
                abgen::lods::PUBLISHED_GLB_DIR,
                abgen::lods::published_glb_name(&outcome.scene_id, level.level)
            );
            artifacts.push(artifact(
                &scene_dir,
                &scene_dir.join(rel),
                "published-glb",
                Some(level.level),
                None,
            )?);
        }
    }
    artifacts.push(artifact(
        &scene_dir,
        &scene_dir.join("LOD.manifest.json"),
        "lod-manifest",
        None,
        None,
    )?);
    artifacts.push(artifact(
        &scene_dir,
        &scene_dir.join(format!(
            "{}{}",
            outcome.scene_id,
            abgen::lodgen::placements::ISS_SUFFIX
        )),
        "placement-descriptor",
        None,
        None,
    )?);
    Ok(artifacts)
}

fn failed_record(
    job: Job,
    attempts: u32,
    retry_errors: Vec<String>,
    error: anyhow::Error,
) -> SceneRecord {
    SceneRecord {
        entity_id: job.entity_id,
        source: job.source,
        catalyst: job.catalyst,
        ok: false,
        attempts,
        retry_errors,
        encoder_backend: None,
        elapsed_ms: 0,
        placement_source: None,
        placements: None,
        material_count: 0,
        texture_count: 0,
        source_tris: None,
        bundle_bytes: 0,
        io: serde_json::Map::new(),
        timing_ms: serde_json::Map::new(),
        simplify: Vec::new(),
        artifacts: Vec::new(),
        gates: Vec::new(),
        error: Some(format!("{error:#}")),
    }
}

fn discovery_failure(stage: &str, error: anyhow::Error) -> SceneRecord {
    failed_record(
        Job {
            entity_id: "<discovery>".to_string(),
            source: stage.to_string(),
            catalyst: String::new(),
        },
        1,
        Vec::new(),
        error,
    )
}

fn run_one_attempt(job: &Job, opts: &Options) -> Result<SceneRecord> {
    let params = GenerateParams {
        scene: job.entity_id.clone(),
        catalyst: job.catalyst.clone(),
        out_dir: opts
            .out
            .join(component(&job.source))
            .to_string_lossy()
            .into_owned(),
        cache: Some(opts.cache.clone()),
        platform: opts.platforms[0].clone(),
        platforms: opts.platforms.clone(),
        ..Default::default()
    };
    let encoder_backend = params.simplifier.name().to_string();
    let outcome = abgen::lodgen::generate(&params)?;
    let failed = gate_failures(&outcome.gate);
    let identity_ok = deployment_identity_matches(&job.entity_id, &outcome.entity_id);
    let placement_source = outcome
        .log
        .iter()
        .find_map(|line| line.strip_prefix("placement-source: ").map(str::to_string));
    let artifacts = collect_artifacts(job, opts, &outcome)?;
    let bundle_bytes = artifacts
        .iter()
        .filter(|artifact| artifact.kind == "bundle")
        .map(|artifact| artifact.bytes)
        .sum();
    let placements = outcome.placement_stats;
    let mut gates: Vec<GateRecord> = outcome
        .gate
        .iter()
        .map(|gate| GateRecord {
            label: gate.label.clone(),
            ok: gate.ok,
            detail: gate.detail.clone(),
        })
        .collect();
    gates.push(GateRecord {
        label: "deployment-identity".to_string(),
        ok: identity_ok,
        detail: format!("resolved {} expected {}", outcome.entity_id, job.entity_id),
    });
    Ok(SceneRecord {
        entity_id: job.entity_id.clone(),
        source: job.source.clone(),
        catalyst: job.catalyst.clone(),
        ok: failed == 0 && identity_ok,
        attempts: 1,
        retry_errors: Vec::new(),
        encoder_backend: Some(encoder_backend),
        elapsed_ms: 0,
        placement_source,
        placements: Some(PlacementRecord {
            count: placements.count,
            rotated: placements.rotated,
            non_uniform_scale: placements.non_uniform_scale,
            mirrored: placements.mirrored,
            extreme_scale: placements.extreme_scale,
        }),
        material_count: gate_count(&outcome.gate, ":material-count"),
        texture_count: gate_count(&outcome.gate, ":texture-count"),
        source_tris: Some(outcome.source_tris),
        bundle_bytes,
        io: numeric_fields(&outcome.log, "io: "),
        timing_ms: numeric_fields(&outcome.log, "timing: "),
        simplify: outcome
            .levels
            .iter()
            .map(|level| SimplifyRecord {
                level: level.level,
                policy: level.simplify.policy.to_string(),
                tris_before: level.simplify.tris_before,
                tris_after: level.simplify.tris_after,
                ratios: level.simplify.ratios_run.clone(),
                target_errors: level.simplify.se_run.clone(),
                passthrough: level.simplify.passthrough,
                unsimplified: level.simplify.unsimplified,
            })
            .collect(),
        artifacts,
        gates,
        error: if failed > 0 {
            Some(format!("{failed} self-gate checks failed"))
        } else if !identity_ok {
            Some("resolved deployment differs from snapshotted entity".to_string())
        } else {
            None
        },
    })
}

fn run_with_retry<F, S>(job: Job, opts: &Options, mut attempt_fn: F, mut sleep: S) -> SceneRecord
where
    F: FnMut(&Job, &Options) -> Result<SceneRecord>,
    S: FnMut(Duration),
{
    let started = Instant::now();
    let mut retry_errors = Vec::new();
    for attempt in 1..=opts.max_attempts {
        match attempt_fn(&job, opts) {
            Ok(mut record) => {
                record.attempts = attempt;
                record.retry_errors = retry_errors;
                record.elapsed_ms = started.elapsed().as_millis();
                return record;
            }
            Err(error) => {
                let transient = transient_error(&error);
                retry_errors.push(format!("attempt {attempt}: {error:#}"));
                if !transient || attempt == opts.max_attempts {
                    let mut record = failed_record(job, attempt, retry_errors, error);
                    record.elapsed_ms = started.elapsed().as_millis();
                    return record;
                }
                sleep(retry_delay(attempt - 1));
            }
        }
    }
    unreachable!()
}

fn run_one(job: Job, opts: &Options) -> SceneRecord {
    run_with_retry(job, opts, run_one_attempt, std::thread::sleep)
}

fn process_with<R>(jobs: Vec<Job>, opts: &Options, runner: &R) -> Vec<SceneRecord>
where
    R: Fn(Job, &Options) -> SceneRecord + Sync,
{
    let queue = Arc::new(Mutex::new(VecDeque::from(jobs)));
    let (tx, rx) = mpsc::channel();
    std::thread::scope(|scope| {
        for _ in 0..opts.jobs {
            let queue = Arc::clone(&queue);
            let tx = tx.clone();
            scope.spawn(move || loop {
                let job = queue
                    .lock()
                    .expect("qualification queue poisoned")
                    .pop_front();
                let Some(job) = job else { break };
                let _ = tx.send(runner(job, opts));
            });
        }
        drop(tx);
        rx.into_iter().collect()
    })
}

#[cfg(unix)]
fn peak_rss_kib() -> u64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return 0;
    }
    let rss = unsafe { usage.assume_init().ru_maxrss } as u64;
    rss / if cfg!(target_os = "macos") { 1024 } else { 1 }
}

#[cfg(not(unix))]
fn peak_rss_kib() -> u64 {
    0
}

fn write_report(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension().and_then(|v| v.to_str()).unwrap_or("json")
    ));
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} to {}", tmp.display(), path.display()))
}

fn explorer_candidates(records: &[SceneRecord]) -> Vec<String> {
    let mut selected = BTreeSet::new();
    for risk in RISK_SCENES {
        if records.iter().any(|v| v.entity_id == risk) {
            selected.insert(risk.to_string());
        }
    }
    for metric in 0..5 {
        let mut rows: Vec<&SceneRecord> = records.iter().filter(|v| v.ok).collect();
        rows.sort_by_key(|v| {
            std::cmp::Reverse(match metric {
                0 => v.elapsed_ms as usize,
                1 => v.source_tris.unwrap_or(0),
                2 => v.bundle_bytes,
                3 => v.material_count,
                _ => v.texture_count,
            })
        });
        selected.extend(rows.into_iter().take(5).map(|v| v.entity_id.clone()));
    }
    selected.extend(
        records
            .iter()
            .filter(|v| v.placement_source.as_deref() == Some("embedded-sdk"))
            .take(5)
            .map(|v| v.entity_id.clone()),
    );
    let mut unusual: Vec<&SceneRecord> = records
        .iter()
        .filter(|record| {
            record.ok
                && record.placements.as_ref().is_some_and(|stats| {
                    stats.rotated + stats.non_uniform_scale + stats.mirrored + stats.extreme_scale
                        > 0
                })
        })
        .collect();
    unusual.sort_by_key(|record| {
        std::cmp::Reverse(record.placements.as_ref().map_or(0, |stats| {
            stats.rotated + stats.non_uniform_scale + stats.mirrored + stats.extreme_scale
        }))
    });
    selected.extend(
        unusual
            .into_iter()
            .take(5)
            .map(|record| record.entity_id.clone()),
    );

    selected.into_iter().collect()
}

struct Qualification {
    records: Vec<SceneRecord>,
    stable: bool,
    passes: usize,
    snapshot: Vec<Job>,
}

fn qualify_with<D, R>(opts: &Options, mut discovery: D, runner: &R) -> Qualification
where
    D: FnMut() -> Result<Vec<Job>>,
    R: Fn(Job, &Options) -> SceneRecord + Sync,
{
    let mut processed = HashSet::new();
    let mut records = Vec::new();
    let mut stable = false;
    let mut snapshot_after = Vec::new();
    let mut passes = 0;
    for pass in 1..=opts.snapshot_passes {
        passes = pass;
        let snapshot = match discovery() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                records.push(discovery_failure("snapshot-discovery", error));
                break;
            }
        };
        if snapshot.is_empty() {
            records.push(discovery_failure(
                "snapshot-empty",
                anyhow!("active deployment snapshot contained no scenes"),
            ));
            break;
        }
        let snapshot_keys: Vec<String> = snapshot.iter().map(Job::key).collect();
        let pending: Vec<Job> = snapshot
            .iter()
            .filter(|job| processed.insert(job.key()))
            .cloned()
            .collect();
        eprintln!(
            "qualification snapshot {pass}: {} active, {} pending, {} workers",
            snapshot.len(),
            pending.len(),
            opts.jobs
        );
        records.extend(process_with(pending, opts, runner));
        let check = match discovery() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                records.push(discovery_failure("snapshot-recheck", error));
                break;
            }
        };
        let check_keys: Vec<String> = check.iter().map(Job::key).collect();
        snapshot_after = check;
        if snapshot_keys == check_keys {
            stable = true;
            break;
        }
        eprintln!("qualification snapshot changed during pass {pass}; reconciling");
    }
    Qualification {
        records,
        stable,
        passes,
        snapshot: snapshot_after,
    }
}

fn map_u64(map: &serde_json::Map<String, serde_json::Value>, key: &str) -> u64 {
    map.get(key)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

fn qualification_exit(stable: bool, failed: usize) -> i32 {
    if stable && failed == 0 {
        0
    } else {
        1
    }
}

pub fn run(argv: &[String]) -> Result<i32> {
    let opts = parse(argv)?;
    std::fs::create_dir_all(&opts.out)?;
    std::fs::create_dir_all(&opts.cache)?;
    if let Some(parent) = opts
        .report
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    abgen::arm_gpu_default();
    let texture_encoder = match abgen::gpu_status() {
        Some(status) => TextureEncoderRecord {
            backend: status.backend.to_string(),
            qualified: status.qualified,
            reason: status.reason,
        },
        None => TextureEncoderRecord {
            backend: "cpu".to_string(),
            qualified: true,
            reason: None,
        },
    };

    let started_wall = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let started = Instant::now();
    let mut qualification = qualify_with(&opts, || discover(&opts), &run_one);
    qualification
        .records
        .sort_by(|a, b| (&a.catalyst, &a.entity_id).cmp(&(&b.catalyst, &b.entity_id)));
    let passed = qualification
        .records
        .iter()
        .filter(|record| record.ok)
        .count();
    let failed = qualification.records.len() - passed;
    let output_bytes = qualification
        .records
        .iter()
        .map(|record| record.bundle_bytes)
        .sum();
    let sum_io = |key| {
        qualification
            .records
            .iter()
            .map(|record| map_u64(&record.io, key))
            .sum()
    };
    let elapsed = started.elapsed();
    let snapshot_keys = qualification
        .snapshot
        .iter()
        .map(Job::key)
        .collect::<Vec<_>>()
        .join("\n");
    let report = Report {
        schema_version: 2,
        started_unix_ms: started_wall,
        catalyst: opts.catalyst.clone(),
        worlds_url: opts.worlds_url.clone(),
        platforms: opts.platforms.clone(),
        workers: opts.jobs,
        texture_encoder,
        max_attempts: opts.max_attempts,
        snapshot_limit: opts.snapshot_passes,
        snapshot_sha256: abgen::hashes::sha256_hex(snapshot_keys.as_bytes()),
        snapshot_passes: qualification.passes,
        snapshot_stable: qualification.stable,
        summary: Summary {
            discovered: qualification.snapshot.len(),
            processed: qualification.records.len(),
            passed,
            failed,
            elapsed_ms: elapsed.as_millis(),
            peak_rss_kib: peak_rss_kib(),
            output_bytes,
            network_requests: sum_io("network_requests"),
            network_bytes: sum_io("network_bytes"),
            cache_hits: sum_io("cache_hits"),
            cache_bytes: sum_io("cache_bytes"),
            scenes_per_second: if elapsed.as_secs_f64() == 0.0 {
                0.0
            } else {
                qualification.records.len() as f64 / elapsed.as_secs_f64()
            },
        },
        explorer_candidates: explorer_candidates(&qualification.records),
        scenes: qualification.records,
    };
    let bytes = serde_json::to_vec_pretty(&report)?;
    write_report(&opts.report, &bytes)?;
    println!(
        "qualification: {passed}/{} passed, {failed} failed, stable={}, {:.2} scenes/s, report={}",
        report.summary.processed,
        report.snapshot_stable,
        report.summary.scenes_per_second,
        opts.report.display()
    );
    Ok(qualification_exit(report.snapshot_stable, failed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn deployment_identity_is_exact() {
        assert!(deployment_identity_matches("bafy-a", "bafy-a"));
        assert!(!deployment_identity_matches("bafy-a", "bafy-b"));
        assert!(!deployment_identity_matches("BAFY-A", "bafy-a"));
    }

    #[test]
    fn artifact_records_relative_path_size_and_hash() {
        let root = std::env::temp_dir().join(format!(
            "abgen-artifact-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("nested")).unwrap();
        let path = root.join("nested/result.glb");
        std::fs::write(&path, b"lod-bytes").unwrap();
        let record = artifact(&root, &path, "glb", Some(2), Some("windows")).unwrap();
        assert_eq!(record.relative_path, "nested/result.glb");
        assert_eq!(record.bytes, 9);
        assert_eq!(record.sha256, abgen::hashes::sha256_hex(b"lod-bytes"));
        assert_eq!(record.level, Some(2));
        assert_eq!(record.platform.as_deref(), Some("windows"));
        std::fs::remove_dir_all(root).unwrap();
    }

    fn options(tag: &str) -> Options {
        let root = std::env::temp_dir().join(format!(
            "abgen-qualify-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        Options {
            catalyst: String::new(),
            worlds_url: String::new(),
            out: root.join("out"),
            report: root.join("qualification.json"),
            cache: root.join("cache"),
            jobs: 3,
            max_attempts: 1,
            snapshot_passes: 3,
            city_min: 0,
            city_max: 0,
            city: true,
            worlds: true,
            world_names: Vec::new(),
            entity_ids: Vec::new(),
            platforms: vec!["windows".to_string(), "mac".to_string()],
        }
    }

    fn job(id: &str) -> Job {
        Job {
            entity_id: id.to_string(),
            source: "test".to_string(),
            catalyst: "http://content".to_string(),
        }
    }

    fn record(job: Job, ok: bool) -> SceneRecord {
        SceneRecord {
            entity_id: job.entity_id,
            source: job.source,
            catalyst: job.catalyst,
            ok,
            attempts: 1,
            retry_errors: Vec::new(),
            encoder_backend: Some("meshopt".to_string()),
            elapsed_ms: 1,
            placement_source: Some("static-crdt".to_string()),
            placements: Some(PlacementRecord {
                count: 1,
                rotated: 0,
                non_uniform_scale: 0,
                mirrored: 0,
                extreme_scale: 0,
            }),
            material_count: 1,
            texture_count: 1,
            source_tris: Some(1),
            bundle_bytes: 1,
            io: serde_json::Map::new(),
            timing_ms: serde_json::Map::new(),
            simplify: Vec::new(),
            artifacts: Vec::new(),
            gates: Vec::new(),
            error: (!ok).then(|| "failed".to_string()),
        }
    }

    fn serve<F>(requests: usize, handler: F) -> String
    where
        F: Fn(usize, &str, &str) -> (u16, Vec<u8>) + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for (index, connection) in listener.incoming().take(requests).enumerate() {
                let mut stream = connection.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                let mut parts = request.split_ascii_whitespace();
                let method = parts.next().unwrap_or("");
                let path = parts.next().unwrap_or("");
                let mut content_len = 0;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    if header == "\r\n" {
                        break;
                    }
                    if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:")
                    {
                        content_len = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; content_len];
                reader.read_exact(&mut body).unwrap();
                let (status, response) = handler(index, method, path);
                write!(
                    stream,
                    "HTTP/1.1 {status} test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response.len()
                )
                .unwrap();
                stream.write_all(&response).unwrap();
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn url_component_is_stable_and_escapes_separators() {
        assert_eq!(component("my world.dcl.eth"), "my%20world.dcl.eth");
        assert_eq!(component("a/b"), "a%2Fb");
    }

    #[test]
    fn parses_timing_and_io_lines() {
        let got = numeric_fields(
            &[
                "other".into(),
                "timing: placements_ms=12 total_ms=91".into(),
            ],
            "timing: ",
        );
        assert_eq!(got["placements_ms"], 12);
        assert_eq!(got["total_ms"], 91);
    }

    #[test]
    fn transient_discovery_retries_then_succeeds() {
        let base = serve(2, |index, _, _| {
            if index == 0 {
                (503, b"busy".to_vec())
            } else {
                (200, br#"{"data":[]}"#.to_vec())
            }
        });
        let mut sleeps = 0;
        let value = get_json_with_sleep(&format!("{base}/index"), |_| sleeps += 1).unwrap();
        assert_eq!(value["data"], serde_json::json!([]));
        assert_eq!(sleeps, 1);
    }

    #[test]
    fn fake_city_and_world_inventories_are_deduplicated_by_deployment() {
        let city = serde_json::json!([{
            "id": "city-scene", "type": "scene", "pointers": ["0,0"],
            "content": [], "metadata": {}
        }]);
        let worlds = serde_json::json!({"data": [{
            "name": "one.dcl.eth",
            "scenes": [{"id": "world-scene"}, {"id": "world-scene"}]
        }]});
        let base = serve(2, move |_, method, path| match (method, path) {
            ("POST", "/content/entities/active") => (200, city.to_string().into_bytes()),
            ("GET", "/index") => (200, worlds.to_string().into_bytes()),
            _ => (404, Vec::new()),
        });
        let mut opts = options("inventories");
        opts.catalyst = format!("{base}/content");
        opts.worlds_url = base;
        let jobs = discover(&opts).unwrap();
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs.iter().filter(|job| job.source == "city").count(), 1);
        assert_eq!(
            jobs.iter()
                .filter(|job| job.source.starts_with("world:"))
                .count(),
            1
        );
    }

    #[test]
    fn snapshot_churn_is_reconciled_before_success() {
        let opts = options("churn");
        let mut call = 0;
        let seen = Mutex::new(Vec::new());
        let result = qualify_with(
            &opts,
            || {
                call += 1;
                Ok(if call == 1 {
                    vec![job("a")]
                } else {
                    vec![job("a"), job("b")]
                })
            },
            &|job, _| {
                seen.lock().unwrap().push(job.entity_id.clone());
                record(job, true)
            },
        );
        assert!(result.stable);
        assert_eq!(result.passes, 2);
        assert_eq!(result.records.len(), 2);
        let mut seen = seen.into_inner().unwrap();
        seen.sort();
        assert_eq!(seen, ["a", "b"]);
    }

    #[test]
    fn scene_generation_retries_only_transient_failures() {
        let mut opts = options("scene-retry");
        opts.max_attempts = 3;
        let mut attempts = 0;
        let mut sleeps = 0;
        let result = run_with_retry(
            job("retry"),
            &opts,
            |job, _| {
                attempts += 1;
                if attempts == 1 {
                    Err(anyhow!("HTTP 503 temporary"))
                } else {
                    Ok(record(job.clone(), true))
                }
            },
            |_| sleeps += 1,
        );
        assert!(result.ok);
        assert_eq!(result.attempts, 2);
        assert_eq!(result.retry_errors.len(), 1);
        assert_eq!(sleeps, 1);

        attempts = 0;
        let result = run_with_retry(
            job("malformed"),
            &opts,
            |_, _| {
                attempts += 1;
                Err(anyhow!("malformed glb"))
            },
            |_| panic!("non-transient errors must not sleep"),
        );
        assert!(!result.ok);
        assert_eq!(attempts, 1);
        assert_eq!(result.attempts, 1);
    }

    #[test]
    fn processing_never_exceeds_the_configured_bound() {
        let opts = options("bounded");
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let jobs = (0..12).map(|index| job(&index.to_string())).collect();
        let records = process_with(jobs, &opts, &|job, _| {
            let now = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(5));
            active.fetch_sub(1, Ordering::SeqCst);
            record(job, true)
        });
        assert_eq!(records.len(), 12);
        assert!(peak.load(Ordering::SeqCst) <= opts.jobs);
        assert!(peak.load(Ordering::SeqCst) > 1);
    }

    #[test]
    fn zero_failure_policy_and_unstable_snapshot_are_fail_closed() {
        assert_eq!(qualification_exit(true, 0), 0);
        assert_eq!(qualification_exit(true, 1), 1);
        assert_eq!(qualification_exit(false, 0), 1);

        let mut opts = options("unstable");
        opts.snapshot_passes = 1;
        let mut call = 0;
        let result = qualify_with(
            &opts,
            || {
                call += 1;
                Ok(if call == 1 {
                    vec![job("a")]
                } else {
                    vec![job("b")]
                })
            },
            &|job, _| record(job, true),
        );
        assert!(!result.stable);
    }

    #[test]
    fn report_write_is_atomic_and_versioned_json() {
        let opts = options("report");
        std::fs::create_dir_all(opts.report.parent().unwrap()).unwrap();
        let bytes = br#"{"schema_version":2,"snapshot_stable":false}"#;
        write_report(&opts.report, bytes).unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&opts.report).unwrap()).unwrap();
        assert_eq!(value["schema_version"], 2);
        assert_eq!(value["snapshot_stable"], false);
        assert!(!opts.report.with_extension("json.tmp").exists());
        let _ = std::fs::remove_dir_all(opts.report.parent().unwrap());
    }

    #[test]
    fn defaults_are_memory_aware_and_retries_are_bounded() {
        let opts = parse(&["--out".into(), "/tmp/qualify".into()]).unwrap();
        assert_eq!(opts.jobs, abgen::clihelp::default_file_concurrency());
        assert_eq!(opts.max_attempts, DEFAULT_ATTEMPTS);
        assert_eq!(opts.snapshot_passes, DEFAULT_SNAPSHOT_PASSES);
        assert!(parse(&[
            "--out".into(),
            "/tmp/q".into(),
            "--attempts".into(),
            "0".into()
        ])
        .is_err());
    }
}

use abgen::lodgen::{gate_failures, GenerateParams};
use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use std::collections::{BTreeSet, HashSet, VecDeque};
use std::io::Read;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_CATALYST: &str = "https://catalyst.dcl.one/content";
const DEFAULT_WORLDS: &str = "https://worlds-content-server.decentraland.org";
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
    city_min: i32,
    city_max: i32,
    city: bool,
    worlds: bool,
    world_names: Vec<String>,
    entity_ids: Vec<String>,
    platforms: Vec<String>,
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
    elapsed_ms: u128,
    placement_source: Option<String>,
    source_tris: Option<usize>,
    bundle_bytes: usize,
    io: serde_json::Map<String, serde_json::Value>,
    timing_ms: serde_json::Map<String, serde_json::Value>,
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
    scenes_per_second: f64,
}

#[derive(Serialize)]
struct Report {
    schema_version: u32,
    started_unix_ms: u128,
    catalyst: String,
    worlds_url: String,
    platforms: Vec<String>,
    workers: usize,
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
    let mut jobs = abgen::clihelp::default_network_concurrency().max(1);
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
        city_min,
        city_max,
        city,
        worlds,
        world_names,
        entity_ids,
        platforms,
    })
}

fn get_json(url: &str) -> Result<serde_json::Value> {
    let response = ureq::get(url)
        .config()
        .timeout_global(Some(std::time::Duration::from_secs(120)))
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

fn discovery_failure(stage: &str, error: anyhow::Error) -> SceneRecord {
    SceneRecord {
        entity_id: "<discovery>".to_string(),
        source: stage.to_string(),
        catalyst: String::new(),
        ok: false,
        elapsed_ms: 0,
        placement_source: None,
        source_tris: None,
        bundle_bytes: 0,
        io: serde_json::Map::new(),
        timing_ms: serde_json::Map::new(),
        gates: Vec::new(),
        error: Some(format!("{error:#}")),
    }
}

fn run_one(job: Job, opts: &Options) -> SceneRecord {
    let started = Instant::now();
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
    match abgen::lodgen::generate(&params) {
        Ok(outcome) => {
            let failed = gate_failures(&outcome.gate);
            let placement_source = outcome
                .log
                .iter()
                .find_map(|line| line.strip_prefix("placement-source: ").map(str::to_string));
            let bundle_bytes = outcome.levels.iter().map(|v| v.bundle_bytes).sum();
            SceneRecord {
                entity_id: job.entity_id,
                source: job.source,
                catalyst: job.catalyst,
                ok: failed == 0,
                elapsed_ms: started.elapsed().as_millis(),
                placement_source,
                source_tris: Some(outcome.source_tris),
                bundle_bytes,
                io: numeric_fields(&outcome.log, "io: "),
                timing_ms: numeric_fields(&outcome.log, "timing: "),
                gates: outcome
                    .gate
                    .into_iter()
                    .map(|g| GateRecord {
                        label: g.label,
                        ok: g.ok,
                        detail: g.detail,
                    })
                    .collect(),
                error: (failed > 0).then(|| format!("{failed} self-gate checks failed")),
            }
        }
        Err(error) => SceneRecord {
            entity_id: job.entity_id,
            source: job.source,
            catalyst: job.catalyst,
            ok: false,
            elapsed_ms: started.elapsed().as_millis(),
            placement_source: None,
            source_tris: None,
            bundle_bytes: 0,
            io: serde_json::Map::new(),
            timing_ms: serde_json::Map::new(),
            gates: Vec::new(),
            error: Some(format!("{error:#}")),
        },
    }
}

fn process(jobs: Vec<Job>, opts: &Options) -> Vec<SceneRecord> {
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
                let _ = tx.send(run_one(job, opts));
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
    for metric in 0..3 {
        let mut rows: Vec<&SceneRecord> = records.iter().filter(|v| v.ok).collect();
        rows.sort_by_key(|v| {
            std::cmp::Reverse(match metric {
                0 => v.elapsed_ms as usize,
                1 => v.source_tris.unwrap_or(0),
                _ => v.bundle_bytes,
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
    selected.into_iter().collect()
}

pub fn run(argv: &[String]) -> Result<i32> {
    let opts = parse(argv)?;
    std::fs::create_dir_all(&opts.out)?;
    std::fs::create_dir_all(&opts.cache)?;
    if let Some(parent) = opts.report.parent().filter(|v| !v.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    abgen::arm_gpu_default();

    let started_wall = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let started = Instant::now();
    let mut processed = HashSet::new();
    let mut records = Vec::new();
    let mut stable = false;
    let mut previous = Vec::new();
    let mut passes = 0usize;
    for pass in 1..=3 {
        passes = pass;
        let snapshot = match discover(&opts) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                records.push(discovery_failure("snapshot-discovery", error));
                break;
            }
        };
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
        records.extend(process(pending, &opts));
        let check = match discover(&opts) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                records.push(discovery_failure("snapshot-recheck", error));
                break;
            }
        };
        let check_keys: Vec<String> = check.iter().map(Job::key).collect();
        if snapshot_keys == check_keys {
            stable = true;
            previous = check;
            break;
        }
        previous = check;
        eprintln!("qualification snapshot changed during pass {pass}; reconciling");
    }
    records.sort_by(|a, b| (&a.catalyst, &a.entity_id).cmp(&(&b.catalyst, &b.entity_id)));
    let passed = records.iter().filter(|v| v.ok).count();
    let failed = records.len() - passed;
    let output_bytes = records.iter().map(|v| v.bundle_bytes).sum();
    let elapsed = started.elapsed();
    let report = Report {
        schema_version: 1,
        started_unix_ms: started_wall,
        catalyst: opts.catalyst.clone(),
        worlds_url: opts.worlds_url.clone(),
        platforms: opts.platforms.clone(),
        workers: opts.jobs,
        snapshot_passes: passes,
        snapshot_stable: stable,
        summary: Summary {
            discovered: previous.len(),
            processed: records.len(),
            passed,
            failed,
            elapsed_ms: elapsed.as_millis(),
            peak_rss_kib: peak_rss_kib(),
            output_bytes,
            scenes_per_second: if elapsed.as_secs_f64() == 0.0 {
                0.0
            } else {
                records.len() as f64 / elapsed.as_secs_f64()
            },
        },
        explorer_candidates: explorer_candidates(&records),
        scenes: records,
    };
    let bytes = serde_json::to_vec_pretty(&report)?;
    write_report(&opts.report, &bytes)?;
    println!(
        "qualification: {passed}/{} passed, {failed} failed, stable={stable}, {:.2} scenes/s, report={}",
        report.summary.processed,
        report.summary.scenes_per_second,
        opts.report.display()
    );
    Ok(if stable && failed == 0 { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_component_is_stable_and_escapes_separators() {
        assert_eq!(component("my world.dcl.eth"), "my%20world.dcl.eth");
        assert_eq!(component("a/b"), "a%2Fb");
    }

    #[test]
    fn parses_timing_line() {
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
}

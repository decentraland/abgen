//! Times conversion over real entities, per file and per stage.
//!
//! Attribution rides the progress stream rather than instrumenting the
//! pipeline: `export::run` already emits a discriminated `ev` for every phase
//! it enters, so a sink that timestamps what it is handed reconstructs where
//! the time went without a single probe in the converter. Nothing here can
//! therefore drift out of step with the code it measures, and nothing here
//! costs anything in the shipped hosts.
//!
//! Two ways in. Entity ids read the synced content store, which is where any
//! representative corpus actually lives; directories are for a scene project on
//! disk, where the files are already laid out by name.
//!
//! ```sh
//! abgen-bench --entity-ids ids.txt --content-root ~/content
//! abgen-bench --reps 3 ./my-scene/          # human-readable
//! abgen-bench --json ./my-scene/ > run.json # for diffing two builds
//! ```
//!
//! Reports the MINIMUM across reps, not the mean: a conversion cannot run
//! faster than its true cost, so every sample is that cost plus noise, and the
//! smallest sample is the closest estimate. Means chase whatever else the
//! machine was doing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use abgen::export::{self, HostInfo, InputBuilder, Kind, Sink};
use abgen::hashes;
use abgen::local_store::{LocalContentStore, ABGEN_CONTENT_ROOT_ENV, DEFAULT_CONTENT_ROOT};

const HOST: HostInfo = HostInfo::new("v-abgen-bench", "bench://local");

/// One entity's content: path inside the entity, and the bytes at it.
type EntityFiles = Vec<(String, Vec<u8>)>;
/// What to time: a label to report it under, and the files it converts.
type BenchJob = (String, EntityFiles);

/// A sink that keeps when things happened and discards what they said.
struct TimingSink {
    start: Instant,
    marks: Mutex<Vec<(Duration, String, Option<String>)>>,
    bytes_out: Mutex<u64>,
    bundles: Mutex<usize>,
}

impl TimingSink {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            marks: Mutex::new(Vec::new()),
            bytes_out: Mutex::new(0),
            bundles: Mutex::new(0),
        }
    }
}

impl Sink for TimingSink {
    fn emit_output(&self, _name: &str, data: &[u8]) {
        if let Ok(mut n) = self.bytes_out.lock() {
            *n += data.len() as u64;
        }
        if let Ok(mut n) = self.bundles.lock() {
            *n += 1;
        }
    }

    fn emit(&self, kind: Kind, bytes: &[u8]) {
        if kind == Kind::Output {
            if let Ok(mut n) = self.bytes_out.lock() {
                *n += bytes.len() as u64;
            }
            return;
        }
        if kind != Kind::Json {
            return;
        }
        let at = self.start.elapsed();
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) else {
            return;
        };
        let Some(ev) = v.get("ev").and_then(|e| e.as_str()) else {
            return;
        };
        let subject = v
            .get("file")
            .or_else(|| v.get("name"))
            .and_then(|f| f.as_str())
            .map(str::to_string);
        if let Ok(mut m) = self.marks.lock() {
            m.push((at, ev.to_string(), subject));
        }
    }
}

/// A sink that keeps only what the bundles were: (name, sha256) per output.
///
/// Runs in its own untimed pass so `--out-hash` never inflates the numbers
/// the timed reps report. Every bundle reaches a sink exactly once, through
/// `emit_output`; the `Kind::Output` arm below only matters if a future
/// emitter bypasses that helper, and synthesizes a name so such a bundle
/// still lands in the digest instead of silently vanishing from it.
#[derive(Default)]
struct HashSink {
    pairs: Mutex<Vec<(String, String)>>,
    anon: Mutex<u64>,
}

impl Sink for HashSink {
    fn emit_output(&self, name: &str, data: &[u8]) {
        if let Ok(mut p) = self.pairs.lock() {
            p.push((name.to_string(), hashes::sha256_hex(data)));
        }
    }

    fn emit(&self, kind: Kind, bytes: &[u8]) {
        if kind != Kind::Output {
            return;
        }
        let n = match self.anon.lock() {
            Ok(mut a) => {
                *a += 1;
                *a
            }
            Err(_) => return,
        };
        if let Ok(mut p) = self.pairs.lock() {
            p.push((format!("!raw-output-{n}"), hashes::sha256_hex(bytes)));
        }
    }
}

/// One entity's conversion, timed.
struct Run {
    total: Duration,
    /// file-start to file-done, per converted file.
    per_file: BTreeMap<String, Duration>,
    /// Wall time between one event kind and the next, summed.
    per_stage: BTreeMap<String, Duration>,
    bytes_out: u64,
    bundles: usize,
    code: i32,
}

fn build_request(files: &[(String, Vec<u8>)], platform: &str) -> Vec<u8> {
    let mut builder = InputBuilder::new().platform(platform);
    for (name, data) in files {
        builder = builder.file(name.clone(), data.clone());
    }
    builder.build()
}

fn time_once(files: &[(String, Vec<u8>)], platform: &str) -> Run {
    // Clear per rep: caching is enabled (see main()) so intra-entity
    // duplicate textures get deduped like production, but a rep must not
    // see hits from the *previous* rep's identical input, or "minimum
    // across reps" degenerates into a warm-cache measurement.
    abgen::texencode_cache::clear();
    abgen::decode_cache::clear();

    let request = build_request(files, platform);

    let sink = TimingSink::new();
    let began = Instant::now();
    let code = export::run(&request, &sink, HOST);
    let total = began.elapsed();

    let marks = sink.marks.into_inner().unwrap_or_default();
    let bytes_out = sink.bytes_out.into_inner().unwrap_or(0);
    let bundles = sink.bundles.into_inner().unwrap_or(0);

    let mut per_file = BTreeMap::new();
    let mut open: BTreeMap<String, Duration> = BTreeMap::new();
    let mut per_stage: BTreeMap<String, Duration> = BTreeMap::new();

    for (i, (at, ev, subject)) in marks.iter().enumerate() {
        match ev.as_str() {
            "file-start" => {
                if let Some(s) = subject {
                    open.insert(s.clone(), *at);
                }
            }
            "file-done" | "file-error" => {
                if let Some(s) = subject {
                    if let Some(from) = open.remove(s) {
                        per_file.insert(s.clone(), at.saturating_sub(from));
                    }
                }
            }
            _ => {}
        }
        let next = marks.get(i + 1).map(|(t, _, _)| *t).unwrap_or(total);
        *per_stage.entry(ev.clone()).or_default() += next.saturating_sub(*at);
    }

    Run {
        total,
        per_file,
        per_stage,
        bytes_out,
        bundles,
        code,
    }
}

fn read_entity(dir: &Path) -> std::io::Result<EntityFiles> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let rel = path
                .strip_prefix(dir)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            files.push((rel, std::fs::read(&path)?));
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

/// Rebuilds one entity's files from the content store.
///
/// The store is content-addressed, so the entity json is itself an object in
/// it, and its `content` table is what maps a hash back to the name the
/// converter expects. Same shape `abgen-corpus --entity-ids` reads, so the two
/// tools take the same id lists.
fn read_entity_id(
    store: &LocalContentStore,
    entity_id: &str,
) -> Result<Vec<(String, Vec<u8>)>, String> {
    let raw = store
        .fetch(entity_id)
        .map_err(|e| format!("entity json absent from the content store: {e}"))?;
    let json: serde_json::Value =
        serde_json::from_slice(&raw).map_err(|e| format!("entity json is not json: {e}"))?;
    let scene = abgen::catalyst::CatalystClient::parse_entity(&json)
        .map_err(|e| format!("entity json did not parse: {e}"))?;

    let mut files = Vec::with_capacity(scene.content.len());
    for entry in &scene.content {
        let data = store.fetch(&entry.hash).map_err(|e| {
            format!(
                "{} ({}) absent from the content store: {e}",
                entry.file, entry.hash
            )
        })?;
        files.push((entry.file.clone(), data));
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

/// One id per line; blanks and `#` comments ignored, as abgen-corpus reads them.
fn read_id_list(path: &Path) -> std::io::Result<Vec<String>> {
    Ok(std::fs::read_to_string(path)?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect())
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn main() {
    let mut reps = 3usize;
    let mut json = false;
    let mut out_hash = false;
    let mut platform = "windows".to_string();
    let mut targets: Vec<PathBuf> = Vec::new();
    let mut id_list: Option<PathBuf> = None;
    let mut content_root: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--reps" => reps = args.next().and_then(|v| v.parse().ok()).unwrap_or(3).max(1),
            "--platform" => platform = args.next().unwrap_or_else(|| "windows".into()),
            "--entity-ids" => id_list = args.next().map(PathBuf::from),
            "--content-root" => content_root = args.next(),
            "--json" => json = true,
            "--out-hash" => out_hash = true,
            "-h" | "--help" => {
                eprintln!(
                    "usage:\n  \
                     abgen-bench [--reps N] [--platform P] [--json] [--out-hash] <entity-dir>...\n  \
                     abgen-bench [--reps N] [--platform P] [--json] [--out-hash] \\\n               \
                       --entity-ids <ids.txt> [--content-root <dir>]\n\
                     \n\
                     Times conversion, reporting the minimum of N repetitions per\n\
                     entity, broken down by file and by stage.\n\
                     \n\
                     --out-hash adds one untimed pass per entity and prints a single\n\
                     digest over every output bundle: two builds are byte-identical\n\
                     on this corpus iff they print the same digest.\n\
                     \n\
                     --content-root defaults to ${} then {}.",
                    ABGEN_CONTENT_ROOT_ENV, DEFAULT_CONTENT_ROOT
                );
                return;
            }
            _ => targets.push(PathBuf::from(a)),
        }
    }

    if id_list.is_some() && !targets.is_empty() {
        eprintln!("abgen-bench: --entity-ids and entity directories are alternatives, not a union");
        std::process::exit(2);
    }

    // Same bounded caches the lambda/live paths use, so a bench run reflects
    // production's intra-entity texture dedup. time_once() clears both
    // before every rep — otherwise reps 2..N would measure cache hits
    // instead of the conversion, and "minimum across reps" would report a
    // warm-cache number instead of the true cost. The in-memory clear can't
    // reach the encode cache's on-disk backing though, so turn that off
    // here specifically: a rep hitting a *previous rep's* on-disk entry
    // would silently corrupt the "minimum across reps" measurement the same
    // way an uncleared in-memory hit would, just persistently.
    if std::env::var("ABGEN_DISK_CACHE").is_err() {
        std::env::set_var("ABGEN_DISK_CACHE", "0");
    }
    abgen::texencode_cache::enable();
    abgen::decode_cache::enable();

    let mut jobs: Vec<BenchJob> = Vec::new();
    if let Some(list) = &id_list {
        let root = content_root
            .or_else(|| std::env::var(ABGEN_CONTENT_ROOT_ENV).ok())
            .unwrap_or_else(|| DEFAULT_CONTENT_ROOT.to_string());
        let store = LocalContentStore::new(&root);
        let ids = match read_id_list(list) {
            Ok(ids) => ids,
            Err(e) => {
                eprintln!("abgen-bench: {}: {e}", list.display());
                std::process::exit(2);
            }
        };
        for id in ids {
            match read_entity_id(&store, &id) {
                Ok(files) if !files.is_empty() => jobs.push((id, files)),
                Ok(_) => eprintln!("abgen-bench: {id} has no content; skipped"),
                Err(e) => eprintln!("abgen-bench: {id}: {e}; skipped"),
            }
        }
    } else {
        for target in &targets {
            match read_entity(target) {
                Ok(f) if !f.is_empty() => jobs.push((target.display().to_string(), f)),
                Ok(_) => eprintln!("abgen-bench: {} is empty; skipped", target.display()),
                Err(e) => eprintln!("abgen-bench: {}: {e}", target.display()),
            }
        }
    }

    if jobs.is_empty() {
        eprintln!("abgen-bench: nothing to convert (--help for usage)");
        std::process::exit(2);
    }

    let mut report = Vec::new();
    let mut out_pairs: Vec<(String, String)> = Vec::new();
    for (target, files) in &jobs {
        let input_bytes: u64 = files.iter().map(|(_, d)| d.len() as u64).sum();

        let mut best: Option<Run> = None;
        for _ in 0..reps {
            let run = time_once(files, &platform);
            if best.as_ref().is_none_or(|b| run.total < b.total) {
                best = Some(run);
            }
        }
        let Some(best) = best else { continue };

        if out_hash {
            let request = build_request(files, &platform);
            let sink = HashSink::default();
            let code = export::run(&request, &sink, HOST);
            if code != 0 {
                eprintln!("abgen-bench: {target}: hash pass exited {code}");
            }
            for (name, hex) in sink.pairs.into_inner().unwrap_or_default() {
                out_pairs.push((format!("{target}/{name}"), hex));
            }
        }

        report.push((target.clone(), input_bytes, best));
    }

    let out_hash_hex = out_hash.then(|| {
        out_pairs.sort();
        let mut cat = String::new();
        for (name, hex) in &out_pairs {
            cat.push_str(name);
            cat.push(':');
            cat.push_str(hex);
            cat.push('\n');
        }
        hashes::sha256_hex(cat.as_bytes())
    });
    if let Some(d) = &out_hash_hex {
        eprintln!("out-hash: {d}");
    }

    if json {
        let out: Vec<_> = report
            .iter()
            .map(|(t, input_bytes, r)| {
                serde_json::json!({
                    "entity": t,
                    "reps": reps,
                    "platform": platform,
                    "code": r.code,
                    "totalMs": ms(r.total),
                    "inputBytes": input_bytes,
                    "outputBytes": r.bytes_out,
                    "bundles": r.bundles,
                    "perFileMs": r.per_file.iter()
                        .map(|(k, v)| (k.clone(), ms(*v)))
                        .collect::<BTreeMap<_, _>>(),
                    "perStageMs": r.per_stage.iter()
                        .map(|(k, v)| (k.clone(), ms(*v)))
                        .collect::<BTreeMap<_, _>>(),
                })
            })
            .collect();
        let mut top = serde_json::json!({
            "buildId": option_env!("ABGEN_BUILD_ID").unwrap_or("devbuild0000"),
            "runs": out,
        });
        if let Some(d) = &out_hash_hex {
            top["out_hash"] = serde_json::Value::String(d.clone());
        }
        println!("{}", serde_json::to_string_pretty(&top).unwrap_or_default());
        return;
    }

    for (target, input_bytes, r) in &report {
        println!(
            "{}  {:.0}ms  in {:.1}MB  out {:.1}MB{}",
            target,
            ms(r.total),
            *input_bytes as f64 / 1e6,
            r.bytes_out as f64 / 1e6,
            if r.code == 0 { "" } else { "  (FAILED)" }
        );
        let mut stages: Vec<_> = r.per_stage.iter().collect();
        stages.sort_by(|a, b| b.1.cmp(a.1));
        for (ev, d) in stages.iter().take(8) {
            println!("    {:>7.0}ms  {}", ms(**d), ev);
        }
        let mut files: Vec<_> = r.per_file.iter().collect();
        files.sort_by(|a, b| b.1.cmp(a.1));
        for (name, d) in files.iter().take(8) {
            println!("    {:>7.0}ms  {}", ms(**d), name);
        }
    }
}

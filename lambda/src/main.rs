//! abgen-lambda — the asset-bundle conversion pipeline as one binary, shaped
//! for AWS Lambda (custom runtime on `provided.al2023`, container image).
//!
//! One invocation handles one deployment event end to end:
//!
//!   1. parse the event — an SQS record batch whose bodies are catalyst
//!      `DeploymentToSqs` payloads, or a plain
//!      `{"entityId": "...", "contentServerUrl": "..."}` for manual invokes,
//!   2. skip platforms whose manifest is already current on the CDN,
//!   3. convert the remaining platforms in one process with the
//!      texture-encode cache enabled — the mac pass reuses the windows
//!      pass's BC7/DXT encodes ("dual-emit") — while abgen's space probes
//!      per-file asset reuse and writes bundles + manifests through to S3
//!      during the build,
//!   4. publish scene sources (`main.crdt`, `scene.json`, main script),
//!   5. notify the asset-bundle-registry SQS queue (deferred — registry
//!      duplicate ships as a follow-up).
//!
//! No async runtime: the Lambda runtime API is a plain HTTP long-poll, served
//! with the same blocking ureq the rest of abgen uses. Cold start is the
//! binary's exec time.
//!
//! Modes:
//!   abgen-lambda                   serve the Lambda runtime API (requires
//!                                  AWS_LAMBDA_RUNTIME_API, set by Lambda)
//!   abgen-lambda --once FILE.json  handle one event read from a file, print
//!                                  the result JSON and exit — full local run
//!                                  without any AWS.

mod catalyst;
mod config;
mod convert;
mod event;
mod notify;
mod output;
mod runtime;

use anyhow::{Context, Result};

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.first().map(String::as_str) {
        Some("--once") => {
            let Some(path) = argv.get(1) else {
                eprintln!("--once needs an event file path");
                std::process::exit(2);
            };
            init();
            let mut cfg = config::Config::from_env();
            // Local runs keep the corpus on disk for inspection.
            cfg.keep_output = true;
            match run_once(&cfg, path) {
                Ok(v) => {
                    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                }
                Err(e) => {
                    eprintln!("error: {e:#}");
                    std::process::exit(1);
                }
            }
        }
        Some("-h") | Some("--help") => {
            eprintln!(
                "usage: abgen-lambda [--once EVENT.json]\n\
                 \n\
                 With no arguments, serves the AWS Lambda runtime API\n\
                 (AWS_LAMBDA_RUNTIME_API must be set — Lambda sets it).\n\
                 --once EVENT.json handles a single event locally and exits.\n\
                 \n\
                 env: PLATFORMS (windows,mac), AB_VERSION (v49), ABGEN_CACHE_DIR,\n\
                 \x20    CONTENT_SERVER_URL, OUT_ROOT, KEEP_OUTPUT, REGISTRY_QUEUE_URL,\n\
                 \x20    ABGEN_S3_ENDPOINT, ABGEN_S3_BUCKET, ABGEN_S3_REGION,\n\
                 \x20    ABGEN_S3_PATH_STYLE, ABGEN_S3_READ_ONLY (+ AWS credentials)"
            );
        }
        Some(other) => {
            eprintln!("unknown argument: {other} (try --help)");
            std::process::exit(2);
        }
        None => {
            init();
            let cfg = config::Config::from_env();
            runtime::serve(&cfg, handle);
        }
    }
}

/// One-time process setup shared by both modes.
fn init() {
    abgen::builder::require_templates().unwrap_or_else(|e| {
        eprintln!(
            "fatal: build templates unavailable ({}): {e:#}",
            abgen::builder::template_source()
        );
        std::process::exit(1);
    });
    // Lambda has no GPU; this warns once and settles on the CPU encoders.
    abgen::arm_gpu_default();
    // Dual-emit: the second platform's texture encodes become cache hits.
    abgen::texencode_cache::enable();
}

fn run_once(cfg: &config::Config, path: &str) -> Result<serde_json::Value> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read event file {path}"))?;
    let event: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("parse event file {path} as JSON"))?;
    handle(cfg, &event)
}

/// The Lambda handler: event in, summary JSON out. An `Err` is reported to
/// the runtime API as a function error, so the SQS message retries and
/// eventually lands in the DLQ.
fn handle(cfg: &config::Config, event: &serde_json::Value) -> Result<serde_json::Value> {
    let jobs = event::jobs_from_event(event)?;
    let mut summaries = Vec::with_capacity(jobs.len());
    for job in &jobs {
        summaries.push(handle_job(cfg, job)?);
    }
    Ok(serde_json::json!({ "jobs": summaries }))
}

fn handle_job(cfg: &config::Config, job: &event::Job) -> Result<serde_json::Value> {
    if job.is_lods {
        // LOD generation stays on the Unity pipeline; these arrive only if the
        // queue subscription filter lets them through. Succeed so they are not
        // retried into the DLQ.
        eprintln!("skip: LOD job for {} (unsupported here)", job.entity_id);
        return Ok(serde_json::json!({
            "entityId": job.entity_id, "skipped": "lods-unsupported"
        }));
    }

    let content_server = job
        .content_server_url
        .as_deref()
        .unwrap_or(&cfg.default_content_server);
    // SSRF guard: the content server travels in the attacker-influenced SQS
    // payload; with an allowlist configured, off-list hosts are rejected
    // (message retries → DLQ, where the attempt is visible).
    if let Some(allowed) = &cfg.allowed_content_server_hosts {
        event::ensure_allowed_content_server(content_server, allowed)?;
    }
    let proxy = convert::make_proxy(cfg, content_server);

    // Entity-level already-converted skip (consumer-server semantics): a
    // platform whose manifest exists with exitCode 0 at the current AB
    // version needs no work; partially-converted entities rebuild only the
    // missing targets. Finer-grained per-FILE reuse happens inside the build
    // itself (the space probe). `force` bypasses this manifest check, but
    // per-file reuse still applies — existing canonical bundles are never
    // overwritten.
    let mut pending: Vec<String> = cfg.platforms.clone();
    if !job.force {
        pending.retain(|platform| {
            let done = output::platform_converted(&proxy, cfg, &job.entity_id, platform);
            if done {
                eprintln!(
                    "skip: {} {platform} already converted at {}",
                    job.entity_id, cfg.version
                );
            }
            !done
        });
        if pending.is_empty() {
            return Ok(serde_json::json!({
                "entityId": job.entity_id, "skipped": "already-converted"
            }));
        }
    }

    // The entity document drives scene-source publishing (and records the
    // entity type in the summary).
    let agent = catalyst::agent();
    let entity_doc = catalyst::fetch_entity(&agent, content_server, &job.entity_id)?;

    let outcome = convert::convert_entity(cfg, &proxy, &job.entity_id, content_server, &pending)?;

    // Bundles + manifests were written through to the space during the build;
    // publish() adds scene sources. Then drop the local corpus (Lambda /tmp
    // is 10 GB, shared across warm invocations) unless a local run wants it.
    let published = output::publish(cfg, &agent, &proxy, &entity_doc, &outcome)
        .and_then(|upload| notify::send(cfg, &outcome).map(|notified| (upload, notified)));
    if !cfg.keep_output {
        let _ = std::fs::remove_dir_all(&outcome.cid_dir);
    }
    let (upload, notified) = published?;

    eprintln!(
        "done: {} platforms={} exitCode={} texcache hits={} misses={}",
        outcome.entity_id,
        outcome
            .platforms
            .iter()
            .map(|p| format!("{}:{}", p.platform, p.built.len()))
            .collect::<Vec<_>>()
            .join(","),
        outcome.exit_code(),
        outcome.cache_hits,
        outcome.cache_misses,
    );

    Ok(serde_json::json!({
        "entityId": outcome.entity_id,
        "contentServer": outcome.content_server,
        "exitCode": outcome.exit_code(),
        "platforms": outcome.platforms.iter().map(|p| serde_json::json!({
            "platform": p.platform,
            "bundles": p.built.len(),
            "exitCode": p.exit_code,
        })).collect::<Vec<_>>(),
        "texEncodeCache": { "hits": outcome.cache_hits, "misses": outcome.cache_misses },
        "upload": upload,
        "notified": notified,
    }))
}

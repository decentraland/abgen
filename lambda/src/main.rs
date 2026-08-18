//! abgen-lambda — the asset-bundle conversion pipeline as one binary, shaped
//! for AWS Lambda (custom runtime on `provided.al2023`, container image).
//!
//! One invocation handles one deployment event end to end:
//!
//!   1. parse the event — an SQS record batch whose bodies are catalyst
//!      `DeploymentToSqs` payloads, or a plain
//!      `{"entityId": "...", "contentServerUrl": "..."}` for manual invokes,
//!   2. skip work that is already on the CDN            (TODO: step 5),
//!   3. convert the entity for every configured platform in one process with
//!      the texture-encode cache enabled — the mac pass reuses the windows
//!      pass's BC7/DXT encodes ("dual-emit"),
//!   4. brotli + upload bundles and manifests to S3     (TODO: step 3),
//!   5. notify the asset-bundle-registry SQS queue      (TODO: step 4).
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
            let cfg = config::Config::from_env();
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
                 \x20    CONTENT_SERVER_URL, OUT_ROOT, S3_BUCKET, REGISTRY_QUEUE_URL"
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

    // TODO(step 5): manifest HEAD on the CDN — return early when every
    // configured platform is already converted.

    let content_server = job
        .content_server_url
        .as_deref()
        .unwrap_or(&cfg.default_content_server);
    let outcome = convert::convert_entity(cfg, &job.entity_id, content_server)?;

    let upload = output::publish(cfg, &outcome)?;
    let notified = notify::send(cfg, &outcome)?;

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

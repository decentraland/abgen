mod catalyst;
mod config;
mod convert;
mod emf;
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
            cfg.keep_output = true;
            let result = run_once(&cfg, path);
            emf::flush();
            match result {
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
                 \x20    ABGEN_S3_PATH_STYLE, ABGEN_S3_READ_ONLY (+ AWS credentials),\n\
                 \x20    ABGEN_EMF_NAMESPACE (CloudWatch EMF metrics on stdout)"
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

fn init() {
    emf::init();
    abgen::builder::require_templates().unwrap_or_else(|e| {
        eprintln!(
            "fatal: build templates unavailable ({}): {e:#}",
            abgen::builder::template_source()
        );
        std::process::exit(1);
    });
    abgen::arm_gpu_default();
    abgen::texencode_cache::enable();
}

fn run_once(cfg: &config::Config, path: &str) -> Result<serde_json::Value> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read event file {path}"))?;
    let event: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("parse event file {path} as JSON"))?;
    handle(cfg, &event)
}

fn handle(cfg: &config::Config, event: &serde_json::Value) -> Result<serde_json::Value> {
    let jobs = event::jobs_from_event(event)?;
    let mut summaries = Vec::with_capacity(jobs.len());
    for job in &jobs {
        let started = std::time::Instant::now();
        let summary = handle_job(cfg, job);
        let outcome = job_outcome(&summary);
        metrics::histogram!("abgen_lambda_job_duration_seconds", "outcome" => outcome)
            .record(started.elapsed().as_secs_f64());
        metrics::counter!("abgen_lambda_jobs_total", "outcome" => outcome).increment(1);
        summaries.push(summary?);
    }
    Ok(serde_json::json!({ "jobs": summaries }))
}

fn job_outcome(summary: &Result<serde_json::Value>) -> &'static str {
    match summary {
        Err(_) => "error",
        Ok(v) if v.get("skipped").is_some() => "skipped",
        Ok(v) if v.get("exitCode").and_then(serde_json::Value::as_i64) != Some(0) => "failed",
        Ok(_) => "converted",
    }
}

fn handle_job(cfg: &config::Config, job: &event::Job) -> Result<serde_json::Value> {
    if job.is_lods {
        eprintln!("skip: LOD job for {} (unsupported here)", job.entity_id);
        return Ok(serde_json::json!({
            "entityId": job.entity_id, "skipped": "lods-unsupported"
        }));
    }

    let content_server = job
        .content_server_url
        .as_deref()
        .unwrap_or(&cfg.default_content_server);
    if let Some(allowed) = &cfg.allowed_content_server_hosts {
        event::ensure_allowed_content_server(content_server, allowed)?;
    }
    let proxy = convert::make_proxy(cfg, content_server);

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

    let agent = catalyst::agent();
    let entity_doc = catalyst::fetch_entity(&agent, content_server, &job.entity_id)?;

    let outcome = convert::convert_entity(cfg, &proxy, &job.entity_id, content_server, &pending)?;

    metrics::counter!("abgen_lambda_texencode_cache_total", "result" => "hit")
        .increment(outcome.cache_hits);
    metrics::counter!("abgen_lambda_texencode_cache_total", "result" => "miss")
        .increment(outcome.cache_misses);
    for p in &outcome.platforms {
        metrics::counter!("abgen_lambda_bundles_total", "platform" => p.platform.clone())
            .increment(p.built.len() as u64);
    }

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

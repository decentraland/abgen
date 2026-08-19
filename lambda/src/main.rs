mod catalyst;
mod config;
mod convert;
mod event;
mod http;
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
            match run_once(&cfg, path) {
                Ok(v) => {
                    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                    if v["statusCode"].as_u64().unwrap_or(200) >= 400 {
                        std::process::exit(1);
                    }
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
                 \x20    CONTENT_SERVER_URL, OUT_ROOT, KEEP_OUTPUT,\n\
                 \x20    ABGEN_S3_ENDPOINT, ABGEN_S3_BUCKET, ABGEN_S3_REGION,\n\
                 \x20    ABGEN_S3_PATH_STYLE, ABGEN_S3_READ_ONLY (+ AWS credentials),\n\
                 \x20    ABGEN_SNS_TOPIC_ARN, ABGEN_SNS_ENDPOINT,\n\
                 \x20    ABGEN_REDIS_URL, ABGEN_REDIS_TTL_SECONDS,\n\
                 \x20    ABGEN_HTTP_SECRET (required by the Function URL POST path)"
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
    abgen::builder::require_templates().unwrap_or_else(|e| {
        eprintln!(
            "fatal: build templates unavailable ({}): {e:#}",
            abgen::builder::template_source()
        );
        std::process::exit(1);
    });
    abgen::arm_gpu_default();
    abgen::texencode_cache::enable();
    if abgen::sns::Sns::global().is_some() {
        eprintln!("init: finished-event publishing enabled (ABGEN_SNS_TOPIC_ARN)");
    }
    if abgen::rediscache::enabled() {
        eprintln!("init: redis hit-cache enabled (ABGEN_REDIS_URL)");
    }
}

fn run_once(cfg: &config::Config, path: &str) -> Result<serde_json::Value> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read event file {path}"))?;
    let event: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("parse event file {path} as JSON"))?;
    handle(cfg, &event)
}

fn handle(cfg: &config::Config, event: &serde_json::Value) -> Result<serde_json::Value> {
    match http::Request::from_event(event) {
        Some(req) => Ok(handle_http(cfg, &req)),
        None => run_jobs(cfg, event),
    }
}

/// Function URL invocations answer in-band: every outcome, including refusals
/// and handler errors, is an HTTP response rather than a Lambda error.
fn handle_http(cfg: &config::Config, req: &http::Request) -> serde_json::Value {
    let payload = match http::accept(cfg.http_secret.as_deref(), req) {
        Ok(v) => v,
        Err(response) => return response,
    };
    match run_jobs(cfg, &payload) {
        Ok(v) => http::respond(200, v),
        Err(e) => {
            eprintln!("http: handler error: {e:#}");
            http::respond(500, serde_json::json!({"error": format!("{e:#}")}))
        }
    }
}

fn run_jobs(cfg: &config::Config, event: &serde_json::Value) -> Result<serde_json::Value> {
    match event::parse_event(event)? {
        event::Event::Direct(job) => Ok(serde_json::json!({ "jobs": [handle_job(cfg, &job)?] })),
        event::Event::Sqs(records) => run_batch(records, |job| handle_job(cfg, job)),
    }
}

/// Runs an SQS batch and answers in the `ReportBatchItemFailures` format: a
/// failing record is reported by message id and redelivered on its own instead
/// of failing — and so redelivering — every record in the batch.
fn run_batch(
    records: Vec<event::Record>,
    mut run: impl FnMut(&event::Job) -> Result<serde_json::Value>,
) -> Result<serde_json::Value> {
    let mut failures = Vec::new();
    for (i, record) in records.into_iter().enumerate() {
        let label = record
            .message_id
            .clone()
            .unwrap_or_else(|| format!("record {i}"));
        match record.job.and_then(|job| run(&job)) {
            Ok(summary) => eprintln!("ok {label}: {summary}"),
            Err(err) => {
                eprintln!("failed {label}: {err:#}");
                // Without a message id the failure cannot be reported, and
                // succeeding here would delete the record: fail the invocation.
                let Some(id) = record.message_id else {
                    return Err(err.context(format!(
                        "SQS record {i} has no messageId — cannot report a partial-batch failure"
                    )));
                };
                failures.push(serde_json::json!({ "itemIdentifier": id }));
            }
        }
    }
    Ok(serde_json::json!({ "batchItemFailures": failures }))
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
    let mut already: Vec<String> = Vec::new();
    if !job.force {
        pending.retain(|platform| {
            let done = output::platform_converted(&proxy, cfg, &job.entity_id, platform);
            if done {
                eprintln!(
                    "skip: {} {platform} already converted at {}",
                    job.entity_id, cfg.version
                );
                already.push(platform.clone());
            }
            !done
        });
        if pending.is_empty() {
            // Prod publishes from every terminal branch, incl. already-converted
            // (13); this also re-notifies on redelivery after a failed publish.
            let finished: Vec<notify::Finished> = already
                .iter()
                .map(|p| notify::Finished {
                    platform: p,
                    status_code: notify::STATUS_ALREADY_CONVERTED,
                })
                .collect();
            let notified = notify::send_finished(cfg, &job.entity_id, content_server, &finished)?;
            return Ok(serde_json::json!({
                "entityId": job.entity_id, "skipped": "already-converted", "notified": notified
            }));
        }
    } else {
        // A force reconversion can downgrade a previously-ok manifest, so a
        // stale converted-ok marker must not outlive it.
        for platform in &cfg.platforms {
            if let Some(key) = output::converted_marker_key(&proxy, cfg, &job.entity_id, platform) {
                abgen::rediscache::forget(&key);
            }
        }
    }

    let agent = catalyst::agent();
    let entity_doc = catalyst::fetch_entity(&agent, content_server, &job.entity_id)?;

    let outcome = convert::convert_entity(cfg, &proxy, &job.entity_id, content_server, &pending)?;

    let published =
        output::publish(cfg, &agent, &proxy, &entity_doc, &outcome).and_then(|upload| {
            let mut finished: Vec<notify::Finished> = outcome
                .platforms
                .iter()
                .map(|p| notify::Finished {
                    platform: &p.platform,
                    status_code: p.exit_code,
                })
                .collect();
            finished.extend(already.iter().map(|p| notify::Finished {
                platform: p,
                status_code: notify::STATUS_ALREADY_CONVERTED,
            }));
            notify::send_finished(cfg, &job.entity_id, content_server, &finished)
                .map(|notified| (upload, notified))
        });
    if !cfg.keep_output {
        let _ = std::fs::remove_dir_all(&outcome.cid_dir);
    }
    let (upload, notified) = published?;

    if job.force {
        // A concurrent non-force redelivery may have re-marked from the old
        // manifest while this force job ran; drop the markers again now that
        // the (possibly downgraded) result is published. A re-mark landing
        // after this point is the residual race documented in the README.
        for platform in &cfg.platforms {
            if let Some(key) = output::converted_marker_key(&proxy, cfg, &job.entity_id, platform) {
                abgen::rediscache::forget(&key);
            }
        }
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(message_id: Option<&str>, entity_id: &str) -> event::Record {
        event::Record {
            message_id: message_id.map(str::to_string),
            job: Ok(event::Job {
                entity_id: entity_id.to_string(),
                content_server_url: None,
                is_lods: false,
                force: false,
            }),
        }
    }

    fn run(records: Vec<event::Record>, failing: &[&str]) -> Result<serde_json::Value> {
        let failing: Vec<String> = failing.iter().map(|s| s.to_string()).collect();
        run_batch(records, |job| {
            if failing.contains(&job.entity_id) {
                anyhow::bail!("boom");
            }
            Ok(json!({ "entityId": job.entity_id }))
        })
    }

    #[test]
    fn reports_no_failures_when_every_record_succeeds() {
        let batch = vec![
            record(Some("m-1"), "bafkone"),
            record(Some("m-2"), "bafktwo"),
        ];
        assert_eq!(run(batch, &[]).unwrap(), json!({"batchItemFailures": []}));
    }

    #[test]
    fn reports_only_the_failing_records() {
        let batch = vec![
            record(Some("m-1"), "bafkone"),
            record(Some("m-2"), "bafktwo"),
            record(Some("m-3"), "bafkthree"),
        ];
        assert_eq!(
            run(batch, &["bafktwo"]).unwrap(),
            json!({"batchItemFailures": [{"itemIdentifier": "m-2"}]})
        );
    }

    #[test]
    fn reports_records_that_failed_to_parse() {
        let batch = vec![
            event::Record {
                message_id: Some("m-1".into()),
                job: Err(anyhow::anyhow!("body is not JSON")),
            },
            record(Some("m-2"), "bafktwo"),
        ];
        assert_eq!(
            run(batch, &[]).unwrap(),
            json!({"batchItemFailures": [{"itemIdentifier": "m-1"}]})
        );
    }

    #[test]
    fn fails_the_invocation_when_a_failed_record_has_no_message_id() {
        let batch = vec![record(None, "bafkone")];
        assert!(run(batch, &["bafkone"]).is_err());
    }

    #[test]
    fn direct_invokes_keep_the_job_summary_shape() {
        let cfg = config::Config::from_env();
        let e = json!({"entity": {"entityId": "bafklod789"}, "lods": ["https://x/lod0.glb"]});
        assert_eq!(
            handle(&cfg, &e).unwrap(),
            json!({"jobs": [{"entityId": "bafklod789", "skipped": "lods-unsupported"}]})
        );
    }

    #[test]
    fn lod_records_are_acknowledged_not_reported_as_failures() {
        let cfg = config::Config::from_env();
        let body = json!({"entity": {"entityId": "bafklod789"}, "lods": ["https://x/lod0.glb"]});
        let e = json!({"Records": [{"messageId": "m-1", "body": body.to_string()}]});
        assert_eq!(handle(&cfg, &e).unwrap(), json!({"batchItemFailures": []}));
    }
}

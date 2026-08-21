mod catalyst;
mod config;
mod convert;
mod emf;
mod event;
mod http;
mod lod;
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
                 \x20    ABGEN_HTTP_SECRET (required by the Function URL POST path),\n\
                 \x20    ABGEN_EMF_NAMESPACE (CloudWatch EMF metrics on stdout),\n\
                 \x20    ABGEN_LOG_FORMAT (json for JSON logs), RUST_LOG (filter,\n\
                 \x20    default abgen=info,abgen_lambda=info),\n\
                 \x20    ENABLE_LODS (off: LOD jobs are acked and skipped; on: levels 0+1\n\
                 \x20    are regenerated from the scene and written to LOD/<level>/ and\n\
                 \x20    lods-unity/manifests/ — the deployment's FBX sources are unused)"
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
    init_tracing();
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
    abgen::decode_cache::enable();
    if abgen::sns::Sns::global().is_some() {
        eprintln!("init: finished-event publishing enabled (ABGEN_SNS_TOPIC_ARN)");
    }
    if abgen::rediscache::enabled() {
        eprintln!("init: redis hit-cache enabled (ABGEN_REDIS_URL)");
    }
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "abgen=info,abgen_lambda=info".into());
    let json_logs = std::env::var("ABGEN_LOG_FORMAT")
        .map(|v| v.trim().eq_ignore_ascii_case("json"))
        .unwrap_or(false);
    if json_logs {
        tracing_subscriber::fmt().json().with_env_filter(filter).init();
    } else {
        // CloudWatch renders ANSI escapes literally.
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_ansi(false)
            .init();
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
        None => run_jobs(cfg, event::parse_event(event)?),
    }
}

/// Function URL invocations answer in-band: every outcome, including refusals
/// and handler errors, is an HTTP response rather than a Lambda error.
/// Client mistakes (unrecognized event shape) are `400`; handler errors are a
/// `500` with a generic body — the error chain names internal paths, upstream
/// URLs and ARNs, so it goes to the log only, never over the wire.
fn handle_http(cfg: &config::Config, req: &http::Request) -> serde_json::Value {
    let payload = match http::accept(cfg.http_secret.as_deref(), req) {
        Ok(v) => v,
        Err(response) => return response,
    };
    let parsed = match event::parse_event(&payload) {
        Ok(parsed) => parsed,
        Err(e) => return http::respond(400, serde_json::json!({"error": format!("{e:#}")})),
    };
    match run_jobs(cfg, parsed) {
        Ok(v) => {
            // A Records-shaped body reports per-record failures instead of
            // erroring, and no queue exists on this path to redeliver them:
            // surface any failure as a 500 so callers keying on the status
            // code cannot mistake a lost job for success.
            let failed = v
                .get("batchItemFailures")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|a| !a.is_empty());
            http::respond(if failed { 500 } else { 200 }, v)
        }
        Err(e) => {
            eprintln!("http: handler error: {e:#}");
            http::respond(
                500,
                serde_json::json!({"error": "conversion failed (see function logs)"}),
            )
        }
    }
}

fn run_jobs(cfg: &config::Config, event: event::Event) -> Result<serde_json::Value> {
    match event {
        event::Event::Direct(job) => Ok(serde_json::json!({
            "jobs": [instrumented_job(cfg, Ok(job))?]
        })),
        event::Event::Sqs(records) => run_batch(records, |job| instrumented_job(cfg, job)),
    }
}

/// Every job — direct, SQS record, or HTTP-submitted — goes through here so
/// the per-job outcome counters and duration histogram see all of them,
/// including records that failed to parse (a poison-pill message redelivering
/// until the DLQ must show up as `outcome="error"`, not only in stderr).
fn instrumented_job(cfg: &config::Config, job: Result<event::Job>) -> Result<serde_json::Value> {
    let started = std::time::Instant::now();
    let summary = job.and_then(|job| catch_job_panic(|| handle_job(cfg, &job)));
    let outcome = job_outcome(&summary);
    metrics::histogram!("abgen_lambda_job_duration_seconds", "outcome" => outcome)
        .record(started.elapsed().as_secs_f64());
    metrics::counter!("abgen_lambda_jobs_total", "outcome" => outcome).increment(1);
    summary
}

/// A panic anywhere in a conversion must not unwind past the runtime loop: the
/// process would abort before `emf::flush`, no response would be posted, and
/// the whole SQS batch would redeliver (re-running and re-notifying records
/// that already succeeded). Converted to an `Err`, it becomes a plain
/// batchItemFailure / handler error instead.
fn catch_job_panic(f: impl FnOnce() -> Result<serde_json::Value>) -> Result<serde_json::Value> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(summary) => summary,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .copied()
                .map(str::to_string)
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "non-string panic payload".to_string());
            Err(anyhow::anyhow!("job panicked: {msg}"))
        }
    }
}

/// Runs an SQS batch and answers in the `ReportBatchItemFailures` format: a
/// failing record is reported by message id and redelivered on its own instead
/// of failing — and so redelivering — every record in the batch.
fn run_batch(
    records: Vec<event::Record>,
    mut run: impl FnMut(Result<event::Job>) -> Result<serde_json::Value>,
) -> Result<serde_json::Value> {
    let mut failures = Vec::new();
    for (i, record) in records.into_iter().enumerate() {
        let label = record
            .message_id
            .clone()
            .unwrap_or_else(|| format!("record {i}"));
        match run(record.job) {
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

pub(crate) fn job_outcome(summary: &Result<serde_json::Value>) -> &'static str {
    match summary {
        Err(_) => "error",
        Ok(v) if v.get("skipped").is_some() => "skipped",
        Ok(v) if v.get("exitCode").and_then(serde_json::Value::as_i64) != Some(0) => "failed",
        Ok(_) => "converted",
    }
}

fn handle_job(cfg: &config::Config, job: &event::Job) -> Result<serde_json::Value> {
    if job.is_lods && !cfg.lods_enabled {
        eprintln!(
            "skip: LOD job for {} (LOD generation is off; set ENABLE_LODS=1)",
            job.entity_id
        );
        return Ok(serde_json::json!({
            "entityId": job.entity_id, "skipped": "lods-disabled"
        }));
    }

    let content_server = job
        .content_server_url
        .as_deref()
        .unwrap_or(&cfg.default_content_server);
    // Scheme/shape validation is unconditional — an event-supplied URL must
    // never make the handler fetch plaintext/internal targets even on a
    // deployment that (fail-open) never set ALLOWED_CONTENT_SERVER_HOSTS.
    event::validate_content_server(content_server, cfg.allowed_content_server_hosts.as_deref())?;
    let proxy = convert::make_proxy(cfg, content_server);

    if job.is_lods {
        return lod::convert(cfg, &proxy, &job.entity_id, content_server);
    }

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
            let finished: Vec<notify::Finished> = already
                .iter()
                .map(|p| notify::Finished {
                    platform: p,
                    status_code: notify::STATUS_ALREADY_CONVERTED,
                })
                .collect();
            let notified =
                notify::send_finished(cfg, &job.entity_id, content_server, false, &finished)?;
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

    metrics::counter!("abgen_lambda_texencode_cache_total", "outcome" => "hit")
        .increment(outcome.cache_hits);
    metrics::counter!("abgen_lambda_texencode_cache_total", "outcome" => "miss")
        .increment(outcome.cache_misses);
    for p in &outcome.platforms {
        metrics::counter!("abgen_lambda_bundles_total", "platform" => p.platform.clone())
            .increment(p.built.len() as u64);
    }

    let published = publish_forget_notify(
        || output::publish(cfg, &agent, &proxy, &entity_doc, &outcome),
        || {
            if job.force {
                // A concurrent non-force redelivery may have re-marked from
                // the old manifest while this force job ran; drop the markers
                // again the moment the (possibly downgraded) result is
                // published — before notify, whose SNS retries would widen the
                // stale-marker window and whose failure must not skip the
                // forget. A re-mark landing after this point is the residual
                // race documented in the README.
                for platform in &cfg.platforms {
                    if let Some(key) =
                        output::converted_marker_key(&proxy, cfg, &job.entity_id, platform)
                    {
                        abgen::rediscache::forget(&key);
                    }
                }
            }
        },
        || {
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
            notify::send_finished(cfg, &job.entity_id, content_server, false, &finished)
        },
    );
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

/// Sequences the terminal steps of a conversion: `forget` runs immediately
/// after a successful publish and before `notify`, and runs even when notify
/// then fails — a stale converted-ok marker must never outlive a publish
/// because SNS was slow or down.
fn publish_forget_notify<U>(
    publish: impl FnOnce() -> Result<U>,
    forget: impl FnOnce(),
    notify: impl FnOnce() -> Result<bool>,
) -> Result<(U, bool)> {
    let upload = publish()?;
    forget();
    notify().map(|notified| (upload, notified))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Explicit config so no test depends on ambient env vars (`ENABLE_LODS=1`
    /// once sent the lod tests below through the real network-touching lane).
    fn test_cfg() -> config::Config {
        config::Config {
            platforms: vec!["windows".to_string(), "mac".to_string()],
            version: "v49".to_string(),
            cache_dir: "/tmp/cache".to_string(),
            default_content_server: "https://peer.decentraland.org/content".to_string(),
            out_root: std::path::PathBuf::from("/tmp/out"),
            keep_output: false,
            allowed_content_server_hosts: None,
            http_secret: None,
            lods_enabled: false,
        }
    }

    fn http_post(body: &serde_json::Value, secret: &str) -> serde_json::Value {
        json!({
            "version": "2.0",
            "requestContext": {"http": {"method": "POST", "path": "/"}},
            "headers": {"x-abgen-secret": secret},
            "body": body.to_string(),
            "isBase64Encoded": false,
        })
    }

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
            let job = job?;
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
        let cfg = test_cfg();
        let e = json!({"entity": {"entityId": "bafklod789"}, "lods": ["https://x/lod0.glb"]});
        assert_eq!(
            handle(&cfg, &e).unwrap(),
            json!({"jobs": [{"entityId": "bafklod789", "skipped": "lods-disabled"}]})
        );
    }

    #[test]
    fn lod_records_are_acknowledged_not_reported_as_failures() {
        let cfg = test_cfg();
        let body = json!({"entity": {"entityId": "bafklod789"}, "lods": ["https://x/lod0.glb"]});
        let e = json!({"Records": [{"messageId": "m-1", "body": body.to_string()}]});
        assert_eq!(handle(&cfg, &e).unwrap(), json!({"batchItemFailures": []}));
    }

    #[test]
    fn forget_runs_after_publish_and_before_notify_even_when_notify_fails() {
        use std::cell::RefCell;
        let log: RefCell<Vec<&str>> = RefCell::new(Vec::new());
        let result = publish_forget_notify(
            || {
                log.borrow_mut().push("publish");
                Ok(1)
            },
            || log.borrow_mut().push("forget"),
            || {
                log.borrow_mut().push("notify");
                anyhow::bail!("sns down")
            },
        );
        assert!(result.is_err());
        assert_eq!(*log.borrow(), vec!["publish", "forget", "notify"]);

        let log: RefCell<Vec<&str>> = RefCell::new(Vec::new());
        let (upload, notified) = publish_forget_notify(
            || {
                log.borrow_mut().push("publish");
                Ok(2)
            },
            || log.borrow_mut().push("forget"),
            || {
                log.borrow_mut().push("notify");
                Ok(true)
            },
        )
        .unwrap();
        assert_eq!((upload, notified), (2, true));
        assert_eq!(*log.borrow(), vec!["publish", "forget", "notify"]);
    }

    #[test]
    fn failed_publish_skips_forget_and_notify() {
        let log: std::cell::RefCell<Vec<&str>> = std::cell::RefCell::new(Vec::new());
        let r: Result<(i32, bool)> = publish_forget_notify(
            || {
                log.borrow_mut().push("publish");
                anyhow::bail!("s3 down")
            },
            || log.borrow_mut().push("forget"),
            || {
                log.borrow_mut().push("notify");
                Ok(true)
            },
        );
        assert!(r.is_err());
        assert_eq!(*log.borrow(), vec!["publish"]);
    }

    #[test]
    fn http_unrecognized_shape_is_a_400() {
        let mut cfg = test_cfg();
        cfg.http_secret = Some("s3cret".to_string());
        let resp = handle(&cfg, &http_post(&json!({"foo": 1}), "s3cret")).unwrap();
        assert_eq!(resp["statusCode"], 400);
        assert!(
            resp["body"]
                .as_str()
                .unwrap()
                .contains("unrecognized event shape"),
            "{resp}"
        );
    }

    #[test]
    fn http_records_body_with_a_failing_record_is_a_500() {
        let mut cfg = test_cfg();
        cfg.http_secret = Some("s3cret".to_string());
        let body = json!({"Records": [{"messageId": "m-1", "body": "not json"}]});
        let resp = handle(&cfg, &http_post(&body, "s3cret")).unwrap();
        assert_eq!(resp["statusCode"], 500);
        let inner: serde_json::Value =
            serde_json::from_str(resp["body"].as_str().unwrap()).unwrap();
        assert_eq!(
            inner["batchItemFailures"],
            json!([{"itemIdentifier": "m-1"}])
        );
    }

    #[test]
    fn http_records_body_with_only_skips_is_a_200() {
        let mut cfg = test_cfg();
        cfg.http_secret = Some("s3cret".to_string());
        let record = json!({"entity": {"entityId": "bafklod789"}, "lods": ["https://x/lod0.glb"]});
        let body = json!({"Records": [{"messageId": "m-1", "body": record.to_string()}]});
        let resp = handle(&cfg, &http_post(&body, "s3cret")).unwrap();
        assert_eq!(resp["statusCode"], 200);
    }

    #[test]
    fn http_500_body_is_generic_not_the_error_chain() {
        let mut cfg = test_cfg();
        cfg.http_secret = Some("s3cret".to_string());
        cfg.allowed_content_server_hosts = Some(vec!["peer.decentraland.org".to_string()]);
        let body = json!({
            "entityId": "bafkabc123",
            "contentServerUrl": "https://evil.example.com/content"
        });
        let resp = handle(&cfg, &http_post(&body, "s3cret")).unwrap();
        assert_eq!(resp["statusCode"], 500);
        let text = resp["body"].as_str().unwrap();
        assert!(!text.contains("evil.example.com"), "{text}");
        assert!(text.contains("see function logs"), "{text}");
    }

    #[test]
    fn content_server_scheme_validation_applies_without_an_allowlist() {
        let cfg = test_cfg();
        assert!(cfg.allowed_content_server_hosts.is_none());
        let e = json!({"entityId": "bafkabc123", "contentServerUrl": "http://10.0.3.7:8500"});
        let err = handle(&cfg, &e).unwrap_err();
        assert!(format!("{err:#}").contains("https required"), "{err:#}");
    }

    #[test]
    fn catch_job_panic_converts_panics_to_errors() {
        assert_eq!(catch_job_panic(|| Ok(json!(1))).unwrap(), json!(1));
        let err = catch_job_panic(|| panic!("boom {}", 1)).unwrap_err();
        assert!(format!("{err}").contains("job panicked: boom 1"), "{err}");
        let err = catch_job_panic(|| anyhow::bail!("plain error")).unwrap_err();
        assert_eq!(format!("{err}"), "plain error");
    }

    #[test]
    fn a_panicking_record_becomes_a_batch_item_failure() {
        let batch = vec![
            record(Some("m-1"), "bafkboom"),
            record(Some("m-2"), "bafkok"),
        ];
        let out = run_batch(batch, |job| {
            catch_job_panic(|| {
                let job = job?;
                if job.entity_id == "bafkboom" {
                    panic!("unwrap oops");
                }
                Ok(json!({"entityId": job.entity_id, "exitCode": 0}))
            })
        })
        .unwrap();
        assert_eq!(
            out,
            json!({"batchItemFailures": [{"itemIdentifier": "m-1"}]})
        );
    }

    #[test]
    fn job_outcome_classification() {
        assert_eq!(job_outcome(&Err(anyhow::anyhow!("x"))), "error");
        assert_eq!(
            job_outcome(&Ok(json!({"skipped": "lods-disabled"}))),
            "skipped"
        );
        assert_eq!(job_outcome(&Ok(json!({"exitCode": 1}))), "failed");
        assert_eq!(job_outcome(&Ok(json!({"exitCode": 0}))), "converted");
        // Success summaries must carry a top-level exitCode — without one the
        // job counts as failed (this miscounted every successful LOD job once).
        assert_eq!(job_outcome(&Ok(json!({"entityId": "e"}))), "failed");
    }

    /// A record whose body fails to parse must still hit the per-job metrics
    /// (`abgen_lambda_jobs_total{outcome="error"}`) — a poison-pill message
    /// redelivering to the DLQ was previously invisible in EMF.
    #[test]
    fn parse_failures_are_counted_in_job_metrics() {
        use metrics::{
            Counter, CounterFn, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString,
            Unit,
        };
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::{Arc, Mutex};

        type Registered = (String, Vec<(String, String)>, Arc<AtomicU64>);
        #[derive(Default)]
        struct Capture {
            counters: Mutex<Vec<Registered>>,
        }
        struct Count(Arc<AtomicU64>);
        impl CounterFn for Count {
            fn increment(&self, v: u64) {
                self.0.fetch_add(v, Ordering::Relaxed);
            }
            fn absolute(&self, v: u64) {
                self.0.store(v, Ordering::Relaxed);
            }
        }
        impl Recorder for Capture {
            fn describe_counter(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
            fn describe_gauge(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
            fn describe_histogram(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
            fn register_counter(&self, key: &Key, _: &Metadata<'_>) -> Counter {
                let cell = Arc::new(AtomicU64::new(0));
                self.counters.lock().unwrap().push((
                    key.name().to_string(),
                    key.labels()
                        .map(|l| (l.key().to_string(), l.value().to_string()))
                        .collect(),
                    cell.clone(),
                ));
                Counter::from_arc(Arc::new(Count(cell)))
            }
            fn register_gauge(&self, _: &Key, _: &Metadata<'_>) -> Gauge {
                Gauge::noop()
            }
            fn register_histogram(&self, _: &Key, _: &Metadata<'_>) -> Histogram {
                Histogram::noop()
            }
        }

        let capture = Capture::default();
        let cfg = test_cfg();
        let summary = metrics::with_local_recorder(&capture, || {
            instrumented_job(&cfg, Err(anyhow::anyhow!("body is not JSON")))
        });
        assert!(summary.is_err());
        let counters = capture.counters.lock().unwrap();
        let (_, _, count) = counters
            .iter()
            .find(|(name, labels, _)| {
                name == "abgen_lambda_jobs_total"
                    && labels.iter().any(|(k, v)| k == "outcome" && v == "error")
            })
            .expect("jobs_total{outcome=error} must be registered");
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }
}

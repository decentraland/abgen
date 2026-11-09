use crate::config::Config;
use anyhow::Result;

const API_VERSION: &str = "2018-06-01";

/// Serves invocations one after another in this process. A runtime that exits after
/// answering is reported by Lambda as `Runtime.ExitError` on every invocation, so the
/// process stays up; isolation between scenes comes from the sandbox child and, for the
/// scene runner, from one Lambda tenant per entity.
pub fn serve(
    cfg: &Config,
    handler: impl Fn(&Config, &serde_json::Value) -> Result<serde_json::Value>,
) -> ! {
    let api = std::env::var("AWS_LAMBDA_RUNTIME_API").unwrap_or_else(|_| {
        eprintln!(
            "fatal: AWS_LAMBDA_RUNTIME_API is not set — run under AWS Lambda, \
             or use --once EVENT.json for a local run"
        );
        std::process::exit(1);
    });
    let agent: ureq::Agent = ureq::Agent::config_builder().build().into();
    let next_url = format!("http://{api}/{API_VERSION}/runtime/invocation/next");

    loop {
        let (request_id, deadline, event) = match next_invocation(&agent, &next_url) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("runtime: failed to fetch next invocation: {e:#}");
                std::thread::sleep(std::time::Duration::from_millis(200));
                continue;
            }
        };

        abgen::lambda_invoke::set_invocation_deadline(deadline);
        let started = std::time::Instant::now();
        let outcome = handler(cfg, &event);
        abgen::lambda_invoke::set_invocation_deadline(None);
        let result = if outcome.is_ok() { "ok" } else { "error" };
        metrics::counter!("abgen_lambda_invocations_total", "result" => result).increment(1);
        metrics::histogram!("abgen_lambda_invocation_duration_seconds", "result" => result)
            .record(started.elapsed().as_secs_f64());
        crate::emf::flush();

        match outcome {
            Ok(response) => {
                let url =
                    format!("http://{api}/{API_VERSION}/runtime/invocation/{request_id}/response");
                if let Err(e) = post_json(&agent, &url, &response) {
                    eprintln!("runtime: failed to post response for {request_id}: {e:#}");
                }
            }
            Err(err) => {
                eprintln!("handler error for {request_id}: {err:#}");
                let url =
                    format!("http://{api}/{API_VERSION}/runtime/invocation/{request_id}/error");
                let body = serde_json::json!({
                    "errorMessage": format!("{err:#}"),
                    "errorType": "HandlerError",
                });
                if let Err(e) = post_json(&agent, &url, &body) {
                    eprintln!("runtime: failed to post error for {request_id}: {e:#}");
                }
            }
        }
    }
}

type Invocation = (String, Option<std::time::SystemTime>, serde_json::Value);

fn next_invocation(agent: &ureq::Agent, url: &str) -> Result<Invocation> {
    let resp = agent
        .get(url)
        .call()
        .map_err(|e| anyhow::anyhow!("GET invocation/next: {e}"))?;
    let request_id = resp
        .headers()
        .get("lambda-runtime-aws-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("invocation/next without Lambda-Runtime-Aws-Request-Id"))?;
    let deadline = resp
        .headers()
        .get("lambda-runtime-deadline-ms")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_deadline_ms);
    let mut buf = Vec::new();
    use std::io::Read;
    resp.into_body().into_reader().read_to_end(&mut buf)?;
    let event: serde_json::Value = serde_json::from_slice(&buf)
        .map_err(|e| anyhow::anyhow!("invocation payload is not JSON: {e}"))?;
    Ok((request_id, deadline, event))
}

/// `Lambda-Runtime-Deadline-Ms`: the invocation's deadline in Unix epoch milliseconds.
fn parse_deadline_ms(value: &str) -> Option<std::time::SystemTime> {
    let ms = value.trim().parse::<u64>().ok()?;
    std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_millis(ms))
}

fn post_json(agent: &ureq::Agent, url: &str, body: &serde_json::Value) -> Result<()> {
    agent
        .post(url)
        .header("Content-Type", "application/json")
        .send(body.to_string())
        .map_err(|e| anyhow::anyhow!("POST {url}: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_header_is_epoch_millis() {
        let got = parse_deadline_ms(" 1700000000123 ").unwrap();
        assert_eq!(
            got.duration_since(std::time::UNIX_EPOCH).unwrap(),
            std::time::Duration::from_millis(1_700_000_000_123)
        );
        assert!(parse_deadline_ms("soon").is_none());
        assert!(parse_deadline_ms("").is_none());
    }
}

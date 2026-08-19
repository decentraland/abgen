//! Hand-rolled AWS Lambda custom-runtime loop.
//!
//! The runtime API is a local HTTP long-poll — `GET …/invocation/next` blocks
//! until an event arrives, the handler runs, and the result is POSTed back.
//! That is the whole contract, so rather than pulling in an async stack
//! (lambda_runtime + tokio + hyper) this speaks it directly with the same
//! blocking ureq abgen already uses. See
//! https://docs.aws.amazon.com/lambda/latest/dg/runtimes-api.html

use crate::config::Config;
use anyhow::Result;

const API_VERSION: &str = "2018-06-01";

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
    // No timeouts: /invocation/next intentionally blocks until an event
    // arrives (Lambda freezes the process in between).
    let agent: ureq::Agent = ureq::Agent::config_builder().build().into();
    let next_url = format!("http://{api}/{API_VERSION}/runtime/invocation/next");

    loop {
        let (request_id, event) = match next_invocation(&agent, &next_url) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("runtime: failed to fetch next invocation: {e:#}");
                std::thread::sleep(std::time::Duration::from_millis(200));
                continue;
            }
        };

        match handler(cfg, &event) {
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

fn next_invocation(agent: &ureq::Agent, url: &str) -> Result<(String, serde_json::Value)> {
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
    let mut buf = Vec::new();
    use std::io::Read;
    resp.into_body().into_reader().read_to_end(&mut buf)?;
    let event: serde_json::Value = serde_json::from_slice(&buf)
        .map_err(|e| anyhow::anyhow!("invocation payload is not JSON: {e}"))?;
    Ok((request_id, event))
}

fn post_json(agent: &ureq::Agent, url: &str, body: &serde_json::Value) -> Result<()> {
    agent
        .post(url)
        .header("Content-Type", "application/json")
        .send(body.to_string())
        .map_err(|e| anyhow::anyhow!("POST {url}: {e}"))?;
    Ok(())
}

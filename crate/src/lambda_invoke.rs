//! Synchronous AWS Lambda `Invoke` client, SigV4 sibling of [`crate::sns`] (shared signer,
//! creds and hash primitives from [`crate::space`]).
//!
//! Only what the LOD lane needs: a `RequestResponse` invoke of a function named by its
//! plain name in the caller's region, a JSON payload in and the function's JSON result
//! out. A function error (`X-Amz-Function-Error`) is returned as an error carrying the
//! function's `errorMessage`. Throttling (429), 5xx and failures to connect are retried
//! with jittered backoff inside the time the caller allows. A 5xx may come after the
//! function already started, so a retry can overlap a run still going: callers must only
//! invoke functions without side effects (a scene run has none; the overlap costs
//! concurrency). Errors after the request may have been sent (a dropped connection, an
//! unreadable body, a timeout) are not retried.
//!
//! Also holds the deadline of the Lambda invocation this process is serving
//! ([`set_invocation_deadline`]), so work started on its behalf can stop in time.

use crate::space::{sha256_hex, sigv4_authorization, timestamps, CredsSource};
use crate::Result;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

static INVOCATION_DEADLINE: Mutex<Option<SystemTime>> = Mutex::new(None);

/// Records when the invocation being served must have answered (Lambda's
/// `Lambda-Runtime-Deadline-Ms`), or clears it.
pub fn set_invocation_deadline(deadline: Option<SystemTime>) {
    *INVOCATION_DEADLINE
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = deadline;
}

/// Time left in the invocation being served: `None` outside one (local runs), zero once
/// it has passed.
pub fn invocation_remaining() -> Option<Duration> {
    let deadline = (*INVOCATION_DEADLINE
        .lock()
        .unwrap_or_else(|e| e.into_inner()))?;
    Some(
        deadline
            .duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO),
    )
}

pub struct LambdaInvoker {
    function: String,
    scheme: String,
    host: String,
    region: String,
    creds: CredsSource,
    agent: ureq::Agent,
}

/// Lambda function names: letters, digits, `-` and `_`, at most 64 characters. ARNs and
/// qualifiers are not accepted, which keeps the request path free of characters that
/// SigV4 would need to encode.
pub fn valid_function_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `scheme://host[:port]` of an endpoint override. Plain `http` is accepted only for a
/// loopback host: the request carries a session token.
fn split_endpoint(endpoint: &str) -> Result<(String, String)> {
    let (scheme, host) = match endpoint.split_once("://") {
        Some((s, h)) => (s.to_ascii_lowercase(), h.trim_end_matches('/').to_string()),
        None => (
            "https".to_string(),
            endpoint.trim_end_matches('/').to_string(),
        ),
    };
    if host.is_empty() || host.contains(['/', '@', '?', '#']) {
        crate::bail!("lambda endpoint {endpoint:?} is not scheme://host[:port]");
    }
    match scheme.as_str() {
        "https" => {}
        "http" if is_loopback(&host) => {}
        _ => crate::bail!("lambda endpoint {endpoint:?}: https required (http only for loopback)"),
    }
    Ok((scheme, host))
}

fn is_loopback(authority: &str) -> bool {
    let host = match authority.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => authority.rsplit_once(':').map_or(authority, |(h, _)| h),
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// How one attempt ended, for the retry decision.
enum Attempt {
    Done(Result<serde_json::Value>),
    Retry(anyhow::Error),
}

const MAX_ATTEMPTS: u32 = 4;
const BACKOFF_BASE: Duration = Duration::from_millis(250);
/// A retry is not started with less than this left: about a cold start plus a short run.
const MIN_ATTEMPT: Duration = Duration::from_secs(10);

impl LambdaInvoker {
    /// `endpoint` overrides `https://lambda.{region}.amazonaws.com` (tests, local stacks).
    /// Credentials come from the AWS variables or the container endpoint only, never the
    /// `ABGEN_S3_*` overrides.
    pub fn new(function: &str, region: &str, endpoint: Option<&str>) -> Result<LambdaInvoker> {
        if !valid_function_name(function) {
            crate::bail!("lambda function name {function:?} is not a plain function name");
        }
        let creds = CredsSource::from_aws_env()
            .ok_or_else(|| crate::anyhow!("no AWS credentials to invoke lambda {function}"))?;
        Self::with_creds(function, region, endpoint, creds)
    }

    fn with_creds(
        function: &str,
        region: &str,
        endpoint: Option<&str>,
        creds: CredsSource,
    ) -> Result<LambdaInvoker> {
        let (scheme, host) = match endpoint.map(str::trim).filter(|e| !e.is_empty()) {
            Some(e) => split_endpoint(e)?,
            None => (
                "https".to_string(),
                format!("lambda.{region}.amazonaws.com"),
            ),
        };
        let agent = ureq::Agent::config_builder()
            .max_redirects(0)
            .http_status_as_error(false)
            .build()
            .into();
        Ok(LambdaInvoker {
            function: function.to_string(),
            scheme,
            host,
            region: region.to_string(),
            creds,
            agent,
        })
    }

    pub fn function(&self) -> &str {
        &self.function
    }

    fn path(&self) -> String {
        format!("/2015-03-31/functions/{}/invocations", self.function)
    }

    /// One `RequestResponse` invoke that must finish within `budget`, retries included.
    /// Only throttling (429), 5xx and connection failures are retried: a scene run has no
    /// side effects, but a function error or a timeout would only repeat.
    pub fn invoke(
        &self,
        payload: &serde_json::Value,
        budget: Duration,
    ) -> Result<serde_json::Value> {
        let body = serde_json::to_vec(payload)?;
        let deadline = Instant::now() + budget;
        let mut attempt = 1;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let err = match self.invoke_once(&body, left) {
                Attempt::Done(result) => return result,
                Attempt::Retry(e) => e,
            };
            let backoff = backoff(attempt);
            let left = deadline.saturating_duration_since(Instant::now());
            if attempt >= MAX_ATTEMPTS || left < backoff + MIN_ATTEMPT {
                return Err(err.context(format!(
                    "invoke lambda {} (gave up after {attempt} attempt(s))",
                    self.function
                )));
            }
            tracing::warn!(
                function = %self.function,
                attempt,
                error = %err,
                "lambda invoke attempt failed, retrying"
            );
            std::thread::sleep(backoff);
            attempt += 1;
        }
    }

    fn invoke_once(&self, body: &[u8], timeout: Duration) -> Attempt {
        let c = match self.creds.resolve() {
            Ok(c) => c,
            Err(e) => return Attempt::Done(Err(e)),
        };
        let payload_hash = sha256_hex(body);
        let (date, amz) = timestamps();
        let path = self.path();
        let auth = sigv4_authorization(
            &c,
            "lambda",
            &self.region,
            "POST",
            &self.host,
            &path,
            "",
            &payload_hash,
            &amz,
            &date,
        );
        let url = format!("{}://{}{}", self.scheme, self.host, path);
        let mut req = self
            .agent
            .post(&url)
            .config()
            .timeout_global(Some(timeout))
            .build()
            .header("Content-Type", "application/json")
            .header("x-amz-date", &amz)
            .header("Authorization", &auth);
        if let Some(token) = &c.session_token {
            req = req.header("x-amz-security-token", token);
        }
        let resp = match req.send(body) {
            Ok(resp) => resp,
            Err(ureq::Error::Timeout(t)) => {
                return Attempt::Done(Err(crate::anyhow!(
                    "invoke lambda {}: no answer within {}s ({t})",
                    self.function,
                    timeout.as_secs()
                )))
            }
            Err(e) if never_sent(&e) => {
                return Attempt::Retry(crate::anyhow!("invoke lambda {}: {e}", self.function))
            }
            Err(e) => {
                return Attempt::Done(Err(crate::anyhow!("invoke lambda {}: {e}", self.function)))
            }
        };
        let status = resp.status().as_u16();
        let function_error = resp
            .headers()
            .get("x-amz-function-error")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let mut out = Vec::new();
        use std::io::Read;
        if let Err(e) = resp.into_body().into_reader().read_to_end(&mut out) {
            return Attempt::Done(Err(crate::anyhow!(
                "read lambda {} response: {e}",
                self.function
            )));
        }
        if status != 200 {
            let err = crate::anyhow!(
                "invoke lambda {}: HTTP {status}: {}",
                self.function,
                String::from_utf8_lossy(&out[..out.len().min(512)])
            );
            return if status == 429 || status >= 500 {
                Attempt::Retry(err)
            } else {
                Attempt::Done(Err(err))
            };
        }
        let value: serde_json::Value = match serde_json::from_slice(&out) {
            Ok(v) => v,
            Err(e) => {
                return Attempt::Done(Err(crate::anyhow!(
                    "lambda {} returned non-JSON: {e}",
                    self.function
                )))
            }
        };
        if let Some(kind) = function_error {
            let message = value
                .get("errorMessage")
                .and_then(|m| m.as_str())
                .unwrap_or("no errorMessage");
            return Attempt::Done(Err(crate::anyhow!(
                "lambda {} failed ({kind}): {message}",
                self.function
            )));
        }
        Attempt::Done(Ok(value))
    }
}

/// Errors that mean the request never reached Lambda: safe to retry.
fn never_sent(e: &ureq::Error) -> bool {
    use std::io::ErrorKind;
    match e {
        ureq::Error::HostNotFound | ureq::Error::ConnectionFailed => true,
        ureq::Error::Io(io) => matches!(
            io.kind(),
            ErrorKind::ConnectionRefused | ErrorKind::AddrNotAvailable | ErrorKind::NotConnected
        ),
        _ => false,
    }
}

/// `BACKOFF_BASE * 2^(attempt-1)`, plus up to the same again of jitter so that throttled
/// callers do not retry in step. The jitter comes from a freshly keyed std hasher, which
/// differs per call and per process.
fn backoff(attempt: u32) -> Duration {
    use std::hash::BuildHasher;
    let base = BACKOFF_BASE * 2u32.saturating_pow(attempt.saturating_sub(1));
    let random = std::collections::hash_map::RandomState::new().hash_one(attempt);
    base + base.mul_f64((random % 1024) as f64 / 1024.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::space::ResolvedCreds;
    use std::io::{Read, Write};

    fn creds(token: Option<&str>) -> CredsSource {
        CredsSource::Static(ResolvedCreds {
            access_key: "AKIDEXAMPLE".into(),
            secret_key: "example-secret".into(),
            session_token: token.map(str::to_string),
        })
    }

    fn invoker(endpoint: &str, token: Option<&str>) -> LambdaInvoker {
        LambdaInvoker::with_creds("abgen-scenerun", "us-east-1", Some(endpoint), creds(token))
            .unwrap()
    }

    #[test]
    fn function_names_are_plain() {
        assert!(valid_function_name("abgen-scenerun_1"));
        assert!(!valid_function_name(""));
        assert!(!valid_function_name(
            "arn:aws:lambda:us-east-1:123456789012:function:x"
        ));
        assert!(!valid_function_name("x:prod"));
        assert!(!valid_function_name("a/b"));
        assert!(!valid_function_name(&"a".repeat(65)));
    }

    #[test]
    fn plain_http_endpoints_must_be_loopback() {
        for ok in [
            "http://127.0.0.1:9001",
            "http://localhost:4566/",
            "http://[::1]:4566",
            "https://lambda.example.org",
            "lambda.example.org",
        ] {
            assert!(split_endpoint(ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://lambda.example.org",
            "http://10.0.0.5:4566",
            "ftp://127.0.0.1",
            "https://user@lambda.example.org",
            "https://lambda.example.org/path",
            "https://",
        ] {
            assert!(split_endpoint(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn invocation_deadline_counts_down() {
        set_invocation_deadline(Some(SystemTime::now() + Duration::from_secs(60)));
        let left = invocation_remaining().unwrap();
        assert!(left > Duration::from_secs(55) && left <= Duration::from_secs(60));
        set_invocation_deadline(Some(SystemTime::now() - Duration::from_secs(1)));
        assert_eq!(invocation_remaining(), Some(Duration::ZERO));
        set_invocation_deadline(None);
        assert_eq!(invocation_remaining(), None);
    }

    /// Serves one canned HTTP response per connection, in order, and hands back the
    /// requests it read.
    fn server(
        responses: Vec<&'static str>,
    ) -> (String, std::thread::JoinHandle<Vec<(String, Vec<u8>)>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for response in responses {
                let (mut sock, _) = listener.accept().unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head, body_start, len) = loop {
                    let n = sock.read(&mut chunk).unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..i]).to_string();
                        let len = head
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        break (head, i + 4, len);
                    }
                };
                while buf.len() < body_start + len {
                    let n = sock.read(&mut chunk).unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                }
                if response.is_empty() {
                    // Hold the connection open, unanswered, past the client's timeout.
                    std::thread::sleep(Duration::from_secs(2));
                } else {
                    sock.write_all(response.as_bytes()).unwrap();
                }
                seen.push((head, buf[body_start..body_start + len].to_vec()));
            }
            seen
        });
        (format!("http://{addr}"), handle)
    }

    const OK: &str = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}";
    const BUDGET: Duration = Duration::from_secs(20);

    #[test]
    fn invoke_signs_and_posts_the_payload() {
        let (endpoint, server) = server(vec![OK]);
        let got = invoker(&endpoint, Some("example-token"))
            .invoke(&serde_json::json!({"a": 1}), BUDGET)
            .unwrap();
        assert_eq!(got, serde_json::json!({"ok": true}));
        let (head, body) = server.join().unwrap().remove(0);
        assert!(head.starts_with("POST /2015-03-31/functions/abgen-scenerun/invocations HTTP/1.1"));
        let lower = head.to_lowercase();
        assert!(lower.contains("authorization: aws4-hmac-sha256 credential=akidexample/"));
        assert!(lower.contains("/us-east-1/lambda/aws4_request"));
        assert!(lower.contains("signedheaders=host;x-amz-date;x-amz-security-token"));
        assert!(lower.contains("x-amz-security-token: example-token"));
        assert_eq!(body, br#"{"a":1}"#);
    }

    #[test]
    fn throttling_and_5xx_are_retried() {
        let (endpoint, server) = server(vec![
            "HTTP/1.1 429 Too Many Requests\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
            "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
            OK,
        ]);
        let got = invoker(&endpoint, None)
            .invoke(&serde_json::json!({}), BUDGET)
            .unwrap();
        assert_eq!(got, serde_json::json!({"ok": true}));
        assert_eq!(server.join().unwrap().len(), 3);
    }

    #[test]
    fn retries_stop_after_the_last_attempt() {
        const BUSY: &str =
            "HTTP/1.1 429 Too Many Requests\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
        let (endpoint, server) = server(vec![BUSY; MAX_ATTEMPTS as usize]);
        let err = invoker(&endpoint, None)
            .invoke(&serde_json::json!({}), Duration::from_secs(60))
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("HTTP 429") && msg.contains("4 attempt"),
            "{msg}"
        );
        assert_eq!(server.join().unwrap().len(), MAX_ATTEMPTS as usize);

        // A refused connection never reached Lambda: retried, then reported.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        let err = invoker(&endpoint, None)
            .invoke(&serde_json::json!({}), Duration::from_secs(60))
            .unwrap_err();
        assert!(format!("{err:#}").contains("4 attempt"), "{err:#}");
    }

    #[test]
    fn function_errors_and_4xx_are_not_retried() {
        let (endpoint, server) = server(vec![
            "HTTP/1.1 200 OK\r\nX-Amz-Function-Error: Unhandled\r\nContent-Length: 30\r\nConnection: close\r\n\r\n{\"errorMessage\":\"scene broke\"}",
            "HTTP/1.1 403 Forbidden\r\nContent-Length: 9\r\nConnection: close\r\n\r\n{\"x\":\"y\"}",
        ]);
        let inv = invoker(&endpoint, None);
        let err = inv.invoke(&serde_json::json!({}), BUDGET).unwrap_err();
        let err = format!("{err:#}");
        assert!(
            err.contains("Unhandled") && err.contains("scene broke"),
            "{err}"
        );
        let err = format!(
            "{:#}",
            inv.invoke(&serde_json::json!({}), BUDGET).unwrap_err()
        );
        assert!(err.contains("HTTP 403"), "{err}");
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[test]
    fn a_timeout_ends_the_invoke() {
        // The server reads the request and never answers.
        let (endpoint, server) = server(vec![""]);
        let started = Instant::now();
        let err = invoker(&endpoint, None)
            .invoke(&serde_json::json!({}), Duration::from_millis(500))
            .unwrap_err();
        assert!(format!("{err:#}").contains("no answer within"), "{err:#}");
        assert!(started.elapsed() < Duration::from_secs(5));
        server.join().unwrap();
    }
}

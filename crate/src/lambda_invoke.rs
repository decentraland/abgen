//! Synchronous AWS Lambda `Invoke` client, SigV4 sibling of [`crate::sns`] (shared creds
//! and hash primitives from [`crate::space`]).
//!
//! Only what the LOD lane needs: a `RequestResponse` invoke of a function named by its
//! plain name in the caller's region, a JSON payload in and the function's JSON result
//! out. A function error (`X-Amz-Function-Error`) is returned as an error carrying the
//! function's `errorMessage`.

use crate::space::{hex, hmac, sha256_hex, timestamps, CredsSource, ResolvedCreds};
use crate::Result;
use std::time::Duration;

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

fn split_endpoint(endpoint: &str) -> (String, String) {
    match endpoint.split_once("://") {
        Some((s, h)) => (s.to_string(), h.trim_end_matches('/').to_string()),
        None => (
            "https".to_string(),
            endpoint.trim_end_matches('/').to_string(),
        ),
    }
}

impl LambdaInvoker {
    /// `endpoint` overrides `https://lambda.{region}.amazonaws.com` (tests, local stacks).
    pub fn new(
        function: &str,
        region: &str,
        endpoint: Option<&str>,
        timeout: Duration,
    ) -> Result<LambdaInvoker> {
        if !valid_function_name(function) {
            crate::bail!("lambda function name {function:?} is not a plain function name");
        }
        let creds = CredsSource::from_env()
            .ok_or_else(|| crate::anyhow!("no AWS credentials to invoke lambda {function}"))?;
        Ok(Self::with_creds(function, region, endpoint, timeout, creds))
    }

    fn with_creds(
        function: &str,
        region: &str,
        endpoint: Option<&str>,
        timeout: Duration,
        creds: CredsSource,
    ) -> LambdaInvoker {
        let (scheme, host) = match endpoint.filter(|e| !e.is_empty()) {
            Some(e) => split_endpoint(e),
            None => (
                "https".to_string(),
                format!("lambda.{region}.amazonaws.com"),
            ),
        };
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .max_redirects(0)
            .http_status_as_error(false)
            .build()
            .into();
        LambdaInvoker {
            function: function.to_string(),
            scheme,
            host,
            region: region.to_string(),
            creds,
            agent,
        }
    }

    pub fn function(&self) -> &str {
        &self.function
    }

    fn path(&self) -> String {
        format!("/2015-03-31/functions/{}/invocations", self.function)
    }

    fn authorize(
        &self,
        c: &ResolvedCreds,
        payload_hash: &str,
        amz_date: &str,
        date: &str,
    ) -> String {
        let (signed_headers, canonical_headers) = match &c.session_token {
            Some(token) => (
                "host;x-amz-date;x-amz-security-token",
                format!(
                    "host:{}\nx-amz-date:{}\nx-amz-security-token:{}\n",
                    self.host, amz_date, token
                ),
            ),
            None => (
                "host;x-amz-date",
                format!("host:{}\nx-amz-date:{}\n", self.host, amz_date),
            ),
        };
        let canonical_request = format!(
            "POST\n{}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
            self.path()
        );
        let scope = format!("{date}/{}/lambda/aws4_request", self.region);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            sha256_hex(canonical_request.as_bytes())
        );
        let k_date = hmac(format!("AWS4{}", c.secret_key).as_bytes(), date.as_bytes());
        let k_region = hmac(&k_date, self.region.as_bytes());
        let k_service = hmac(&k_region, b"lambda");
        let k_signing = hmac(&k_service, b"aws4_request");
        let signature = hex(&hmac(&k_signing, string_to_sign.as_bytes()));
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            c.access_key
        )
    }

    /// One `RequestResponse` invoke. No retries: the caller decides whether a failed
    /// scene is worth running again.
    pub fn invoke(&self, payload: &serde_json::Value) -> Result<serde_json::Value> {
        let body = serde_json::to_vec(payload)?;
        let c = self.creds.resolve()?;
        let payload_hash = sha256_hex(&body);
        let (date, amz) = timestamps();
        let auth = self.authorize(&c, &payload_hash, &amz, &date);
        let url = format!("{}://{}{}", self.scheme, self.host, self.path());
        let mut req = self
            .agent
            .post(&url)
            .header("Content-Type", "application/json")
            .header("x-amz-date", &amz)
            .header("Authorization", &auth);
        if let Some(token) = &c.session_token {
            req = req.header("x-amz-security-token", token);
        }
        let resp = req
            .send(&body[..])
            .map_err(|e| crate::anyhow!("invoke lambda {}: {e}", self.function))?;
        let status = resp.status().as_u16();
        let function_error = resp
            .headers()
            .get("x-amz-function-error")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let mut out = Vec::new();
        use std::io::Read;
        resp.into_body()
            .into_reader()
            .read_to_end(&mut out)
            .map_err(|e| crate::anyhow!("read lambda {} response: {e}", self.function))?;
        if status != 200 {
            crate::bail!(
                "invoke lambda {}: HTTP {status}: {}",
                self.function,
                String::from_utf8_lossy(&out[..out.len().min(512)])
            );
        }
        let value: serde_json::Value = serde_json::from_slice(&out)
            .map_err(|e| crate::anyhow!("lambda {} returned non-JSON: {e}", self.function))?;
        if let Some(kind) = function_error {
            let message = value
                .get("errorMessage")
                .and_then(|m| m.as_str())
                .unwrap_or("no errorMessage");
            crate::bail!("lambda {} failed ({kind}): {message}", self.function);
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn creds(token: Option<&str>) -> CredsSource {
        CredsSource::Static(ResolvedCreds {
            access_key: "AKIDEXAMPLE".into(),
            secret_key: "example-secret".into(),
            session_token: token.map(str::to_string),
        })
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

    /// Serves one canned HTTP response and hands back the request it read.
    fn one_shot(response: &'static str) -> (String, std::thread::JoinHandle<(String, Vec<u8>)>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
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
            sock.write_all(response.as_bytes()).unwrap();
            (head, buf[body_start..body_start + len].to_vec())
        });
        (format!("http://{addr}"), handle)
    }

    #[test]
    fn invoke_signs_and_posts_the_payload() {
        let (endpoint, server) = one_shot(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
        );
        let inv = LambdaInvoker::with_creds(
            "abgen-scenerun",
            "us-east-1",
            Some(&endpoint),
            Duration::from_secs(5),
            creds(Some("example-token")),
        );
        let got = inv.invoke(&serde_json::json!({"a": 1})).unwrap();
        assert_eq!(got, serde_json::json!({"ok": true}));
        let (head, body) = server.join().unwrap();
        assert!(head.starts_with("POST /2015-03-31/functions/abgen-scenerun/invocations HTTP/1.1"));
        let lower = head.to_lowercase();
        assert!(lower.contains("authorization: aws4-hmac-sha256 credential=akidexample/"));
        assert!(lower.contains("/us-east-1/lambda/aws4_request"));
        assert!(lower.contains("signedheaders=host;x-amz-date;x-amz-security-token"));
        assert!(lower.contains("x-amz-security-token: example-token"));
        assert_eq!(body, br#"{"a":1}"#);
    }

    #[test]
    fn function_errors_surface_the_error_message() {
        let (endpoint, server) = one_shot(
            "HTTP/1.1 200 OK\r\nX-Amz-Function-Error: Unhandled\r\nContent-Length: 30\r\nConnection: close\r\n\r\n{\"errorMessage\":\"scene broke\"}",
        );
        let inv = LambdaInvoker::with_creds(
            "abgen-scenerun",
            "us-east-1",
            Some(&endpoint),
            Duration::from_secs(5),
            creds(None),
        );
        let err = inv.invoke(&serde_json::json!({})).unwrap_err().to_string();
        server.join().unwrap();
        assert!(
            err.contains("Unhandled") && err.contains("scene broke"),
            "{err}"
        );
    }

    #[test]
    fn non_200_is_an_error() {
        let (endpoint, server) = one_shot(
            "HTTP/1.1 403 Forbidden\r\nContent-Length: 9\r\nConnection: close\r\n\r\n{\"x\":\"y\"}",
        );
        let inv = LambdaInvoker::with_creds(
            "abgen-scenerun",
            "us-east-1",
            Some(&endpoint),
            Duration::from_secs(5),
            creds(None),
        );
        let err = inv.invoke(&serde_json::json!({})).unwrap_err().to_string();
        server.join().unwrap();
        assert!(err.contains("HTTP 403"), "{err}");
    }
}

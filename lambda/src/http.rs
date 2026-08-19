use serde_json::{json, Value};

pub const SECRET_HEADER: &str = "x-abgen-secret";

pub struct Request {
    pub method: String,
    headers: Value,
    body: Option<String>,
    body_is_base64: bool,
}

impl Request {
    /// `Some` only for Lambda Function URL / API Gateway payload-format-2.0
    /// events; anything else (SQS batch, direct invoke) is not HTTP-shaped.
    pub fn from_event(event: &Value) -> Option<Request> {
        let method = event
            .pointer("/requestContext/http/method")
            .and_then(Value::as_str)?;
        Some(Request {
            method: method.to_ascii_uppercase(),
            headers: event.get("headers").cloned().unwrap_or(Value::Null),
            body: event
                .get("body")
                .and_then(Value::as_str)
                .map(str::to_string),
            body_is_base64: event
                .get("isBase64Encoded")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .as_object()?
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .and_then(|(_, v)| v.as_str())
    }

    fn body_bytes(&self) -> Result<Vec<u8>, &'static str> {
        let raw = self.body.as_deref().unwrap_or("");
        if self.body_is_base64 {
            base64_decode(raw).ok_or("body is not valid base64")
        } else {
            Ok(raw.as_bytes().to_vec())
        }
    }
}

/// Authenticate and decode an HTTP invocation into the JSON payload the SQS
/// path would have carried. `Err` is a ready-to-return HTTP response.
pub fn accept(secret: Option<&str>, req: &Request) -> Result<Value, Value> {
    let Some(secret) = secret else {
        eprintln!("http: rejected — ABGEN_HTTP_SECRET is not set");
        return Err(respond(
            503,
            json!({"error": "http invocation disabled: ABGEN_HTTP_SECRET is not set"}),
        ));
    };
    let presented = req.header(SECRET_HEADER).unwrap_or("");
    if !secret_matches(presented, secret) {
        eprintln!("http: rejected — bad or missing {SECRET_HEADER}");
        return Err(respond(401, json!({"error": "unauthorized"})));
    }
    if req.method != "POST" {
        return Err(respond(
            405,
            json!({"error": format!("method {} not allowed, use POST", req.method)}),
        ));
    }
    let bytes = req
        .body_bytes()
        .map_err(|e| respond(400, json!({"error": e})))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| respond(400, json!({"error": format!("body is not JSON: {e}")})))
}

pub fn respond(status: u16, body: Value) -> Value {
    json!({
        "statusCode": status,
        "headers": {"content-type": "application/json"},
        "isBase64Encoded": false,
        "body": body.to_string(),
    })
}

/// Hash-then-compare: the byte comparison always runs over two equal-length
/// SHA-256 digests, so neither its duration nor an early length check can
/// leak how long the configured secret is.
fn secret_matches(presented: &str, secret: &str) -> bool {
    if secret.is_empty() {
        return false;
    }
    let a = abgen::hashes::sha256_hex(presented.as_bytes());
    let b = abgen::hashes::sha256_hex(secret.as_bytes());
    constant_time_eq(a.as_bytes(), b.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() || a.is_empty() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            b'\r' | b'\n' | b' ' | b'\t' => continue,
            _ => return None,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post(body: &str, secret: Option<&str>) -> Value {
        let mut headers = serde_json::Map::new();
        if let Some(s) = secret {
            headers.insert(SECRET_HEADER.to_string(), json!(s));
        }
        json!({
            "version": "2.0",
            "requestContext": {"http": {"method": "POST", "path": "/"}},
            "headers": Value::Object(headers),
            "body": body,
            "isBase64Encoded": false,
        })
    }

    fn status(v: &Value) -> u64 {
        v["statusCode"].as_u64().unwrap()
    }

    #[test]
    fn ignores_non_http_events() {
        assert!(Request::from_event(&json!({"entityId": "bafkabc123"})).is_none());
        assert!(Request::from_event(&json!({"Records": [{"body": "{}"}]})).is_none());
    }

    #[test]
    fn accepts_authenticated_post() {
        let e = post(r#"{"entityId":"bafkabc123"}"#, Some("s3cret"));
        let req = Request::from_event(&e).unwrap();
        let body = accept(Some("s3cret"), &req).unwrap();
        assert_eq!(body["entityId"], "bafkabc123");
    }

    #[test]
    fn header_lookup_is_case_insensitive() {
        let mut e = post("{}", None);
        e["headers"] = json!({"X-Abgen-Secret": "s3cret"});
        let req = Request::from_event(&e).unwrap();
        assert_eq!(req.header(SECRET_HEADER), Some("s3cret"));
    }

    #[test]
    fn rejects_bad_or_missing_secret() {
        let cases = [
            None,
            Some(""),
            Some("wrong"),
            Some("s3cre"),
            Some("s3cret-and-more"),
        ];
        for presented in cases {
            let e = post("{}", presented);
            let req = Request::from_event(&e).unwrap();
            let err = accept(Some("s3cret"), &req).unwrap_err();
            assert_eq!(status(&err), 401, "presented {presented:?}");
        }
    }

    #[test]
    fn secret_comparison_handles_any_length_pair() {
        assert!(secret_matches("s3cret", "s3cret"));
        assert!(!secret_matches("", "s3cret"));
        assert!(!secret_matches("s3cret", ""));
        assert!(!secret_matches("", ""));
        assert!(!secret_matches("short", "a-much-longer-secret"));
        assert!(!secret_matches("a-much-longer-secret", "short"));
    }

    #[test]
    fn fails_closed_without_configured_secret() {
        let e = post("{}", Some("anything"));
        let req = Request::from_event(&e).unwrap();
        assert_eq!(status(&accept(None, &req).unwrap_err()), 503);
    }

    #[test]
    fn rejects_non_post_methods() {
        let mut e = post("{}", Some("s3cret"));
        e["requestContext"]["http"]["method"] = json!("get");
        let req = Request::from_event(&e).unwrap();
        let err = accept(Some("s3cret"), &req).unwrap_err();
        assert_eq!(status(&err), 405);
    }

    #[test]
    fn rejects_non_json_body() {
        let e = post("not json", Some("s3cret"));
        let req = Request::from_event(&e).unwrap();
        assert_eq!(status(&accept(Some("s3cret"), &req).unwrap_err()), 400);
    }

    #[test]
    fn decodes_base64_body() {
        let mut e = post("eyJlbnRpdHlJZCI6ImJhZmthYmMxMjMifQ==", Some("s3cret"));
        e["isBase64Encoded"] = json!(true);
        let req = Request::from_event(&e).unwrap();
        let body = accept(Some("s3cret"), &req).unwrap();
        assert_eq!(body["entityId"], "bafkabc123");

        e["body"] = json!("!!!!");
        let req = Request::from_event(&e).unwrap();
        assert_eq!(status(&accept(Some("s3cret"), &req).unwrap_err()), 400);
    }

    #[test]
    fn response_body_is_a_json_string() {
        let r = respond(200, json!({"jobs": []}));
        assert_eq!(r["statusCode"], 200);
        assert_eq!(r["body"].as_str().unwrap(), r#"{"jobs":[]}"#);
        assert_eq!(r["isBase64Encoded"], false);
    }
}

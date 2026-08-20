//! SNS `Publish` client, SigV4 sibling of [`crate::space`] (shared creds and
//! hash primitives). Configured by `ABGEN_SNS_TOPIC_ARN` (region parsed from
//! the ARN) + optional `ABGEN_SNS_ENDPOINT`. Message attributes matter:
//! SQS subscription filter policies match on them, not on the JSON body.

use crate::space::{agent, hex, hmac, sha256_hex, timestamps, CredsSource, ResolvedCreds};
use crate::Result;

pub struct Sns {
    topic_arn: String,
    scheme: String,
    host: String,
    region: String,
    creds: CredsSource,
}

/// AWS query-protocol form encoding — unlike `space::uri_encode_key` it does
/// NOT preserve `/`.
fn form_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `arn:aws:sns:us-east-1:123456789012:topic-name` → `us-east-1`.
fn region_from_arn(arn: &str) -> Option<String> {
    let mut parts = arn.split(':');
    if parts.next()? != "arn" {
        return None;
    }
    let _partition = parts.next()?;
    if parts.next()? != "sns" {
        return None;
    }
    let region = parts.next()?;
    (!region.is_empty()).then(|| region.to_string())
}

fn build_publish_body(topic_arn: &str, message: &str, attributes: &[(&str, &str)]) -> String {
    let mut body = format!(
        "Action=Publish&Version=2010-03-31&TopicArn={}&Message={}",
        form_encode(topic_arn),
        form_encode(message)
    );
    for (i, (name, value)) in attributes.iter().enumerate() {
        let n = i + 1;
        body.push_str(&format!(
            "&MessageAttributes.entry.{n}.Name={}\
             &MessageAttributes.entry.{n}.Value.DataType=String\
             &MessageAttributes.entry.{n}.Value.StringValue={}",
            form_encode(name),
            form_encode(value)
        ));
    }
    body
}

impl Sns {
    pub fn from_env() -> Option<Sns> {
        let topic_arn = std::env::var("ABGEN_SNS_TOPIC_ARN")
            .ok()
            .filter(|s| !s.is_empty())?;
        let creds = CredsSource::from_env()?;
        let region = region_from_arn(&topic_arn)
            .or_else(|| std::env::var("AWS_REGION").ok().filter(|s| !s.is_empty()))
            .unwrap_or_else(|| "us-east-1".to_string());
        let (scheme, host) = match std::env::var("ABGEN_SNS_ENDPOINT")
            .ok()
            .filter(|s| !s.is_empty())
        {
            Some(endpoint) => match endpoint.split_once("://") {
                Some((s, h)) => (s.to_string(), h.trim_end_matches('/').to_string()),
                None => (
                    "https".to_string(),
                    endpoint.trim_end_matches('/').to_string(),
                ),
            },
            None => ("https".to_string(), format!("sns.{region}.amazonaws.com")),
        };
        Some(Sns {
            topic_arn,
            scheme,
            host,
            region,
            creds,
        })
    }

    /// Process-wide so container-credential caching survives across publishes.
    pub fn global() -> Option<&'static Sns> {
        static S: std::sync::OnceLock<Option<Sns>> = std::sync::OnceLock::new();
        S.get_or_init(Sns::from_env).as_ref()
    }

    #[allow(clippy::too_many_arguments)]
    #[cfg(test)]
    fn with_static_creds(
        topic_arn: &str,
        scheme: &str,
        host: &str,
        region: &str,
        access_key: &str,
        secret_key: &str,
        session_token: Option<&str>,
    ) -> Sns {
        Sns {
            topic_arn: topic_arn.to_string(),
            scheme: scheme.to_string(),
            host: host.to_string(),
            region: region.to_string(),
            creds: CredsSource::Static(ResolvedCreds {
                access_key: access_key.to_string(),
                secret_key: secret_key.to_string(),
                session_token: session_token.map(str::to_string),
            }),
        }
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
        let canonical_request =
            format!("POST\n/\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}");
        let scope = format!("{date}/{}/sns/aws4_request", self.region);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            sha256_hex(canonical_request.as_bytes())
        );
        let k_date = hmac(format!("AWS4{}", c.secret_key).as_bytes(), date.as_bytes());
        let k_region = hmac(&k_date, self.region.as_bytes());
        let k_service = hmac(&k_region, b"sns");
        let k_signing = hmac(&k_service, b"aws4_request");
        let signature = hex(&hmac(&k_signing, string_to_sign.as_bytes()));
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            c.access_key
        )
    }

    pub fn publish(&self, message: &str, attributes: &[(&str, &str)]) -> Result<()> {
        let body = build_publish_body(&self.topic_arn, message, attributes);
        let c = self.creds.resolve()?;
        let payload_hash = sha256_hex(body.as_bytes());
        let (date, amz) = timestamps();
        let auth = self.authorize(&c, &payload_hash, &amz, &date);
        let url = format!("{}://{}/", self.scheme, self.host);
        let mut req = agent()
            .post(&url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("x-amz-date", &amz)
            .header("Authorization", &auth);
        if let Some(token) = &c.session_token {
            req = req.header("x-amz-security-token", token);
        }
        req.send(body.as_bytes())
            .map_err(|e| crate::anyhow!("sns publish to {}: {e}", self.topic_arn))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_parses_from_arn() {
        assert_eq!(
            region_from_arn("arn:aws:sns:us-east-1:123456789012:abgen-conversion-finished"),
            Some("us-east-1".to_string())
        );
        assert_eq!(region_from_arn("arn:aws:sqs:us-east-1:123:q"), None);
        assert_eq!(region_from_arn("not-an-arn"), None);
        assert_eq!(region_from_arn("arn:aws:sns::123:t"), None);
    }

    #[test]
    fn form_encoding_escapes_reserved() {
        assert_eq!(form_encode("abc-_.~123"), "abc-_.~123");
        assert_eq!(form_encode("a b/c:d"), "a%20b%2Fc%3Ad");
        assert_eq!(form_encode("{\"k\":1}"), "%7B%22k%22%3A1%7D");
    }

    #[test]
    fn publish_body_matches_query_protocol() {
        let body = build_publish_body(
            "arn:aws:sns:us-east-1:123:t",
            "{\"a\":1}",
            &[("type", "asset-bundle"), ("subType", "converted")],
        );
        assert_eq!(
            body,
            "Action=Publish&Version=2010-03-31\
             &TopicArn=arn%3Aaws%3Asns%3Aus-east-1%3A123%3At\
             &Message=%7B%22a%22%3A1%7D\
             &MessageAttributes.entry.1.Name=type\
             &MessageAttributes.entry.1.Value.DataType=String\
             &MessageAttributes.entry.1.Value.StringValue=asset-bundle\
             &MessageAttributes.entry.2.Name=subType\
             &MessageAttributes.entry.2.Value.DataType=String\
             &MessageAttributes.entry.2.Value.StringValue=converted"
        );
    }

    /// Accepts one HTTP request, returns (headers, body), responds 200.
    fn capture_one_request(
        listener: std::net::TcpListener,
    ) -> std::thread::JoinHandle<(String, Vec<u8>)> {
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut sock, _) = listener.accept().expect("accept");
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            let (headers_end, content_length) = loop {
                let n = sock.read(&mut tmp).expect("read");
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                    let len = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(str::to_string)
                        })
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    break (pos + 4, len);
                }
            };
            while buf.len() < headers_end + content_length {
                let n = sock.read(&mut tmp).expect("read body");
                buf.extend_from_slice(&tmp[..n]);
            }
            let headers = String::from_utf8_lossy(&buf[..headers_end]).to_string();
            let body = buf[headers_end..headers_end + content_length].to_vec();
            let resp = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            sock.write_all(resp).expect("write");
            (headers, body)
        })
    }

    #[test]
    fn publish_signs_and_posts_the_form() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = capture_one_request(listener);

        let sns = Sns::with_static_creds(
            "arn:aws:sns:us-east-1:123:t",
            "http",
            &format!("127.0.0.1:{}", addr.port()),
            "us-east-1",
            "AKIDEXAMPLE",
            "sekret",
            Some("session-tok"),
        );
        sns.publish("{\"a\":1}", &[("type", "asset-bundle")])
            .expect("publish");

        let (headers, body) = handle.join().expect("join");
        let lower = headers.to_ascii_lowercase();
        assert!(lower.starts_with("post / http/1.1"), "{headers}");
        assert!(lower.contains("content-type: application/x-www-form-urlencoded"));
        assert!(lower.contains("x-amz-security-token: session-tok"));
        assert!(headers.contains("/us-east-1/sns/aws4_request"));
        assert!(headers.contains("SignedHeaders=host;x-amz-date;x-amz-security-token"));
        let body = String::from_utf8(body).expect("utf8 body");
        assert!(body.starts_with("Action=Publish&Version=2010-03-31&TopicArn="));
        assert!(body.contains("MessageAttributes.entry.1.Value.StringValue=asset-bundle"));
    }
}

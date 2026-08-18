//! Minimal AWS SigV4 request signing over abgen's in-house SHA-256 — enough
//! for S3 `PutObject` and SQS `SendMessage`. No SDK, no async: the signature
//! is ~40 lines of well-specified HMAC chaining, verified against the AWS
//! documentation test vectors below.
//!
//! https://docs.aws.amazon.com/IAM/latest/UserGuide/create-signed-request.html

use anyhow::{Context, Result};

pub struct Credentials {
    pub access_key: String,
    pub secret_key: String,
    /// Present under a Lambda execution role (STS temporary credentials);
    /// signed and sent as `x-amz-security-token`.
    pub session_token: Option<String>,
}

impl Credentials {
    pub fn from_env() -> Result<Self> {
        Ok(Credentials {
            access_key: std::env::var("AWS_ACCESS_KEY_ID")
                .context("AWS_ACCESS_KEY_ID is not set")?,
            secret_key: std::env::var("AWS_SECRET_ACCESS_KEY")
                .context("AWS_SECRET_ACCESS_KEY is not set")?,
            session_token: std::env::var("AWS_SESSION_TOKEN")
                .ok()
                .filter(|t| !t.is_empty()),
        })
    }
}

pub fn region_from_env() -> String {
    std::env::var("AWS_REGION")
        .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
        .unwrap_or_else(|_| "us-east-1".to_string())
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&abgen::hashes::sha256(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..64 {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = abgen::hashes::Sha256::new();
    inner.update(&ipad);
    inner.update(msg);
    let ih = inner.finalize();
    let mut outer = abgen::hashes::Sha256::new();
    outer.update(&opad);
    outer.update(&ih);
    outer.finalize()
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// SigV4 URI encoding: RFC 3986 unreserved characters pass through, `/` is
/// preserved when encoding a path (S3 canonical URIs are encoded exactly
/// once — no double encoding).
pub fn uri_encode(input: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b'/' if !encode_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `(YYYYMMDDTHHMMSSZ, YYYYMMDD)` — SigV4's two timestamp forms.
pub fn amz_date_now() -> (String, String) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    amz_date(secs)
}

fn amz_date(total_secs: u64) -> (String, String) {
    let days = (total_secs / 86_400) as i64;
    let sod = total_secs % 86_400;
    let (h, mi, s) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    let (y, mo, d) = abgen::dates::civil_from_days(days);
    (
        format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z"),
        format!("{y:04}{mo:02}{d:02}"),
    )
}

pub struct SignParams<'a> {
    pub method: &'a str,
    pub host: &'a str,
    /// Absolute path, already SigV4 URI-encoded (segments encoded, `/` kept).
    pub canonical_uri: &'a str,
    /// Sorted, encoded query string; empty when there is none.
    pub canonical_query: &'a str,
    /// Headers to sign beyond host / x-amz-date / x-amz-content-sha256 /
    /// x-amz-security-token, as (lowercase name, trimmed value).
    pub extra_headers: &'a [(String, String)],
    pub payload_sha256_hex: &'a str,
    pub service: &'a str,
    pub region: &'a str,
    pub amz_date: &'a str,
    pub date: &'a str,
}

/// Returns the `Authorization` header value. The caller must send exactly the
/// signed headers with the request (host comes from the URL).
pub fn authorization_header(creds: &Credentials, p: &SignParams) -> String {
    let mut headers: Vec<(String, String)> = vec![
        ("host".to_string(), p.host.to_string()),
        (
            "x-amz-content-sha256".to_string(),
            p.payload_sha256_hex.to_string(),
        ),
        ("x-amz-date".to_string(), p.amz_date.to_string()),
    ];
    if let Some(t) = &creds.session_token {
        headers.push(("x-amz-security-token".to_string(), t.clone()));
    }
    headers.extend(p.extra_headers.iter().cloned());
    headers.sort();

    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers: String = headers
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        p.method,
        p.canonical_uri,
        p.canonical_query,
        canonical_headers,
        signed_headers,
        p.payload_sha256_hex
    );
    let scope = format!("{}/{}/{}/aws4_request", p.date, p.region, p.service);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        p.amz_date,
        scope,
        abgen::hashes::sha256_hex(canonical_request.as_bytes())
    );

    let k_date = hmac_sha256(format!("AWS4{}", creds.secret_key).as_bytes(), p.date.as_bytes());
    let k_region = hmac_sha256(&k_date, p.region.as_bytes());
    let k_service = hmac_sha256(&k_region, p.service.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = hex(&hmac_sha256(&k_signing, string_to_sign.as_bytes()));

    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        creds.access_key
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4231 test case 1.
    #[test]
    fn hmac_sha256_rfc4231_vector() {
        let key = [0x0bu8; 20];
        let out = hmac_sha256(&key, b"Hi There");
        assert_eq!(
            hex(&out),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    /// The S3 SigV4 example from the AWS docs (GET /test.txt on
    /// examplebucket, 2013-05-24, us-east-1):
    /// https://docs.aws.amazon.com/AmazonS3/latest/API/sig-v4-header-based-auth.html
    #[test]
    fn s3_docs_get_object_vector() {
        let creds = Credentials {
            access_key: "AKIAIOSFODNN7EXAMPLE".to_string(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_string(),
            session_token: None,
        };
        let empty_payload_sha =
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let auth = authorization_header(
            &creds,
            &SignParams {
                method: "GET",
                host: "examplebucket.s3.amazonaws.com",
                canonical_uri: "/test.txt",
                canonical_query: "",
                extra_headers: &[("range".to_string(), "bytes=0-9".to_string())],
                payload_sha256_hex: empty_payload_sha,
                service: "s3",
                region: "us-east-1",
                amz_date: "20130524T000000Z",
                date: "20130524",
            },
        );
        assert!(
            auth.ends_with(
                "Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
            ),
            "unexpected authorization header: {auth}"
        );
    }

    #[test]
    fn uri_encoding() {
        assert_eq!(uri_encode("v49/assets/Qm_1.bin", false), "v49/assets/Qm_1.bin");
        assert_eq!(uri_encode("a b+c", true), "a%20b%2Bc");
        assert_eq!(uri_encode("x/y", true), "x%2Fy");
    }

    #[test]
    fn amz_date_formats() {
        // 2013-05-24T00:00:00Z = 1369353600
        assert_eq!(
            amz_date(1_369_353_600),
            ("20130524T000000Z".to_string(), "20130524".to_string())
        );
    }
}

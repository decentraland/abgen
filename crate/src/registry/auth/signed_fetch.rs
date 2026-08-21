use axum::http::HeaderMap;
use thiserror::Error;

use super::error::AuthError;
use super::types::{
    AuthLink as CryptoAuthLink, AuthLinkType as CryptoAuthLinkType, EthAddress,
    MAX_AUTH_CHAIN_LINKS,
};
use super::verify::verify_auth_chain;

pub const AUTH_CHAIN_HEADER_PREFIX: &str = "x-identity-auth-chain-";
pub const AUTH_TIMESTAMP_HEADER: &str = "x-identity-timestamp";
pub const AUTH_METADATA_HEADER: &str = "x-identity-metadata";

pub const FIVE_MINUTES: i64 = 5 * 60;

#[derive(Debug, Clone)]
pub struct AuthLink {
    pub kind: CryptoAuthLinkType,
    pub payload: String,
    pub signature: String,
}

#[derive(Debug, Clone)]
pub struct AuthChain {
    pub links: Vec<AuthLink>,
    pub signer: EthAddress,
}

#[derive(Debug, Error)]
pub enum AuthChainError {
    #[error("Invalid Auth Chain")]
    MalformedChain { detail: String },
    #[error("Invalid Auth Chain")]
    InsufficientLinks,
    #[error("Missing timestamp")]
    MissingTimestamp,
    #[error("Expired signature")]
    Expired {
        signed_at: i64,
        now: i64,
        window_secs: i64,
    },
    #[error("Invalid signature")]
    InvalidSignature(String),
    #[error("EIP-1654 not implemented")]
    EipNotImplemented,
}

/// Builds the string the auth-chain signature is checked against.
///
/// Only the method and path are lowercased. The timestamp and metadata are interpolated verbatim,
/// so the metadata's bytes -- and with them its casing -- are covered by the signature.
///
/// Folding the whole payload, as this did previously, left the metadata's casing outside the
/// signature: two spellings of the same metadata shared one valid signature, so a key or value
/// could be renamed or re-cased between signing and delivery and still verify. Nothing here reads
/// the metadata beyond building this payload, so that was latent rather than exploitable, but it
/// meant any future read of a metadata field would have been reading something unsigned.
///
/// Matches `createPayload` in `@dcl/crypto-middleware` 6.x, so a client signs one payload that
/// every Decentraland verifier reconstructs identically.
pub fn build_payload(method: &str, path: &str, timestamp: &str, metadata: &str) -> String {
    format!(
        "{}:{}:{}:{}",
        method.to_lowercase(),
        path.to_lowercase(),
        timestamp,
        metadata
    )
}

fn signed_fetch_path<'a>(headers: &HeaderMap, fallback: &'a str) -> std::borrow::Cow<'a, str> {
    match headers.get("x-original-path").and_then(|v| v.to_str().ok()) {
        Some(raw) => std::borrow::Cow::Owned(raw.split('?').next().unwrap_or(raw).to_string()),
        None => std::borrow::Cow::Borrowed(fallback),
    }
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

pub fn extract_auth_chain(headers: &HeaderMap) -> Result<AuthChain, AuthChainError> {
    let mut links = Vec::new();

    for i in 0..MAX_AUTH_CHAIN_LINKS {
        let name = format!("{}{}", AUTH_CHAIN_HEADER_PREFIX, i);
        let Some(raw) = header_str(headers, &name) else {
            break;
        };

        let link: CryptoAuthLink = serde_json::from_str(raw).map_err(|e| {
            let mut detail = e.to_string();
            if detail.len() > 64 {
                detail.truncate(64);
            }
            AuthChainError::MalformedChain { detail }
        })?;

        match link.link_type {
            CryptoAuthLinkType::SIGNER => {
                if i != 0 {
                    return Err(AuthChainError::MalformedChain {
                        detail: format!("SIGNER link at non-zero index {}", i),
                    });
                }
            }
            _ => {
                if i == 0 {
                    return Err(AuthChainError::MalformedChain {
                        detail: "first link must be SIGNER".to_string(),
                    });
                }
                if link.signature.as_deref().unwrap_or("").is_empty() {
                    return Err(AuthChainError::MalformedChain {
                        detail: format!("missing signature on link {}", i),
                    });
                }
            }
        }

        links.push(AuthLink {
            kind: link.link_type,
            payload: link.payload,
            signature: link.signature.unwrap_or_default(),
        });
    }

    let overflow = format!("{}{}", AUTH_CHAIN_HEADER_PREFIX, MAX_AUTH_CHAIN_LINKS);
    if header_str(headers, &overflow).is_some() {
        return Err(AuthChainError::MalformedChain {
            detail: format!("exceeds max length of {}", MAX_AUTH_CHAIN_LINKS),
        });
    }
    if links.len() < 2 {
        return Err(AuthChainError::InsufficientLinks);
    }
    let signer = links[0].payload.to_lowercase();
    Ok(AuthChain { links, signer })
}

pub fn validate_signature(
    chain: &AuthChain,
    payload: &str,
    timestamp: &str,
    expiration_secs: i64,
    now: i64,
) -> Result<EthAddress, AuthChainError> {
    if let Ok(signed_at_ms) = timestamp.parse::<i64>() {
        let signed_at = signed_at_ms / 1000;
        if (now - signed_at).abs() > expiration_secs {
            return Err(AuthChainError::Expired {
                signed_at,
                now,
                window_secs: expiration_secs,
            });
        }
    }

    let crypto_chain: Vec<CryptoAuthLink> = chain
        .links
        .iter()
        .map(|link| CryptoAuthLink {
            link_type: link.kind,
            payload: link.payload.clone(),
            signature: if link.signature.is_empty() {
                None
            } else {
                Some(link.signature.clone())
            },
        })
        .collect();

    verify_auth_chain(&crypto_chain, payload, Some(now * 1000)).map_err(map_auth_error)?;
    Ok(chain.signer.clone())
}

fn map_auth_error(err: AuthError) -> AuthChainError {
    match err {
        AuthError::MalformedChain(d) => AuthChainError::MalformedChain { detail: d },
        AuthError::MissingSignature { .. } => AuthChainError::MalformedChain {
            detail: err.to_string(),
        },
        AuthError::RecoveryFailed(d) => AuthChainError::InvalidSignature(d),
        AuthError::SignerMismatch { .. } | AuthError::FinalAuthorityMismatch { .. } => {
            AuthChainError::InvalidSignature(err.to_string())
        }
        AuthError::EphemeralExpired {
            expiration_ms,
            now_ms,
        } => AuthChainError::Expired {
            signed_at: expiration_ms / 1000,
            now: now_ms / 1000,
            window_secs: 0,
        },
        AuthError::InvalidEphemeralPayload(d) => AuthChainError::MalformedChain { detail: d },
        AuthError::Eip1654NotImplemented => AuthChainError::EipNotImplemented,
    }
}

pub fn require_signer(
    headers: &HeaderMap,
    method: &str,
    path: &str,
) -> Result<String, AuthChainError> {
    let path = signed_fetch_path(headers, path);
    let path = path.as_ref();
    let chain = extract_auth_chain(headers)?;
    let ts = header_str(headers, AUTH_TIMESTAMP_HEADER)
        .ok_or(AuthChainError::MissingTimestamp)?
        .to_string();
    let metadata = header_str(headers, AUTH_METADATA_HEADER)
        .unwrap_or("{}")
        .to_string();
    let payload = build_payload(method, path, &ts, &metadata);
    let now = chrono::Utc::now().timestamp();
    validate_signature(&chain, &payload, &ts, FIVE_MINUTES, now)
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_payload_lowercases_the_method_and_path() {
        assert_eq!(
            build_payload("POST", "/Registry/Status", "1700000000000", "{}"),
            "post:/registry/status:1700000000000:{}"
        );
    }

    #[test]
    fn build_payload_leaves_the_metadata_verbatim() {
        // The point of the format: a client and a verifier that disagree about metadata casing
        // would disagree about the signature, so the casing cannot drift after signing.
        let metadata = r#"{"sceneId":"QmAbC","signer":"dcl:explorer"}"#;

        let payload = build_payload("GET", "/", "1700000000000", metadata);

        assert!(payload.ends_with(metadata), "metadata was rewritten: {payload}");
    }

    #[test]
    fn build_payload_distinguishes_metadata_that_differs_only_in_case() {
        // Under the previous fold these two collapsed to the same string, which is what let a
        // re-spelled field ride an otherwise valid signature.
        let lower = build_payload("GET", "/", "1700000000000", r#"{"signer":"dcl:explorer"}"#);
        let upper = build_payload("GET", "/", "1700000000000", r#"{"Signer":"dcl:explorer"}"#);

        assert_ne!(lower, upper);
    }
}

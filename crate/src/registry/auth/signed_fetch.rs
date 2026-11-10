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
    #[error("Invalid timestamp")]
    InvalidTimestamp,
    #[error("Invalid metadata")]
    InvalidMetadata,
    #[error("Invalid request path")]
    InvalidPath,
    #[error("decentraland-kernel-scene signer is not allowed")]
    ForbiddenSigner,
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
/// could be renamed or re-cased between signing and delivery and still verify. The signer policy
/// below must read exactly the metadata that was authenticated.
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

fn signed_fetch_path<'a>(
    headers: &HeaderMap,
    actual_path: &'a str,
) -> Result<std::borrow::Cow<'a, str>, AuthChainError> {
    let Some(original) = headers.get("x-original-path") else {
        return Ok(std::borrow::Cow::Borrowed(actual_path));
    };
    let original = original.to_str().map_err(|_| AuthChainError::InvalidPath)?;
    let original = original.split('?').next().unwrap_or(original);
    const REGISTRY_PREFIX: &str = "/asset-bundle-registry";
    let unprefixed = if original
        .get(..REGISTRY_PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(REGISTRY_PREFIX))
    {
        &original[REGISTRY_PREFIX.len()..]
    } else {
        original
    };
    // Nix forwards either the service path or its known Registry prefix. An
    // arbitrary original-path header cannot move a signature to another target.
    if !unprefixed.eq_ignore_ascii_case(actual_path) {
        return Err(AuthChainError::InvalidPath);
    }
    Ok(std::borrow::Cow::Owned(original.to_string()))
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
            let detail = e.to_string().chars().take(64).collect();
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
    now_ms: i64,
) -> Result<EthAddress, AuthChainError> {
    if timestamp.is_empty() || !timestamp.bytes().all(|c| c.is_ascii_digit()) {
        return Err(AuthChainError::InvalidTimestamp);
    }
    let signed_at_ms = timestamp
        .parse::<i64>()
        .map_err(|_| AuthChainError::InvalidTimestamp)?;
    let distance = (i128::from(now_ms) - i128::from(signed_at_ms)).abs();
    if expiration_secs < 0 || distance > i128::from(expiration_secs) * 1000 {
        return Err(AuthChainError::Expired {
            signed_at: signed_at_ms / 1000,
            now: now_ms / 1000,
            window_secs: expiration_secs,
        });
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

    verify_auth_chain(&crypto_chain, payload, Some(now_ms)).map_err(map_auth_error)?;
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
    // Validate target binding before trying the two fixed public aliases. A
    // gateway can prepend the Registry prefix after a host-based client signed.
    signed_fetch_path(headers, path)?;
    let chain = extract_auth_chain(headers)?;
    let ts = header_str(headers, AUTH_TIMESTAMP_HEADER)
        .ok_or(AuthChainError::MissingTimestamp)?
        .to_string();
    let metadata = header_str(headers, AUTH_METADATA_HEADER)
        .unwrap_or("{}")
        .to_string();
    let payload = build_payload(method, path, &ts, &metadata);
    let now_ms = chrono::Utc::now().timestamp_millis();
    let signer =
        validate_signature(&chain, &payload, &ts, FIVE_MINUTES, now_ms).or_else(|error| {
            if !matches!(error, AuthChainError::InvalidSignature(_)) {
                return Err(error);
            }
            let alias = format!("/asset-bundle-registry{path}");
            validate_signature(
                &chain,
                &build_payload(method, &alias, &ts, &metadata),
                &ts,
                FIVE_MINUTES,
                now_ms,
            )
        })?;
    // Apply the policy to the metadata bytes covered by the signature, never
    // to a separate caller-controlled header.
    let metadata: serde_json::Value =
        serde_json::from_str(&metadata).map_err(|_| AuthChainError::InvalidMetadata)?;
    if metadata.get("signer").and_then(serde_json::Value::as_str)
        == Some("decentraland-kernel-scene")
    {
        return Err(AuthChainError::ForbiddenSigner);
    }
    Ok(signer)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn malformed_non_ascii_auth_chain_returns_an_error_without_panicking() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-identity-auth-chain-0",
            format!(
                r#"{{"type":"{}","payload":"invalid"}}"#,
                "\\u00e9".repeat(100)
            )
            .parse()
            .unwrap(),
        );
        assert!(matches!(
            extract_auth_chain(&headers),
            Err(AuthChainError::MalformedChain { .. })
        ));
    }

    pub(crate) fn signed_headers(
        method: &str,
        path: &str,
        timestamp: &str,
        metadata: &str,
    ) -> (HeaderMap, String) {
        use ethers_core::k256::ecdsa::SigningKey;
        use ethers_core::utils::{hash_message, keccak256};

        let key = SigningKey::from_slice(&[7u8; 32]).unwrap();
        let public_key = key.verifying_key().to_encoded_point(false);
        let address_hash = keccak256(&public_key.as_bytes()[1..]);
        let address = format!("0x{}", hex::encode(&address_hash[12..]));
        let payload = build_payload(method, path, timestamp, metadata);
        let (signature, recovery_id) = key
            .sign_prehash_recoverable(hash_message(&payload).as_bytes())
            .unwrap();
        let mut signature = signature.to_bytes().to_vec();
        signature.push(recovery_id.to_byte() + 27);
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-identity-auth-chain-0",
            serde_json::json!({"type":"SIGNER", "payload":address})
                .to_string()
                .parse()
                .unwrap(),
        );
        headers.insert(
            "x-identity-auth-chain-1",
            serde_json::json!({
                "type":"ECDSA_SIGNED_ENTITY", "payload":payload,
                "signature":format!("0x{}", hex::encode(signature))
            })
            .to_string()
            .parse()
            .unwrap(),
        );
        headers.insert(AUTH_TIMESTAMP_HEADER, timestamp.parse().unwrap());
        headers.insert(AUTH_METADATA_HEADER, metadata.parse().unwrap());
        (headers, address)
    }

    #[test]
    fn signed_metadata_policy_cannot_be_bypassed_with_auxiliary_header() {
        let timestamp = chrono::Utc::now().timestamp_millis().to_string();
        let (mut headers, _) = signed_headers(
            "POST",
            "/denylist/entity",
            &timestamp,
            r#"{"signer":"decentraland-kernel-scene"}"#,
        );
        headers.insert("x-identity-metadata-signer", "explorer".parse().unwrap());
        assert!(matches!(
            require_signer(&headers, "post", "/denylist/entity"),
            Err(AuthChainError::ForbiddenSigner)
        ));

        let (mut headers, address) = signed_headers(
            "POST",
            "/denylist/entity",
            &timestamp,
            r#"{"signer":"explorer"}"#,
        );
        assert_eq!(
            require_signer(&headers, "post", "/denylist/entity").unwrap(),
            address
        );
        headers.insert(
            AUTH_METADATA_HEADER,
            r#"{"signer":"other"}"#.parse().unwrap(),
        );
        assert!(require_signer(&headers, "post", "/denylist/entity").is_err());
    }

    #[test]
    fn signed_requests_require_numeric_fresh_timestamps_at_millisecond_precision() {
        let now_ms = 1_700_000_000_123i64;
        for offset in [-300_000, 0, 300_000] {
            let timestamp = (now_ms + offset).to_string();
            let (headers, address) = signed_headers("get", "/entities/status", &timestamp, "{}");
            let chain = extract_auth_chain(&headers).unwrap();
            let payload = build_payload("get", "/entities/status", &timestamp, "{}");
            assert_eq!(
                validate_signature(&chain, &payload, &timestamp, FIVE_MINUTES, now_ms).unwrap(),
                address
            );
        }
        for timestamp in [
            "NaN".to_string(),
            "+1700000000123".to_string(),
            "9223372036854775808".to_string(),
            (now_ms - 300_001).to_string(),
            (now_ms + 300_001).to_string(),
        ] {
            let (headers, _) = signed_headers("get", "/entities/status", &timestamp, "{}");
            let chain = extract_auth_chain(&headers).unwrap();
            let payload = build_payload("get", "/entities/status", &timestamp, "{}");
            assert!(
                validate_signature(&chain, &payload, &timestamp, FIVE_MINUTES, now_ms).is_err(),
                "accepted {timestamp}"
            );
        }
    }

    #[test]
    fn invalid_signed_metadata_is_rejected_and_gateway_path_is_authenticated() {
        let timestamp = chrono::Utc::now().timestamp_millis().to_string();
        let (headers, _) = signed_headers("get", "/entities/status", &timestamp, "{");
        assert!(matches!(
            require_signer(&headers, "get", "/entities/status"),
            Err(AuthChainError::InvalidMetadata)
        ));

        let (mut headers, address) = signed_headers(
            "get",
            "/asset-bundle-registry/entities/status",
            &timestamp,
            "{}",
        );
        headers.insert(
            "x-original-path",
            "/asset-bundle-registry/entities/status?ignored=true"
                .parse()
                .unwrap(),
        );
        assert_eq!(
            require_signer(&headers, "get", "/entities/status").unwrap(),
            address
        );
        assert!(require_signer(&headers, "delete", "/entities/status").is_err());
    }

    #[test]
    fn host_and_path_registry_aliases_authenticate_after_gateway_rewrite() {
        let timestamp = chrono::Utc::now().timestamp_millis().to_string();
        for signed in ["/entities/status", "/asset-bundle-registry/entities/status"] {
            let (mut headers, address) = signed_headers("get", signed, &timestamp, "{}");
            headers.insert(
                "x-original-path",
                "/asset-bundle-registry/entities/status?ignored=1"
                    .parse()
                    .unwrap(),
            );
            assert_eq!(
                require_signer(&headers, "get", "/entities/status").unwrap(),
                address
            );
            assert!(require_signer(&headers, "get", "/denylist/entity").is_err());
        }
    }

    #[test]
    fn forwarded_path_cannot_replay_a_signature_for_another_denylist_entity() {
        let timestamp = chrono::Utc::now().timestamp_millis().to_string();
        for original in ["/denylist/id1", "/asset-bundle-registry/denylist/id1"] {
            let (mut headers, address) = signed_headers("post", original, &timestamp, "{}");
            headers.insert("x-original-path", original.parse().unwrap());
            assert_eq!(
                require_signer(&headers, "post", "/denylist/id1").unwrap(),
                address
            );
            assert!(matches!(
                require_signer(&headers, "post", "/denylist/id2"),
                Err(AuthChainError::InvalidPath)
            ));
        }
        let (mut headers, address) = signed_headers(
            "post",
            "/ASSET-BUNDLE-REGISTRY/DENYLIST/ID1",
            &timestamp,
            "{}",
        );
        headers.insert(
            "x-original-path",
            "/ASSET-BUNDLE-REGISTRY/DENYLIST/ID1?ignored=true"
                .parse()
                .unwrap(),
        );
        assert_eq!(
            require_signer(&headers, "post", "/denylist/id1").unwrap(),
            address
        );
        for unrelated in [
            "/other-service/denylist/id1",
            "/asset-bundle-registry/denylist/id2",
            "/asset-bundle-registry/asset-bundle-registry/denylist/id1",
        ] {
            headers.insert("x-original-path", unrelated.parse().unwrap());
            assert!(matches!(
                require_signer(&headers, "post", "/denylist/id1"),
                Err(AuthChainError::InvalidPath)
            ));
        }
    }

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

        assert!(
            payload.ends_with(metadata),
            "metadata was rewritten: {payload}"
        );
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

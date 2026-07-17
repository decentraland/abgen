use tracing::debug;

use super::chain::{is_valid_auth_chain, parse_ephemeral_payload};
use super::error::AuthError;
use super::recover::recover_address;
use super::types::{AuthChain, AuthLink, AuthLinkType, MAX_AUTH_CHAIN_LINKS};

pub fn verify_auth_chain(
    chain: &AuthChain,
    expected_address: &str,
    now_ms: Option<i64>,
) -> Result<(), AuthError> {
    if chain.len() > MAX_AUTH_CHAIN_LINKS {
        return Err(AuthError::MalformedChain(format!(
            "auth chain too long: {} links (max {})",
            chain.len(),
            MAX_AUTH_CHAIN_LINKS
        )));
    }
    if !is_valid_auth_chain(chain) {
        return Err(AuthError::MalformedChain("invalid chain structure".into()));
    }

    let now_ms = now_ms.unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
    let mut current_authority = String::new();

    for (index, link) in chain.iter().enumerate() {
        match link.link_type {
            AuthLinkType::SIGNER => {
                current_authority = link.payload.clone();
                debug!(address = %current_authority, "SIGNER link: set initial authority");
            }

            AuthLinkType::EcdsaSignedEntity => {
                let signature = require_signature(link, index)?;
                let recovered = recover_address(link.payload.as_bytes(), &signature)?;

                check_address_match(&current_authority, &recovered, index)?;

                debug!(
                    payload = %link.payload,
                    signer = %recovered,
                    "ECDSA_SIGNED_ENTITY: verified"
                );
                current_authority = link.payload.clone();
            }

            AuthLinkType::EcdsaEphemeral => {
                let signature = require_signature(link, index)?;
                let (message, ephemeral_address, expiration_ms) =
                    parse_ephemeral_payload(&link.payload)?;

                if expiration_ms <= now_ms {
                    return Err(AuthError::EphemeralExpired {
                        expiration_ms,
                        now_ms,
                    });
                }

                let recovered = recover_address(message.as_bytes(), &signature)?;

                check_address_match(&current_authority, &recovered, index)?;

                debug!(
                    ephemeral = %ephemeral_address,
                    signer = %recovered,
                    expiration_ms,
                    "ECDSA_EPHEMERAL: verified, advancing authority to ephemeral address"
                );
                current_authority = ephemeral_address;
            }

            AuthLinkType::EcdsaEip1654Ephemeral | AuthLinkType::EcdsaEip1654SignedEntity => {
                return Err(AuthError::Eip1654NotImplemented);
            }
        }
    }

    if current_authority != expected_address {
        return Err(AuthError::FinalAuthorityMismatch {
            expected: expected_address.to_string(),
            actual: current_authority,
        });
    }

    Ok(())
}

fn require_signature(link: &AuthLink, index: usize) -> Result<String, AuthError> {
    match &link.signature {
        Some(sig) if !sig.is_empty() => Ok(sig.clone()),
        _ => Err(AuthError::MissingSignature {
            link_type: link.link_type.to_string(),
            index,
        }),
    }
}

fn check_address_match(expected: &str, actual: &str, index: usize) -> Result<(), AuthError> {
    if expected.to_lowercase() != actual.to_lowercase() {
        return Err(AuthError::SignerMismatch {
            index,
            expected: expected.to_lowercase(),
            actual: actual.to_lowercase(),
        });
    }
    Ok(())
}

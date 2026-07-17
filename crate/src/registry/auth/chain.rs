use super::error::AuthError;
use super::types::{AuthChain, AuthLinkType, MAX_AUTH_CHAIN_LINKS};

pub const MAX_AUTH_LINK_FIELD_LEN: usize = 100_000;

pub fn is_valid_auth_chain(chain: &AuthChain) -> bool {
    if chain.is_empty() {
        return false;
    }
    if chain.len() > MAX_AUTH_CHAIN_LINKS {
        return false;
    }
    for (i, link) in chain.iter().enumerate() {
        if i == 0 && link.link_type != AuthLinkType::SIGNER {
            return false;
        }
        if link.link_type == AuthLinkType::SIGNER && i != 0 {
            return false;
        }
        if link.payload.len() > MAX_AUTH_LINK_FIELD_LEN {
            return false;
        }
        if link
            .signature
            .as_ref()
            .is_some_and(|s| s.len() > MAX_AUTH_LINK_FIELD_LEN)
        {
            return false;
        }
    }
    true
}

fn is_valid_eth_address(addr: &str) -> bool {
    addr.len() == 42 && addr.starts_with("0x") && addr[2..].chars().all(|c| c.is_ascii_hexdigit())
}

pub fn parse_ephemeral_payload(payload: &str) -> Result<(String, String, i64), AuthError> {
    let message = payload.replace('\r', "");
    let parts: Vec<&str> = message.split('\n').collect();

    let ephemeral_prefix = "Ephemeral address: ";
    let expiration_prefix = "Expiration: ";

    if parts.len() < 3
        || !parts[1].starts_with(ephemeral_prefix)
        || !parts[2].starts_with(expiration_prefix)
    {
        return Err(AuthError::InvalidEphemeralPayload(
            "Expected 3 lines with 'Ephemeral address: ' on line 2 and 'Expiration: ' on line 3"
                .into(),
        ));
    }

    let ephemeral_address = parts[1][ephemeral_prefix.len()..].to_string();

    if !is_valid_eth_address(&ephemeral_address) {
        return Err(AuthError::InvalidEphemeralPayload(
            "invalid ephemeral address format".into(),
        ));
    }

    let expiration_str = &parts[2][expiration_prefix.len()..];

    let expiration = parse_expiration(expiration_str)?;

    Ok((message, ephemeral_address, expiration))
}

fn parse_expiration(s: &str) -> Result<i64, AuthError> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(dt.timestamp_millis());
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Ok(naive.and_utc().timestamp_millis());
        }
    }
    Err(AuthError::InvalidEphemeralPayload(format!(
        "Invalid expiration date '{}'",
        s
    )))
}

use ethers_core::types::{RecoveryMessage, Signature, H160};

use super::error::AuthError;

pub fn recover_address(message: &[u8], signature: &str) -> Result<String, AuthError> {
    let sig_bytes = parse_signature_hex(signature)?;
    reject_high_s(&sig_bytes)?;
    let sig = parse_ethers_signature(&sig_bytes)?;

    let recovered: H160 = sig
        .recover(RecoveryMessage::Data(message.to_vec()))
        .map_err(|e| AuthError::RecoveryFailed(format!("ecrecover failed: {}", e)))?;

    Ok(format!("{:#x}", recovered))
}

const SECP256K1_N: [u8; 32] = [
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe,
    0xba, 0xae, 0xdc, 0xe6, 0xaf, 0x48, 0xa0, 0x3b, 0xbf, 0xd2, 0x5e, 0x8c, 0xd0, 0x36, 0x41, 0x41,
];

const SECP256K1_HALF_N: [u8; 32] = [
    0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0x5d, 0x57, 0x6e, 0x73, 0x57, 0xa4, 0x50, 0x1d, 0xdf, 0xe9, 0x2f, 0x46, 0x68, 0x1b, 0x20, 0xa0,
];

fn reject_high_s(bytes: &[u8; 65]) -> Result<(), AuthError> {
    let s = &bytes[32..64];
    if s.iter().all(|&b| b == 0) {
        return Err(AuthError::RecoveryFailed("signature s is zero".into()));
    }
    if cmp_be(s, &SECP256K1_N) != std::cmp::Ordering::Less {
        return Err(AuthError::RecoveryFailed(
            "signature s >= group order n".into(),
        ));
    }
    if cmp_be(s, &SECP256K1_HALF_N) == std::cmp::Ordering::Greater {
        return Err(AuthError::RecoveryFailed(
            "non-canonical high-s signature rejected (malleability)".into(),
        ));
    }
    Ok(())
}

fn cmp_be(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    a.iter().cmp(b.iter())
}

fn parse_signature_hex(hex: &str) -> Result<[u8; 65], AuthError> {
    let hex = hex.strip_prefix("0x").unwrap_or(hex);

    if hex.len() != 130 {
        return Err(AuthError::RecoveryFailed(format!(
            "Signature hex must be 130 characters (65 bytes), got {}",
            hex.len()
        )));
    }

    let bytes = hex::decode(hex)
        .map_err(|e| AuthError::RecoveryFailed(format!("Invalid hex in signature: {}", e)))?;

    let mut arr = [0u8; 65];
    arr.copy_from_slice(&bytes);
    Ok(arr)
}

fn parse_ethers_signature(bytes: &[u8; 65]) -> Result<Signature, AuthError> {
    let mut v = bytes[64];
    if v >= 27 {
        v -= 27;
    }

    let mut sig_bytes = [0u8; 65];
    sig_bytes[..64].copy_from_slice(&bytes[..64]);
    sig_bytes[64] = v;

    Signature::try_from(sig_bytes.as_slice())
        .map_err(|e| AuthError::RecoveryFailed(format!("Invalid signature bytes: {}", e)))
}

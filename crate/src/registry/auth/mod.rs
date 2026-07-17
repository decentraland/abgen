//! Signed-fetch (x-identity-auth-chain) verification for the signed routes.
//! ECDSA personal-sign chains are verified locally; EIP-1654 contract-wallet
//! links are rejected as not implemented (verification would need an RPC).

mod chain;
mod error;
mod recover;
mod signed_fetch;
mod types;
mod verify;

pub(crate) use signed_fetch::require_signer;

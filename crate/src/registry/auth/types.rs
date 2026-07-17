use serde::{Deserialize, Serialize};

pub type EthAddress = String;

pub const MAX_AUTH_CHAIN_LINKS: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[allow(clippy::upper_case_acronyms)]
pub enum AuthLinkType {
    SIGNER,
    #[serde(rename = "ECDSA_EPHEMERAL")]
    EcdsaEphemeral,
    #[serde(rename = "ECDSA_SIGNED_ENTITY")]
    EcdsaSignedEntity,
    #[serde(rename = "ECDSA_EIP_1654_EPHEMERAL")]
    EcdsaEip1654Ephemeral,
    #[serde(rename = "ECDSA_EIP_1654_SIGNED_ENTITY")]
    EcdsaEip1654SignedEntity,
}

impl std::fmt::Display for AuthLinkType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SIGNER => write!(f, "SIGNER"),
            Self::EcdsaEphemeral => write!(f, "ECDSA_EPHEMERAL"),
            Self::EcdsaSignedEntity => write!(f, "ECDSA_SIGNED_ENTITY"),
            Self::EcdsaEip1654Ephemeral => write!(f, "ECDSA_EIP_1654_EPHEMERAL"),
            Self::EcdsaEip1654SignedEntity => write!(f, "ECDSA_EIP_1654_SIGNED_ENTITY"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthLink {
    #[serde(rename = "type")]
    pub link_type: AuthLinkType,

    pub payload: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

pub type AuthChain = Vec<AuthLink>;

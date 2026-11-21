use anyhow::Result;
use std::env;

pub struct Config {
    pub ab_registry_database_url: Option<String>,
    pub admin_token: Option<String>,
    pub denylist_moderators: Vec<String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            ab_registry_database_url: env::var("AB_REGISTRY_PG_CONNECTION_STRING").ok(),
            admin_token: env::var("API_ADMIN_TOKEN").ok(),
            denylist_moderators: env::var("DENYLIST_MODERATORS")
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect(),
        })
    }
}

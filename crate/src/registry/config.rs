use anyhow::{Context, Result};
use std::env;

pub struct Config {
    pub content_database_url: String,
    pub ab_registry_database_url: Option<String>,
    pub admin_token: Option<String>,
    pub denylist_moderators: Vec<String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            content_database_url: env::var("CONTENT_PG_CONNECTION_STRING")
                .context("missing CONTENT_PG_CONNECTION_STRING")?,
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

mod auth;
pub mod config;
pub mod handlers;
pub mod http;
pub mod ports;

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::routing::{delete, get, post};
use axum::Router;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

use dcl_contents::content::ContentComponent;
use dcl_contents::manifest_store::AbManifestStore;

use crate::registry::config::Config;
use crate::registry::ports::registry::RegistryStore;

pub struct AppStateInner {
    pub content: ContentComponent,
    pub manifests: AbManifestStore,
    pub registry: RegistryStore,
    pub admin_token: Option<String>,
    pub denylist_moderators: Vec<String>,
}

pub type AppState = Arc<AppStateInner>;

pub async fn build_state(cfg: &Config, out_root: &str) -> Result<AppState> {
    let content_opts = PgConnectOptions::from_str(&cfg.content_database_url)
        .context("invalid content DB connection string")?
        .options([
            ("statement_timeout", "60000"),
            ("idle_in_transaction_session_timeout", "30000"),
        ]);
    let content_pool = PgPoolOptions::new()
        .max_connections(10)
        .idle_timeout(Duration::from_secs(30))
        .connect_with(content_opts)
        .await
        .context("failed to connect content DB pool")?;

    let registry_pool = match &cfg.ab_registry_database_url {
        Some(url) => {
            let opts = PgConnectOptions::from_str(url)
                .context("invalid ab_registry DB connection string")?;
            let pool = PgPoolOptions::new()
                .max_connections(5)
                .connect_with(opts)
                .await
                .context("failed to connect ab_registry DB pool")?;
            sqlx::migrate!("./migrations")
                .run(&pool)
                .await
                .context("ab_registry migrations failed")?;
            Some(pool)
        }
        None => {
            tracing::warn!(
                "AB_REGISTRY_PG_CONNECTION_STRING unset — denylist + queue admin disabled"
            );
            None
        }
    };

    Ok(Arc::new(AppStateInner {
        content: ContentComponent::new(content_pool),
        manifests: AbManifestStore::new(out_root.to_string()),
        registry: RegistryStore::new(registry_pool),
        admin_token: cfg.admin_token.clone(),
        denylist_moderators: cfg.denylist_moderators.clone(),
    }))
}

pub fn signed_router() -> Router<AppState> {
    Router::new()
        .route(
            "/entities/status",
            get(handlers::status::get_entities_status_signed),
        )
        .route("/queues/status", get(handlers::queues::get_queues_status))
        .route("/queues/retry", post(handlers::queues::post_queues_retry))
        .route("/queues/pause", post(handlers::queues::post_queues_pause))
        .route("/queues/resume", post(handlers::queues::post_queues_resume))
        .route("/denylist", get(handlers::denylist::get_denylist))
        .route(
            "/denylist/{entity_id}",
            post(handlers::denylist::add_denylist).delete(handlers::denylist::remove_denylist),
        )
        .route("/registry", post(handlers::admin::post_registry))
        .route("/flush-cache", delete(handlers::admin::flush_cache))
}

#[cfg(test)]
mod tests {
    #[test]
    fn signed_router_constructs_without_route_conflicts() {
        let _ = super::signed_router();
    }
}

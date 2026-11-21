mod auth;
pub mod config;
pub mod conversion;
pub mod handlers;
pub mod http;
pub mod ports;
pub mod source;
pub mod worker;

use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::routing::{delete, get, post};
use axum::Router;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

#[cfg(test)]
use dcl_contents::content::ContentComponent;
use dcl_contents::manifest_store::AbManifestStore;
use dcl_contents::registry::{EntitySource, RegistryAppState};

use crate::registry::config::Config;
use crate::registry::ports::registry::RegistryStore;

pub struct AppStateInner {
    pub content: Arc<dyn EntitySource>,
    pub manifests: AbManifestStore,
    pub registry: RegistryStore,
    pub admin_token: Option<String>,
    pub denylist_moderators: Vec<String>,
}

pub type AppState = Arc<AppStateInner>;

pub async fn build_state(cfg: &Config, contents: &RegistryAppState) -> Result<AppState> {
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
        content: contents.content.clone(),
        manifests: contents.manifests.clone(),
        registry: RegistryStore::new(registry_pool),
        admin_token: cfg.admin_token.clone(),
        denylist_moderators: cfg.denylist_moderators.clone(),
    }))
}

pub fn signed_router(admin_enabled: bool) -> Router<AppState> {
    let router = Router::new()
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
        );
    if admin_enabled {
        router
            .route("/registry", post(handlers::admin::post_registry))
            .route("/flush-cache", delete(handlers::admin::flush_cache))
    } else {
        router
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn state(moderators: Vec<String>) -> AppState {
        Arc::new(AppStateInner {
            content: Arc::new(ContentComponent::new(
                PgPoolOptions::new()
                    .connect_lazy("postgres://test:test@127.0.0.1:1/test")
                    .unwrap(),
            )),
            manifests: AbManifestStore::new(std::env::temp_dir().join("registry-auth-tests")),
            registry: RegistryStore::new(None),
            admin_token: Some("test-admin-token".into()),
            denylist_moderators: moderators,
        })
    }

    async fn request(app: Router, method: &str, path: &str, headers: HeaderMap) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let mut wire = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 0\r\n");
        for (name, value) in &headers {
            wire.push_str(&format!("{name}: {}\r\n", value.to_str().unwrap()));
        }
        wire.push_str("\r\n");
        stream.write_all(wire.as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        server.abort();
        response
    }

    #[tokio::test]
    async fn administrative_routes_are_absent_when_not_configured() {
        for (method, path) in [("POST", "/registry"), ("DELETE", "/flush-cache")] {
            let response = request(
                signed_router(false).with_state(state(vec![])),
                method,
                path,
                HeaderMap::new(),
            )
            .await;
            assert!(response.starts_with("HTTP/1.1 404"), "{response}");
            let response = request(
                signed_router(true).with_state(state(vec![])),
                method,
                path,
                HeaderMap::new(),
            )
            .await;
            assert!(response.starts_with("HTTP/1.1 401"), "{response}");
        }
    }

    #[tokio::test]
    async fn moderation_requires_signed_moderator_even_with_admin_bearer() {
        for method in ["POST", "DELETE"] {
            let mut bearer = HeaderMap::new();
            bearer.insert("authorization", "Bearer test-admin-token".parse().unwrap());
            let response = request(
                signed_router(true).with_state(state(vec![])),
                method,
                "/denylist/entity",
                bearer,
            )
            .await;
            assert!(response.starts_with("HTTP/1.1 400"), "{response}");
            let body: serde_json::Value =
                serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap()).unwrap();
            assert_eq!(
                body,
                serde_json::json!({
                    "error": "Invalid Auth Chain",
                    "message": "This endpoint requires a signed fetch request. See ADR-44."
                })
            );

            let timestamp = chrono::Utc::now().timestamp_millis().to_string();
            let (headers, address) =
                auth::signed_headers(method, "/denylist/entity", &timestamp, "{}");
            let response = request(
                signed_router(true).with_state(state(vec![])),
                method,
                "/denylist/entity",
                headers.clone(),
            )
            .await;
            assert!(response.starts_with("HTTP/1.1 403"), "{response}");
            // A real signed moderator passes authentication and reaches the
            // deliberately unavailable persistence boundary without touching a DB.
            let response = request(
                signed_router(true).with_state(state(vec![address])),
                method,
                "/denylist/entity",
                headers,
            )
            .await;
            assert!(response.starts_with("HTTP/1.1 501"), "{response}");
        }
    }

    #[test]
    fn signed_router_constructs_without_route_conflicts() {
        let _ = super::signed_router(false);
        let _ = super::signed_router(true);
    }
}

use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use serde_json::json;

use dcl_contents::errors::{ApiError, ApiResult};
use dcl_contents::types::QueuesStatus;

use crate::registry::http::auth::require_bearer;
use crate::registry::AppState;

pub async fn get_queues_status(State(state): State<AppState>) -> ApiResult<Json<QueuesStatus>> {
    if !state.registry.enabled() {
        return Err(ApiError::not_implemented(
            "queue status requires the ab_registry DB",
        ));
    }
    // Missing or failed bundles do not imply that a conversion job is queued.
    let mut windows_pending_jobs: Vec<_> = state
        .registry
        .pending_jobs_for("windows")
        .await?
        .into_iter()
        .collect();
    let mut mac_pending_jobs: Vec<_> = state
        .registry
        .pending_jobs_for("mac")
        .await?
        .into_iter()
        .collect();
    windows_pending_jobs.sort();
    mac_pending_jobs.sort();
    Ok(Json(QueuesStatus {
        windows_pending_jobs,
        mac_pending_jobs,
    }))
}

pub async fn post_queues_retry(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<RetryBody>>,
) -> ApiResult<Json<serde_json::Value>> {
    require_bearer(&state, &headers)?;
    if !state.registry.enabled() {
        return Err(ApiError::not_implemented(
            "queue retry requires the ab_registry DB",
        ));
    }

    let entity_ids: Vec<String> = body
        .map(|b| b.0.entity_ids)
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if entity_ids.is_empty() {
        return Err(ApiError::bad_request("No entity ids provided"));
    }

    let mut retried: Vec<serde_json::Value> = Vec::new();
    let mut failures: Vec<serde_json::Value> = Vec::new();
    for entity_id in &entity_ids {
        match state.content.resolve_one(entity_id).await {
            Ok(Some(_)) => {
                let enqueued = state
                    .registry
                    .enqueue_build(entity_id, "admin-bearer")
                    .await?;
                let attempts = state
                    .registry
                    .record_retry(entity_id, "admin-bearer")
                    .await?;
                state.manifests.invalidate(entity_id).await;
                retried.push(json!({
                    "entityId": entity_id,
                    "attempts": attempts,
                    "enqueued": enqueued,
                }));
            }
            Ok(None) => {
                failures.push(json!({
                    "entityId": entity_id,
                    "error": "Entity not found in catalyst",
                }));
            }
            Err(err) => {
                tracing::error!(error = %err, entity_id = %entity_id, "retry resolve failed");
                failures.push(json!({ "entityId": entity_id, "error": err.to_string() }));
            }
        }
    }

    Ok(Json(json!({ "retried": retried, "failures": failures })))
}

pub async fn post_queues_pause(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    set_paused(&state, &headers, true).await
}

pub async fn post_queues_resume(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    set_paused(&state, &headers, false).await
}

async fn set_paused(
    state: &AppState,
    headers: &HeaderMap,
    paused: bool,
) -> ApiResult<Json<serde_json::Value>> {
    require_bearer(state, headers)?;
    if !state.registry.enabled() {
        return Err(ApiError::not_implemented(
            "queue pause/resume requires the ab_registry DB",
        ));
    }
    let value = state
        .registry
        .set_queue_paused(paused, "admin-bearer")
        .await?;
    Ok(Json(json!({ "ok": true, "paused": value })))
}

#[derive(Debug, serde::Deserialize, Default)]
pub struct RetryBody {
    #[serde(default, alias = "ids")]
    pub entity_ids: Vec<String>,
}

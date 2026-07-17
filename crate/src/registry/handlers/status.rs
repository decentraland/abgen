use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;

use dcl_contents::errors::{ApiError, ApiResult};
use dcl_contents::handlers::status::entity_status_from;
use dcl_contents::types::EntityStatus;

use crate::registry::AppState;

pub async fn get_entities_status_signed(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<EntityStatus>>> {
    let signer = require_signed_fetch(&headers, "get", "/entities/status")?;

    let ents = state.content.active_entities_by_deployer(&signer).await?;
    let mut out = Vec::with_capacity(ents.len());
    for ent in ents {
        let m = state.manifests.get(&ent.entity_id).await;
        out.push(entity_status_from(&ent.entity_id, &m, ent.is_world()));
    }
    Ok(Json(out))
}

pub(crate) fn require_signed_fetch(
    headers: &HeaderMap,
    method: &str,
    path: &str,
) -> Result<String, ApiError> {
    if let Some(signer) = headers
        .get("x-identity-metadata-signer")
        .and_then(|v| v.to_str().ok())
    {
        if signer == "decentraland-kernel-scene" {
            return Err(ApiError::unauthorized(
                "decentraland-kernel-scene signer is not allowed",
            ));
        }
    }

    crate::registry::auth::require_signer(headers, method, path)
        .map(|s| s.to_lowercase())
        .map_err(|e| ApiError::unauthorized(format!("signed-fetch verification failed: {e}")))
}

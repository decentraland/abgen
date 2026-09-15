use super::*;

pub async fn ping() -> &'static str {
    "ok"
}

pub async fn get_build_progress(
    State(state): State<AppState>,
    axum::extract::Path(entity): axum::extract::Path<String>,
) -> Response {
    let Some(proxy) = state.live_proxy.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match proxy.progress_snapshot(&entity) {
        Some(p) => Json(serde_json::json!({ "done": p.done, "total": p.total, "file": p.file }))
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

pub async fn metrics(token: Option<String>, headers: HeaderMap) -> Response {
    if !metrics_authorized(token.as_deref(), &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    super::super::metrics::metrics_handler().await
}

pub(super) fn metrics_authorized(token: Option<&str>, headers: &HeaderMap) -> bool {
    let Some(token) = token else {
        return true;
    };
    headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| ct_eq(t.as_bytes(), token.as_bytes()))
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub(super) fn is_ready(state: &AppState) -> bool {
    let root_present = state.out_root.is_dir();
    let jit = state.live_proxy.is_some();
    let jit_broken = jit && !state.live_template_ok;
    let templates_ok = state.templates_missing.is_empty();
    (templates_ok || !jit) && (jit || root_present) && !jit_broken
}

pub async fn livez() -> &'static str {
    "ok"
}

pub async fn readyz(State(state): State<AppState>) -> Response {
    let ready = is_ready(&state);
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let body = Json(serde_json::json!({
        "status": if ready { "ready" } else { "degraded" },
    }));
    (status, body).into_response()
}

fn content_db_status(state: &AppState) -> &'static str {
    if state.content_db.is_some() {
        "connected"
    } else {
        "fallback"
    }
}

fn registry_mode(state: &AppState) -> &'static str {
    if state.content_db.is_some() {
        "content-db"
    } else {
        "catalyst-proxy"
    }
}

/// `version` is this binary's crate version and `pid` its OS process id: the two fields a
/// loopback supervisor needs to tell one server build from another and to signal this process.
/// Both describe the running binary, not the corpus it serves.
pub async fn health(State(state): State<AppState>) -> Response {
    let root_present = state.out_root.is_dir();
    let jit = state.live_proxy.is_some();
    let ready = is_ready(&state);
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let mut body = serde_json::json!({
        "status": if ready { "ready" } else { "degraded" },
        "mode": if jit { "in-process" } else { "static" },
        "out_root": state.out_root.to_string_lossy(),
        "out_root_present": root_present,
        "out_root_writable": state.out_root_writable,
        "live_inprocess": jit,
        "template_ok": state.live_template_ok,
        "templates_missing": state.templates_missing,
        "bundle_index": state.bundle_index.len(),
        "turbojpeg": crate::live::Proxy::turbojpeg_available(),
        "content_db": content_db_status(&state),
        "registry": registry_mode(&state),
        "catalyst_url": state.catalyst_url,
        "ab_version": state.ab_version,
        "ab_date": state.ab_date,
        "version": env!("CARGO_PKG_VERSION"),
        "build_id": option_env!("ABGEN_BUILD_ID").unwrap_or("unknown"),
        "pid": std::process::id(),
        "lod_jit": {
            "enabled": state.lod_jit.enabled,
            "simplifier": state.lod_jit.simplifier.name(),
            "gltfpack": state.lod_jit.gltfpack.as_ref().map(|p| p.display().to_string()),
            "embedded_scene_runtime": cfg!(not(target_arch = "wasm32")),
            "disabled_reasons": state.lod_jit.disabled_reasons,
            "neg_cache_entries": state.lod_jit.neg_cache.entry_count(),
            "timeout_s": state.lod_jit.timeout.as_secs(),
        },
    });
    {
        body["gpu"] = match crate::gpu_status() {
            Some(s) => {
                metrics::gauge!("abgen_gpu_qualified", "backend" => s.backend)
                    .set(if s.qualified { 1.0 } else { 0.0 });
                serde_json::json!({
                    "backend": s.backend,
                    "qualified": s.qualified,
                    "reason": s.reason,
                })
            }
            None => serde_json::json!({
                "backend": "uninitialized",
                "qualified": false,
                "reason": null,
            }),
        };
    }
    (status, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq_matches_only_identical_bytes() {
        assert!(ct_eq(b"s3cret", b"s3cret"));
        assert!(ct_eq(b"", b""));
        assert!(!ct_eq(b"s3cret", b"s3crXt"));
        assert!(!ct_eq(b"s3cret", b"s3cre"));
        assert!(!ct_eq(b"s3cret", b"nope"));
    }
}

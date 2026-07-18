use super::*;
use crate::bvwebgpu::{BVW_PLATFORM, BVW_PROFILE};

pub(super) struct BvpkTarget {
    pub entity: String,
    pub is_br: bool,
}

pub(super) fn bvpk_target(path: &str) -> Option<BvpkTarget> {
    let segs: Vec<&str> = path.split('/').collect();
    if segs.len() != 3 || segs[0] != BVW_PLATFORM || segs[1] != BVW_PROFILE {
        return None;
    }
    let raw = segs[2].strip_suffix(".br").unwrap_or(segs[2]);
    let entity = raw.strip_suffix(".pack")?;
    if !resolver::is_safe_component(entity) {
        return None;
    }
    Some(BvpkTarget {
        entity: entity.to_string(),
        is_br: segs[2].ends_with(".br"),
    })
}

pub(super) fn bvpk_preflight() -> Response {
    let mut resp = StatusCode::NO_CONTENT.into_response();
    let h = resp.headers_mut();
    h.insert("Access-Control-Allow-Origin", "*".parse().unwrap());
    h.insert(
        "Access-Control-Allow-Methods",
        "GET, HEAD, OPTIONS".parse().unwrap(),
    );
    h.insert(
        "Access-Control-Allow-Headers",
        "X-IPFS, Content-Type".parse().unwrap(),
    );
    resp
}

fn accepts_brotli(headers: &HeaderMap) -> bool {
    headers
        .get("accept-encoding")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .any(|t| t.trim().split(';').next().map(str::trim) == Some("br"))
        })
}

pub(super) async fn bvpk_serve_local(
    state: &AppState,
    path: &str,
    method: &Method,
    headers: &HeaderMap,
) -> Response {
    let segments: Vec<&str> = path.split('/').collect();
    if segments.len() != 3 {
        return serve::not_found();
    }
    if segments[1] != BVW_PROFILE {
        return with_reason(serve::not_found(), "bvwebgpu-unknown-profile");
    }
    let filename = segments[2];
    let raw = filename.strip_suffix(".br").unwrap_or(filename);
    let is_br = filename.ends_with(".br");
    let Some(entity) = raw
        .strip_suffix(".pack")
        .filter(|e| resolver::is_safe_component(e))
    else {
        return serve::not_found();
    };
    if !is_br && accepts_brotli(headers) {
        let br_name = format!("{filename}.br");
        let hit = state
            .serve_lookup(|r| resolver::bvpack_path(r, entity, &br_name))
            .filter(|(p, _)| p.is_file());
        if let Some((exact, from_jit)) = hit {
            state.touch_if_jit(&exact, from_jit);
            let key = format!("{path}.br");
            let mut resp =
                serve::serve_binary(state, &key, &exact, &br_name, true, method, headers).await;
            if resp.status() != StatusCode::NOT_FOUND {
                resp.headers_mut()
                    .insert("Vary", "Accept-Encoding".parse().unwrap());
                return resp;
            }
        }
    }
    let Some((exact, from_jit)) = state.serve_lookup(|r| resolver::bvpack_path(r, entity, filename))
    else {
        return serve::not_found();
    };
    state.touch_if_jit(&exact, from_jit);
    serve::serve_binary(state, path, &exact, raw, is_br, method, headers).await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn bvpk_fallback(
    state: &AppState,
    path: &str,
    target: &BvpkTarget,
    wait: bool,
    method: &Method,
    headers: &HeaderMap,
    local: Response,
) -> Response {
    let Some(proxy) = state.live_proxy.clone() else {
        return local;
    };
    if !crate::clihelp::env_bool("ABGEN_BVWEBGPU", true) {
        return with_reason(local, "bvwebgpu-disabled");
    }
    if target.is_br {
        return with_reason(local, "br-not-built");
    }
    let fail_key = format!("{}:{BVW_PLATFORM}", target.entity);
    if state.jit_fail_cache.get(&fail_key).await.is_some() {
        return with_reason(local, "bvwebgpu-unbuildable");
    }
    let filename = format!("{}.pack", target.entity);
    let probe = resolver::bvpack_path(&state.jit_root, &target.entity, &filename);
    if proxy.space_configured() {
        if let Some(dst) = probe.clone() {
            let space_key = format!("{BVW_PLATFORM}/{BVW_PROFILE}/{filename}");
            let p2 = proxy.clone();
            let _pin = state.jit_cache.pin(&target.entity);
            if let Some(resp) = space_lane_serve(
                state,
                Some("bvpk"),
                dst,
                move || p2.space_get_key(&space_key),
                path,
                path,
                method,
                headers,
            )
            .await
            {
                state.jit_record(&target.entity);
                return resp;
            }
        }
    }
    if wait {
        match jit_build_entity(state, &proxy, &target.entity, BVW_PLATFORM, probe, "bvpk").await {
            JitBuild::Built | JitBuild::Coalesced => {
                state.resolve_cache.invalidate(path).await;
                dispatch_local(state, path, method, headers).await
            }
            JitBuild::Failed => with_reason(local, "bvwebgpu-unbuildable"),
        }
    } else {
        let st = state.clone();
        let entity = target.entity.clone();
        tokio::spawn(async move {
            let probe = resolver::bvpack_path(&st.jit_root, &entity, &format!("{entity}.pack"));
            let _ = jit_build_entity(&st, &proxy, &entity, BVW_PLATFORM, probe, "bvpk").await;
        });
        with_reason(local, "bvwebgpu-cold")
    }
}

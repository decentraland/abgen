//! Prototype routes of the Bevy variants lane:
//! `GET /bevy/manifest/{entity}.json` and `GET /bevy/contents/{variantCid}`.

use super::*;
use crate::bevyvariants::{self, BEVY_PLATFORM};

#[derive(Debug, PartialEq, Eq)]
pub(super) enum BevyTarget {
    Manifest(String),
    Content(String),
}

pub(super) fn bevy_target(path: &str) -> Option<BevyTarget> {
    let segs: Vec<&str> = path.split('/').collect();
    if segs.len() != 3 || segs[0] != BEVY_PLATFORM {
        return None;
    }
    match segs[1] {
        "manifest" => {
            let entity = segs[2].strip_suffix(".json")?;
            bevyvariants::is_id(entity).then(|| BevyTarget::Manifest(entity.to_string()))
        }
        "contents" => {
            bevyvariants::is_id(segs[2]).then(|| BevyTarget::Content(segs[2].to_string()))
        }
        _ => None,
    }
}

fn disk_path(root: &std::path::Path, target: &BevyTarget) -> Option<std::path::PathBuf> {
    match target {
        BevyTarget::Manifest(e) => bevyvariants::manifest_path(root, e),
        BevyTarget::Content(c) => bevyvariants::content_path(root, c),
    }
}

/// Serve from the corpus root, then the JIT root. Manifests are `no-cache` JSON (a newer
/// encoder regenerates them); contents are immutable octet streams with `Range` support.
pub(super) async fn bevy_serve_local(
    state: &AppState,
    path: &str,
    method: &Method,
    headers: &HeaderMap,
) -> Response {
    let Some(target) = bevy_target(path) else {
        return serve::not_found();
    };
    let Some((exact, from_jit)) = state.serve_lookup(|r| disk_path(r, &target)) else {
        return serve::not_found();
    };
    state.touch_if_jit(&exact, from_jit);
    match &target {
        BevyTarget::Manifest(_) => serve::serve_manifest(state, path, &exact, method).await,
        BevyTarget::Content(cid) => {
            let mut resp = serve::serve_binary(state, path, &exact, cid, method, headers).await;
            if resp.status().is_success() {
                resp.headers_mut().insert(
                    "Content-Type",
                    "application/octet-stream".parse().unwrap(),
                );
            }
            resp
        }
    }
}

/// Misses: object-storage read-through for both kinds; manifests additionally build in the
/// background (or synchronously with `?wait=1`), single-flighted and fail-cached per entity
/// like the bvwebgpu lane. Contents are never built on their own: they exist once the
/// manifest naming them does.
#[allow(clippy::too_many_arguments)]
pub(super) async fn bevy_fallback(
    state: &AppState,
    path: &str,
    target: &BevyTarget,
    wait: bool,
    method: &Method,
    headers: &HeaderMap,
    local: Response,
) -> Response {
    let Some(proxy) = state.live_proxy.clone() else {
        return local;
    };
    if !crate::clihelp::env_bool("ABGEN_BEVY_VARIANTS", true) {
        return with_reason(local, "bevy-variants-disabled");
    }
    if proxy.space_configured() {
        if let Some(dst) = disk_path(&state.jit_root, target) {
            let key = match target {
                BevyTarget::Manifest(e) => bevyvariants::manifest_key(e),
                BevyTarget::Content(c) => bevyvariants::content_key(c),
            };
            let p2 = proxy.clone();
            if let Some(resp) = space_lane_serve(
                state,
                Some("bevy"),
                dst,
                move || p2.space_get_key(&key),
                path,
                path,
                method,
                headers,
            )
            .await
            {
                return resp;
            }
        }
    }
    let BevyTarget::Manifest(entity) = target else {
        return local;
    };
    let fail_key = format!("{entity}:{BEVY_PLATFORM}");
    if state.jit_fail_cache.get(&fail_key).await.is_some() {
        return with_reason(local, "bevy-variants-unbuildable");
    }
    let probe = bevyvariants::manifest_path(&state.jit_root, entity);
    if wait {
        match jit_build_entity(state, &proxy, entity, BEVY_PLATFORM, probe, "bevy").await {
            JitBuild::Built | JitBuild::Coalesced => {
                state.resolve_cache.invalidate(path).await;
                dispatch_local(state, path, method, headers).await
            }
            JitBuild::Failed => with_reason(local, "bevy-variants-unbuildable"),
        }
    } else {
        let st = state.clone();
        let entity = entity.clone();
        tokio::spawn(async move {
            let probe = bevyvariants::manifest_path(&st.jit_root, &entity);
            let _ = jit_build_entity(&st, &proxy, &entity, BEVY_PLATFORM, probe, "bevy").await;
        });
        with_reason(local, "bevy-variants-cold")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bevy_target_parses_only_the_two_routes() {
        assert_eq!(
            bevy_target("bevy/manifest/bafkent.json"),
            Some(BevyTarget::Manifest("bafkent".into()))
        );
        assert_eq!(
            bevy_target("bevy/contents/bafkreiabc"),
            Some(BevyTarget::Content("bafkreiabc".into()))
        );
        for bad in [
            "bevy/manifest/bafkent",
            "bevy/manifest/../x.json",
            "bevy/manifest/b64-abc.json",
            "bevy/contents/a.b",
            "bevy/contents",
            "bevy/other/x",
            "bevy/contents/x/y",
            "bvwebgpu/bv4/x.pack",
        ] {
            assert_eq!(bevy_target(bad), None, "{bad}");
        }
    }
}

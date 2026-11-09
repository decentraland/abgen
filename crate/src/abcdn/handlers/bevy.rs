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
            // A `glb-meshopt` variant may have a Brotli sidecar (`{cid}.br`): clients that
            // accept `br` get those bytes as the encoded representation of the same resource.
            let br = bevyvariants::meshopt::br_sidecar(&exact);
            let has_br = br.is_file();
            let send_br = has_br && accepts_br(headers);
            let mut resp = if send_br {
                let key = format!("{path}.br");
                let etag = format!("{cid}.br");
                serve::serve_binary(state, &key, &br, &etag, method, headers).await
            } else {
                serve::serve_binary(state, path, &exact, cid, method, headers).await
            };
            if resp.status().is_success() {
                let h = resp.headers_mut();
                h.insert("Content-Type", "application/octet-stream".parse().unwrap());
                if has_br {
                    h.insert("Vary", "Accept-Encoding".parse().unwrap());
                }
                if send_br {
                    h.insert("Content-Encoding", "br".parse().unwrap());
                }
            }
            resp
        }
    }
}

/// Whether the request's `Accept-Encoding` lists `br` with a non-zero quality.
pub(super) fn accepts_br(headers: &HeaderMap) -> bool {
    headers
        .get_all("accept-encoding")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|item| {
            let mut parts = item.split(';').map(str::trim);
            let coding = parts.next().unwrap_or("");
            let q_zero = parts.any(|p| {
                p.strip_prefix("q=")
                    .is_some_and(|q| q.trim().parse::<f32>().is_ok_and(|q| q <= 0.0))
            });
            coding.eq_ignore_ascii_case("br") && !q_zero
        })
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
            // A content miss for a client that accepts `br` also fetches the variant's Brotli
            // sidecar, so the local serve below can answer encoded. A miss there is normal (KTX2
            // variants and builds without precompression have none). Once the plain file is on
            // disk this path is not taken again, so a sidecar first wanted by a later client is
            // not fetched; the plain bytes are still served.
            if matches!(target, BevyTarget::Content(_)) && accepts_br(headers) {
                let br_dst = bevyvariants::meshopt::br_sidecar(&dst);
                if !br_dst.is_file() {
                    let p3 = proxy.clone();
                    let br_key = format!("{key}.br");
                    space_materialize(Some("bevy-br"), br_dst, move || p3.space_get_key(&br_key))
                        .await;
                }
            }
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

    #[test]
    fn accept_encoding_br_is_parsed() {
        let h = |v: &str| {
            let mut m = HeaderMap::new();
            m.insert("accept-encoding", v.parse().unwrap());
            m
        };
        assert!(accepts_br(&h("gzip, deflate, br")));
        assert!(accepts_br(&h("br;q=1.0, gzip;q=0.8")));
        assert!(accepts_br(&h("BR")));
        assert!(!accepts_br(&h("gzip, deflate")));
        assert!(!accepts_br(&h("br;q=0")));
        assert!(!accepts_br(&h("brotli")));
        assert!(!accepts_br(&HeaderMap::new()));
    }
}

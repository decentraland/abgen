//! Prototype routes of the Bevy variants lane:
//! `GET /bevy/manifest/{entity}.json` and `GET /bevy/{prefix}/{name}` (a variant by its key,
//! the `path` of its manifest entry).

use super::*;
use crate::bevyvariants::{self, BEVY_PLATFORM};

#[derive(Debug, PartialEq, Eq)]
pub(super) enum BevyTarget {
    Manifest(String),
    /// `bevy/{prefix}/{name}`: a variant's bytes. Its `.br` sidecar is chosen by
    /// `Accept-Encoding`, never by path; its `.json` record is never served.
    Variant {
        prefix: String,
        name: String,
    },
}

impl BevyTarget {
    fn key(&self) -> String {
        match self {
            BevyTarget::Manifest(e) => bevyvariants::manifest_key(e),
            BevyTarget::Variant { prefix, name } => bevyvariants::variant_key(prefix, name),
        }
    }
}

pub(super) fn bevy_target(path: &str) -> Option<BevyTarget> {
    let segs: Vec<&str> = path.split('/').collect();
    if segs.len() != 3 || segs[0] != BEVY_PLATFORM {
        return None;
    }
    if segs[1] == "manifest" {
        let entity = segs[2].strip_suffix(".json")?;
        return bevyvariants::is_id(entity).then(|| BevyTarget::Manifest(entity.to_string()));
    }
    (bevyvariants::is_prefix(segs[1]) && bevyvariants::is_name(segs[2])).then(|| {
        BevyTarget::Variant {
            prefix: segs[1].to_string(),
            name: segs[2].to_string(),
        }
    })
}

fn disk_path(root: &std::path::Path, target: &BevyTarget) -> Option<std::path::PathBuf> {
    match target {
        BevyTarget::Manifest(e) => bevyvariants::manifest_path(root, e),
        BevyTarget::Variant { prefix, name } => bevyvariants::variant_path(root, prefix, name),
    }
}

/// A manifest on disk is served only while it was built from the current generations: one
/// that names an older prefix counts as absent, until the offline build writes a current one.
fn manifest_is_current(path: &std::path::Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    let Ok(m) = serde_json::from_slice::<bevyvariants::Manifest>(&bytes) else {
        return false;
    };
    #[cfg(feature = "bevy-variants")]
    {
        m.is_current(&bevyvariants::BuildOptions::from_env().lanes())
    }
    #[cfg(not(feature = "bevy-variants"))]
    {
        m.v == bevyvariants::MANIFEST_VERSION
    }
}

/// Serve from the corpus root, then the JIT root. Manifests are `no-cache` JSON (a newer
/// encoder regenerates them); variants are immutable octet streams with `Range` support.
pub(super) async fn bevy_serve_local(
    state: &AppState,
    path: &str,
    method: &Method,
    headers: &HeaderMap,
) -> Response {
    let Some(target) = bevy_target(path) else {
        return serve::not_found();
    };
    if let BevyTarget::Manifest(_) = &target {
        // The corpus root first, then the JIT root (copies read through from object storage),
        // but only a current copy is served: a stale corpus manifest must not hide a current
        // one read through.
        let mut roots = vec![(state.out_root.clone(), false)];
        if state.roots_distinct {
            roots.push((state.jit_root.clone(), true));
        }
        let candidates: Vec<(std::path::PathBuf, bool)> = roots
            .into_iter()
            .filter_map(|(r, j)| disk_path(&r, &target).map(|p| (p, j)))
            .collect();
        let found = tokio::task::spawn_blocking(move || {
            let mut any = false;
            for (p, j) in candidates {
                if p.is_file() {
                    any = true;
                    if manifest_is_current(&p) {
                        return (Some((p, j)), true);
                    }
                }
            }
            (None, any)
        })
        .await
        .unwrap_or((None, false));
        return match found {
            (Some((exact, from_jit)), _) => {
                state.touch_if_jit(&exact, from_jit);
                serve::serve_manifest(state, path, &exact, method).await
            }
            (None, true) => with_reason(serve::not_found(), "bevy-variants-stale"),
            (None, false) => serve::not_found(),
        };
    }
    let Some((exact, from_jit)) = state.serve_lookup(|r| disk_path(r, &target)) else {
        return serve::not_found();
    };
    state.touch_if_jit(&exact, from_jit);
    match &target {
        // Served above.
        BevyTarget::Manifest(_) => serve::not_found(),
        BevyTarget::Variant { prefix, name } => {
            // A `glb-meshopt` variant may have a Brotli sidecar (`{name}.br`): clients that
            // accept `br` get those bytes as the encoded representation of the same resource.
            let br = bevyvariants::meshopt::br_sidecar(&exact);
            let has_br = br.is_file();
            let send_br = has_br && accepts_br(headers);
            let etag = format!("{prefix}/{name}");
            let mut resp = if send_br {
                // The sidecar is its own LRU entry: keep it as warm as the bytes it encodes.
                state.touch_if_jit(&br, from_jit);
                let key = format!("{path}.br");
                let etag = format!("{etag}.br");
                serve::serve_binary(state, &key, &br, &etag, method, headers).await
            } else {
                serve::serve_binary(state, path, &exact, &etag, method, headers).await
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

/// Misses: object-storage read-through, for manifests and variants alike. Nothing is built on
/// request: the lane is built offline (`abgen-corpus --bevy-variants`), so a manifest or
/// variant that is neither on disk nor in object storage is a 404 and the client loads the
/// original file.
pub(super) async fn bevy_fallback(
    state: &AppState,
    path: &str,
    target: &BevyTarget,
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
            let key = target.key();
            // A variant miss for a client that accepts `br` also fetches the variant's Brotli
            // sidecar, so the local serve below can answer encoded. A miss there is normal (KTX2
            // variants and builds without precompression have none). Once the plain file is on
            // disk this path is not taken again, so a sidecar first wanted by a later client is
            // not fetched; the plain bytes are still served.
            if matches!(target, BevyTarget::Variant { .. }) && accepts_br(headers) {
                let br_dst = bevyvariants::meshopt::br_sidecar(&dst);
                if !br_dst.is_file() {
                    let p3 = proxy.clone();
                    let br_key = format!("{key}.br");
                    if space_materialize(Some("bevy-br"), br_dst.clone(), move || {
                        p3.space_get_key(&br_key)
                    })
                    .await
                    {
                        state.jit_record_bevy_file(&br_dst);
                    }
                }
            }
            let p2 = proxy.clone();
            if let Some(resp) = space_lane_serve(
                state,
                Some("bevy"),
                dst.clone(),
                move || p2.space_get_key(&key),
                path,
                path,
                method,
                headers,
            )
            .await
            {
                state.jit_record_bevy_file(&dst);
                return resp;
            }
        }
    }
    local
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bevy_target_parses_manifests_and_variant_keys() {
        assert_eq!(
            bevy_target("bevy/manifest/bafkent.json"),
            Some(BevyTarget::Manifest("bafkent".into()))
        );
        assert_eq!(
            bevy_target("bevy/t1/bafkreiabc_srgb"),
            Some(BevyTarget::Variant {
                prefix: "t1".into(),
                name: "bafkreiabc_srgb".into()
            })
        );
        assert_eq!(
            bevy_target("bevy/m1/bafkreiabc_0123456789abcdef").map(|t| t.key()),
            Some("bevy/m1/bafkreiabc_0123456789abcdef".to_string())
        );
        assert!(bevy_target("bevy/t1z9/bafkreiabc_linear").is_some());
        for bad in [
            "bevy/manifest/bafkent",
            "bevy/manifest/../x.json",
            "bevy/manifest/b64-abc.json",
            "bevy/t1/bafkreiabc_srgb.br",
            "bevy/t1/bafkreiabc_srgb.json",
            "bevy/contents/bafkreiabc",
            "bevy/t/bafkreiabc",
            "bevy/x1/bafkreiabc",
            "bevy/t1",
            "bevy/other/x",
            "bevy/t1/x/y",
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

    #[test]
    fn stale_or_unreadable_manifests_are_not_current() {
        let dir = std::env::temp_dir().join(format!("abgen-bevy-current-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("m.json");
        std::fs::write(&p, b"not json").unwrap();
        assert!(!manifest_is_current(&p));
        std::fs::write(
            &p,
            serde_json::json!({"v": 1, "entity": "e", "generator": "g", "files": {}}).to_string(),
        )
        .unwrap();
        assert!(!manifest_is_current(&p));
        std::fs::write(
            &p,
            serde_json::json!({"v": 2, "entity": "e", "generator": "g",
                "lanes": {"texture": "t0", "mesh": "m1"}, "files": {}})
            .to_string(),
        )
        .unwrap();
        assert!(!manifest_is_current(&p));
        // A v2 manifest (full entries) is stale even with current lanes: it is rebuilt as v3.
        #[cfg(feature = "bevy-variants")]
        {
            std::fs::write(
                &p,
                serde_json::json!({"v": 2, "entity": "e", "generator": "g",
                    "lanes": bevyvariants::BuildOptions::from_env().lanes(),
                    "files": {"c": [{"path": "bevy/t2/c_srgb", "cid": "x", "kind": "texture",
                        "format": "ktx2-bc7", "size": 1}]}})
                .to_string(),
            )
            .unwrap();
            assert!(!manifest_is_current(&p));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

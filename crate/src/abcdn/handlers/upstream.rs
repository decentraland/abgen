use super::*;
use std::io::Read as _;

const UPSTREAM_TIMEOUT_SECS: u64 = 60;

/// Read-through to a production ab-cdn (`ABGEN_UPSTREAM_AB_CDN`). Runs when
/// the local caches miss, before any JIT build lane: streams the same path
/// from upstream, storing nothing on disk — this server persists only what
/// it built for local entities. Keeps wearables/emotes/LODs working when a
/// client points its whole optimized-assets base URL at this server while
/// only local scene entities are buildable here, without probing each remote
/// entity with a doomed local build first.
pub(super) async fn upstream_fallback(
    state: &AppState,
    path: &str,
    method: &Method,
    _headers: &HeaderMap,
    local: Response,
) -> Response {
    let Some(base) = state.upstream_ab_cdn.clone() else {
        return local;
    };
    if *method != Method::GET && *method != Method::HEAD {
        return local;
    }
    // Local preview-server entities/files can never exist upstream.
    if path.split('/').any(|s| s.starts_with("b64-")) {
        return local;
    }
    if !upstream_eligible(path) {
        return local;
    }
    let neg_key = format!("up:{path}");
    if state.jit_fail_cache.get(&neg_key).await.is_some() {
        return with_reason(local, "upstream-miss");
    }

    let url = format!("{base}/{path}");
    let fetched = tokio::task::spawn_blocking(move || upstream_get(&url))
        .await
        .unwrap_or(None);
    let outcome = if fetched.is_some() { "hit" } else { "miss" };
    metrics::counter!("abgen_upstream_reads_total", "outcome" => outcome).increment(1);

    let Some(bytes) = fetched else {
        state.jit_fail_cache.insert(neg_key, ()).await;
        return with_reason(local, "upstream-miss");
    };

    let content_type = if path.ends_with(".json") {
        "application/json"
    } else {
        "application/octet-stream"
    };
    let len = bytes.len();
    let mut resp = if *method == Method::HEAD {
        StatusCode::OK.into_response()
    } else {
        (StatusCode::OK, bytes).into_response()
    };
    let h = resp.headers_mut();
    h.insert("Content-Type", content_type.parse().unwrap());
    h.insert("Content-Length", len.to_string().parse().unwrap());
    h.insert("Cache-Control", "public, max-age=600".parse().unwrap());
    h.insert("Access-Control-Allow-Origin", "*".parse().unwrap());
    resp
}

/// True for ab-cdn request shapes the delivery surface serves; anything else
/// stays a local 404 rather than being forwarded upstream.
pub(super) fn upstream_eligible(path: &str) -> bool {
    upstream_dst(path).is_some()
}

fn upstream_dst(path: &str) -> Option<()> {
    let root = std::path::Path::new("");
    let segs: Vec<&str> = path.split('/').collect();
    match segs.as_slice() {
        ["manifest", name] => {
            let stem = name.strip_suffix(".json")?;
            resolver::manifest_path(root, stem).map(|_| ())
        }
        ["LOD", level, filename] => resolver::lod_path(root, level, filename).map(|_| ()),
        ["lods-unity", "manifests", filename] => {
            resolver::iss_manifest_path(root, filename).map(|_| ())
        }
        [ver, entity, file] if *ver != "manifest" && *ver != "LOD" => {
            if !resolver::is_safe_component(entity) || !resolver::is_safe_component(file) {
                return None;
            }
            let raw = file.strip_suffix(".br").unwrap_or(file);
            if !is_bundle_name(raw) {
                return None;
            }
            Some(())
        }
        [ver, file] if *ver != "manifest" && *ver != "dcl" => {
            if !resolver::is_safe_component(ver) || !resolver::is_safe_component(file) {
                return None;
            }
            let raw = file.strip_suffix(".br").unwrap_or(file);
            if !is_bundle_name(raw) {
                return None;
            }
            Some(())
        }
        _ => None,
    }
}

fn upstream_get(url: &str) -> Option<Vec<u8>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(UPSTREAM_TIMEOUT_SECS)))
        .build()
        .into();
    match agent.get(url).call() {
        Ok(resp) => {
            let mut buf: Vec<u8> = Vec::new();
            resp.into_body().into_reader().read_to_end(&mut buf).ok()?;
            (!buf.is_empty()).then_some(buf)
        }
        Err(ureq::Error::StatusCode(code)) => {
            if code != 404 {
                tracing::warn!(url = %url, status = code, "upstream ab-cdn fetch failed");
            }
            None
        }
        Err(e) => {
            tracing::warn!(url = %url, error = %e, "upstream ab-cdn fetch failed");
            None
        }
    }
}

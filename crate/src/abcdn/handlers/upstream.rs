use super::*;
use std::io::Read as _;

const UPSTREAM_TIMEOUT_SECS: u64 = 60;

/// Read-through to a production ab-cdn (`ABGEN_UPSTREAM_AB_CDN`). Runs after
/// every local and JIT lane has 404'd: fetches the same path upstream,
/// materializes the bytes into the JIT cache layout, and re-dispatches. This
/// keeps wearables/emotes/LODs working when a client points its whole
/// optimized-assets base URL at this server while only local scene entities
/// are buildable here.
pub(super) async fn upstream_fallback(
    state: &AppState,
    path: &str,
    method: &Method,
    headers: &HeaderMap,
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
    let Some(dst) = upstream_dst(state, path) else {
        return local;
    };
    let neg_key = format!("up:{path}");
    if state.jit_fail_cache.get(&neg_key).await.is_some() {
        return with_reason(local, "upstream-miss");
    }

    let jit_key = state.jit_key_of(&dst);
    let _pin = jit_key.as_deref().and_then(|k| state.jit_cache.pin(k));
    let url = format!("{base}/{path}");
    let dst2 = dst.clone();
    let materialized = tokio::task::spawn_blocking(move || match upstream_get(&url) {
        Some(bytes) => write_materialized(&dst2, &bytes),
        None => false,
    })
    .await
    .unwrap_or(false);
    let outcome = if materialized { "hit" } else { "miss" };
    metrics::counter!("abgen_upstream_reads_total", "outcome" => outcome).increment(1);
    if !materialized {
        state.jit_fail_cache.insert(neg_key, ()).await;
        return with_reason(local, "upstream-miss");
    }

    if let Some(k) = &jit_key {
        let entry = state.jit_root.join(k);
        if entry.is_dir() {
            state.jit_record(k);
        } else {
            state.jit_record_file(k, &entry);
        }
    }
    state.resolve_cache.invalidate(path).await;
    let resp = dispatch_local(state, path, method, headers).await;
    if resp.status() == StatusCode::NOT_FOUND {
        return with_reason(local, "upstream-miss");
    }
    resp
}

/// Maps an ab-cdn request path to the JIT-cache location where an upstream
/// copy should be materialized. Only paths the delivery surface actually
/// serves are eligible; anything else stays a local 404.
pub(super) fn upstream_dst(state: &AppState, path: &str) -> Option<std::path::PathBuf> {
    let segs: Vec<&str> = path.split('/').collect();
    match segs.as_slice() {
        ["manifest", name] => {
            let stem = name.strip_suffix(".json")?;
            resolver::manifest_path(&state.jit_root, stem)
        }
        ["LOD", level, filename] => resolver::lod_path(&state.jit_root, level, filename),
        ["lods-unity", "manifests", filename] => {
            resolver::iss_manifest_path(&state.jit_root, filename)
        }
        [ver, entity, file] if *ver != "manifest" && *ver != "LOD" => {
            if !resolver::is_safe_component(entity) || !resolver::is_safe_component(file) {
                return None;
            }
            let raw = file.strip_suffix(".br").unwrap_or(file);
            if !is_bundle_name(raw) {
                return None;
            }
            let platform = resolver::platform_of(raw);
            Some(
                state
                    .jit_root
                    .join(&*crate::naming::fs_safe_component(entity))
                    .join(platform)
                    .join(&*crate::naming::fs_safe_component(file)),
            )
        }
        [ver, file] if *ver != "manifest" && *ver != "dcl" => {
            if !resolver::is_safe_component(ver) || !resolver::is_safe_component(file) {
                return None;
            }
            let raw = file.strip_suffix(".br").unwrap_or(file);
            if !is_bundle_name(raw) {
                return None;
            }
            Some(state.jit_root.join(&*crate::naming::fs_safe_component(file)))
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

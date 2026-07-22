use super::*;

/// Dev-mode content revalidation (`ABGEN_JIT_CONTENT_DIGEST`). Runs on
/// manifest requests before local dispatch so content servers that key hashes
/// by file path (the sdk-commands preview server) cannot keep serving stale
/// conversions after a file edit. The heavy lifting lives in
/// [`crate::live::Proxy::refresh_entity_content`]; this wrapper debounces per
/// entity and clears the server-side caches when content actually changed.
pub(super) async fn revalidate_if_stale(state: &AppState, path: &str) {
    if !state.jit_content_digest {
        return;
    }
    let Some(proxy) = state.live_proxy.clone() else {
        return;
    };
    let Some(JitTarget::Manifest { entity, .. }) = jit_target(path) else {
        return;
    };
    if state.revalidate_recent.get(&entity).await.is_some() {
        return;
    }
    state.revalidate_recent.insert(entity.clone(), ()).await;

    let jit_root = state.jit_root.clone();
    let ent = entity.clone();
    let refreshed =
        tokio::task::spawn_blocking(move || proxy.refresh_entity_content(&ent, &jit_root)).await;
    match refreshed {
        Ok(Ok(changed)) if !changed.is_empty() => {
            tracing::info!(
                entity = %entity,
                changed = changed.len(),
                "revalidate: content changed under unchanged hashes — stale conversions pruned"
            );
            metrics::counter!("abgen_revalidate_total", "outcome" => "changed").increment(1);
            state.resolve_cache.invalidate_all();
            state.jit_fail_cache.invalidate_all();
            if let Some(reg) = &state.contents_registry {
                reg.manifests.invalidate(&entity).await;
            }
        }
        Ok(Ok(_)) => {
            metrics::counter!("abgen_revalidate_total", "outcome" => "fresh").increment(1);
        }
        Ok(Err(e)) => {
            metrics::counter!("abgen_revalidate_total", "outcome" => "error").increment(1);
            tracing::debug!(
                entity = %entity,
                error = %format!("{e:#}"),
                "revalidate: skipped (entity not fetchable from the content server)"
            );
        }
        Err(e) => tracing::error!(error = %e, "revalidate worker panicked"),
    }
}

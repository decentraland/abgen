use super::tests::{body_bytes, lane_temp_dir, mk_lane_state, mk_stub_proxy_catalyst, reason_of};
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use std::collections::HashMap;

async fn request(
    state: &super::super::state::AppState,
    method: Method,
    path_and_query: &str,
    headers: HeaderMap,
) -> Response {
    super::dispatch(
        State(state.clone()),
        method,
        headers,
        format!("/{path_and_query}").parse().unwrap(),
    )
    .await
}

async fn get(state: &super::super::state::AppState, path: &str) -> Response {
    request(state, Method::GET, path, HeaderMap::new()).await
}

fn write_warm_pack(dir: &std::path::Path, entity: &str) -> Vec<u8> {
    let entries = vec![crate::bvwebgpu::pack::EntrySpec {
        path: "main.crdt".to_string(),
        cid: "bafkfile".to_string(),
        kind: "raw",
    }];
    let blobs: HashMap<String, Vec<u8>> =
        HashMap::from([("bafkfile".to_string(), b"CRDTBYTES".to_vec())]);
    let (pack, _) = crate::bvwebgpu::pack::build_pack(entity, &entries, &blobs, u64::MAX).unwrap();
    let pdir = dir.join(entity).join("bvwebgpu");
    std::fs::create_dir_all(&pdir).unwrap();
    std::fs::write(pdir.join(format!("{entity}_bv1.pack")), &pack).unwrap();
    pack
}

fn entity_json(id: &str, files: &[(&str, &str)]) -> Vec<u8> {
    let content: Vec<serde_json::Value> = files
        .iter()
        .map(|(f, h)| serde_json::json!({"file": f, "hash": h}))
        .collect();
    serde_json::to_vec(&serde_json::json!({
        "id": id,
        "type": "scene",
        "pointers": ["0,0"],
        "content": content,
        "metadata": {}
    }))
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warm_pack_serves_with_etag_ranges_and_cors() {
    let dir = lane_temp_dir("bvpk-warm");
    let entity = "bafkbvwarm";
    let pack = write_warm_pack(&dir, entity);
    let state = mk_lane_state(&dir, None);

    let path = format!("bvwebgpu/bv1/{entity}.pack");
    let resp = get(&state, &path).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("ETag").and_then(|v| v.to_str().ok()),
        Some(format!("\"{entity}.pack\"").as_str())
    );
    assert_eq!(
        resp.headers()
            .get("Content-Type")
            .and_then(|v| v.to_str().ok()),
        Some("application/octet-stream")
    );
    assert_eq!(
        resp.headers()
            .get("Cache-Control")
            .and_then(|v| v.to_str().ok()),
        Some("public,max-age=31536000,immutable")
    );
    assert_eq!(
        resp.headers()
            .get("Access-Control-Allow-Origin")
            .and_then(|v| v.to_str().ok()),
        Some("*")
    );
    assert_eq!(body_bytes(resp).await, pack);

    let mut inm = HeaderMap::new();
    inm.insert("if-none-match", format!("\"{entity}.pack\"").parse().unwrap());
    let cached = request(&state, Method::GET, &path, inm).await;
    assert_eq!(cached.status(), StatusCode::NOT_MODIFIED);

    let mut range = HeaderMap::new();
    range.insert("range", "bytes=0-7".parse().unwrap());
    let part = request(&state, Method::GET, &path, range).await;
    assert_eq!(part.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(body_bytes(part).await, &pack[..8]);

    let mut bad = HeaderMap::new();
    bad.insert("range", format!("bytes={}-", pack.len()).parse().unwrap());
    let unsat = request(&state, Method::GET, &path, bad).await;
    assert_eq!(unsat.status(), StatusCode::RANGE_NOT_SATISFIABLE);

    let head = request(&state, Method::HEAD, &path, HeaderMap::new()).await;
    assert_eq!(head.status(), StatusCode::OK);
    assert!(body_bytes(head).await.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn brotli_negotiation_serves_sidecar_on_the_plain_url() {
    let dir = lane_temp_dir("bvpk-br");
    let entity = "bafkbvbr";
    let pack = write_warm_pack(&dir, entity);
    let br = crate::compress::brotli(&pack).unwrap();
    std::fs::write(
        dir.join(entity)
            .join("bvwebgpu")
            .join(format!("{entity}_bv1.pack.br")),
        &br,
    )
    .unwrap();
    let state = mk_lane_state(&dir, None);
    let path = format!("bvwebgpu/bv1/{entity}.pack");

    let mut ae = HeaderMap::new();
    ae.insert("accept-encoding", "gzip, br".parse().unwrap());
    let resp = request(&state, Method::GET, &path, ae).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("Content-Encoding")
            .and_then(|v| v.to_str().ok()),
        Some("br")
    );
    assert_eq!(
        resp.headers().get("Vary").and_then(|v| v.to_str().ok()),
        Some("Accept-Encoding")
    );
    assert_eq!(
        resp.headers().get("ETag").and_then(|v| v.to_str().ok()),
        Some(format!("\"{entity}.pack.br\"").as_str())
    );
    assert_eq!(body_bytes(resp).await, br);

    let plain = get(&state, &path).await;
    assert_eq!(plain.status(), StatusCode::OK);
    assert!(plain.headers().get("Content-Encoding").is_none());
    assert_eq!(body_bytes(plain).await, pack);

    let explicit = get(&state, &format!("{path}.br")).await;
    assert_eq!(explicit.status(), StatusCode::OK);
    assert_eq!(
        explicit
            .headers()
            .get("Content-Encoding")
            .and_then(|v| v.to_str().ok()),
        Some("br")
    );
    assert_eq!(body_bytes(explicit).await, br);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_and_error_reasons() {
    let dir = lane_temp_dir("bvpk-reasons");
    let (space_host, _seen) = crate::live::stub::serve(vec![]);
    let proxy = mk_stub_proxy_catalyst(&space_host, "http://127.0.0.1:9", false, "bvpk-reasons");
    let state = mk_lane_state(&dir, Some(proxy));

    let pre = request(
        &state,
        Method::OPTIONS,
        "bvwebgpu/bv1/whatever.pack",
        HeaderMap::new(),
    )
    .await;
    assert_eq!(pre.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        pre.headers()
            .get("Access-Control-Allow-Headers")
            .and_then(|v| v.to_str().ok()),
        Some("X-IPFS, Content-Type")
    );
    assert_eq!(
        pre.headers()
            .get("Access-Control-Allow-Methods")
            .and_then(|v| v.to_str().ok()),
        Some("GET, HEAD, OPTIONS")
    );

    let bad_profile = get(&state, "bvwebgpu/bv9/bafkx.pack").await;
    assert_eq!(bad_profile.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        reason_of(&bad_profile).as_deref(),
        Some("bvwebgpu-unknown-profile")
    );

    let traversal = get(&state, "bvwebgpu/bv1/...pack").await;
    assert_eq!(traversal.status(), StatusCode::NOT_FOUND);
    assert_eq!(reason_of(&traversal), None);

    let not_pack = get(&state, "bvwebgpu/bv1/bafkx.zip").await;
    assert_eq!(not_pack.status(), StatusCode::NOT_FOUND);
    assert_eq!(reason_of(&not_pack), None);

    let br_cold = get(&state, "bvwebgpu/bv1/bafkx.pack.br").await;
    assert_eq!(br_cold.status(), StatusCode::NOT_FOUND);
    assert_eq!(reason_of(&br_cold).as_deref(), Some("br-not-built"));

    std::env::set_var("ABGEN_BVWEBGPU", "0");
    let disabled = get(&state, "bvwebgpu/bv1/bafkx.pack").await;
    std::env::remove_var("ABGEN_BVWEBGPU");
    assert_eq!(disabled.status(), StatusCode::NOT_FOUND);
    assert_eq!(reason_of(&disabled).as_deref(), Some("bvwebgpu-disabled"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_jit_builds_detached_and_wait_coalesces() {
    let dir = lane_temp_dir("bvpk-jit");
    let entity = "bafkbvjit";
    let ent_json = entity_json(
        entity,
        &[
            ("Assets/Readme.TXT", "bafkraw1"),
            ("movie.mp4", "bafkvid1"),
        ],
    );
    let (cat_host, cat_seen) = crate::live::stub::serve(vec![
        (format!("/contents/{entity}"), 200, ent_json),
        ("/contents/bafkraw1".to_string(), 200, b"RAWBYTES".to_vec()),
        ("/contents/bafkvid1".to_string(), 200, b"VIDEO".to_vec()),
    ]);
    let (space_host, _sseen) = crate::live::stub::serve(vec![]);
    let proxy = mk_stub_proxy_catalyst(
        &space_host,
        &format!("http://{cat_host}"),
        false,
        "bvpk-jit",
    );
    let state = mk_lane_state(&dir, Some(proxy));
    let path = format!("bvwebgpu/bv1/{entity}.pack");

    let cold = get(&state, &path).await;
    assert_eq!(cold.status(), StatusCode::NOT_FOUND);
    assert_eq!(reason_of(&cold).as_deref(), Some("bvwebgpu-cold"));

    let probe = dir
        .join(entity)
        .join("bvwebgpu")
        .join(format!("{entity}_bv1.pack"));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !probe.is_file() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(probe.is_file(), "detached cold build never materialized");

    let wait_path = format!("{path}?wait=1");
    let (a, b) = tokio::join!(get(&state, &wait_path), get(&state, &wait_path));
    assert_eq!(a.status(), StatusCode::OK);
    assert_eq!(b.status(), StatusCode::OK);

    let bytes = body_bytes(get(&state, &path).await).await;
    let parsed = crate::bvwebgpu::pack::parse_pack(&bytes).unwrap();
    assert_eq!(parsed.index.entity, entity);
    let paths: Vec<&str> = parsed
        .index
        .files
        .iter()
        .map(|e| e.path.as_str())
        .collect();
    assert_eq!(paths, vec!["assets/readme.txt"]);
    assert_eq!(
        crate::bvwebgpu::pack::entry_slice(&bytes, &parsed, &parsed.index.files[0]),
        b"RAWBYTES"
    );

    let log = cat_seen.lock().unwrap().clone();
    let ent_fetches = log
        .iter()
        .filter(|l| *l == &format!("GET /contents/{entity}"))
        .count();
    assert_eq!(ent_fetches, 1, "{log:?}");
    assert!(!log.contains(&"GET /contents/bafkvid1".to_string()), "{log:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unbuildable_entity_is_negative_cached() {
    let dir = lane_temp_dir("bvpk-fail");
    let entity = "bafkbvfail";
    let (cat_host, _cs) = crate::live::stub::serve(vec![]);
    let (space_host, _ss) = crate::live::stub::serve(vec![]);
    let proxy = mk_stub_proxy_catalyst(
        &space_host,
        &format!("http://{cat_host}"),
        false,
        "bvpk-fail",
    );
    let state = mk_lane_state(&dir, Some(proxy));
    let path = format!("bvwebgpu/bv1/{entity}.pack?wait=1");

    let first = get(&state, &path).await;
    assert_eq!(first.status(), StatusCode::NOT_FOUND);
    assert_eq!(reason_of(&first).as_deref(), Some("bvwebgpu-unbuildable"));

    let second = get(&state, &path).await;
    assert_eq!(second.status(), StatusCode::NOT_FOUND);
    assert_eq!(reason_of(&second).as_deref(), Some("bvwebgpu-unbuildable"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn space_read_through_materializes_the_pack() {
    let dir = lane_temp_dir("bvpk-space");
    let entity = "bafkbvspace";
    let staging = lane_temp_dir("bvpk-space-staging");
    let pack = write_warm_pack(&staging, entity);
    let (space_host, sseen) = crate::live::stub::serve(vec![(
        format!("/bvwebgpu/bv1/{entity}.pack"),
        200,
        pack.clone(),
    )]);
    let proxy = mk_stub_proxy_catalyst(&space_host, "http://127.0.0.1:9", false, "bvpk-space");
    let state = mk_lane_state(&dir, Some(proxy));
    let path = format!("bvwebgpu/bv1/{entity}.pack");

    let resp = get(&state, &path).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_bytes(resp).await, pack);
    assert!(dir
        .join(entity)
        .join("bvwebgpu")
        .join(format!("{entity}_bv1.pack"))
        .is_file());
    assert!(sseen
        .lock()
        .unwrap()
        .contains(&format!("GET /bvwebgpu/bv1/{entity}.pack")));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&staging);
}

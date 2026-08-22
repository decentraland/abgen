use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use crate::builder::{build_bundle, BuildOpts};
use crate::export::{HostInfo, Input, Kind, Sink};
use crate::hashes::sha256_hex;
use crate::naming;
use crate::validate::{validate_bundle_parsed, Severity, ValidateCtx};

/// Records every event a file's conversion emits, in call order, instead of
/// forwarding it straight to the real sink. Lets `convert` run several
/// files' `convert_one` concurrently while still flushing to the real sink
/// in input order — the on-wire event stream stays identical to the serial
/// loop's, byte for byte.
#[derive(Default)]
struct BufferedSink {
    events: Mutex<Vec<(Kind, Vec<u8>)>>,
}

impl BufferedSink {
    fn into_events(self) -> Vec<(Kind, Vec<u8>)> {
        self.events.into_inner().unwrap_or_else(|e| e.into_inner())
    }
}

impl Sink for BufferedSink {
    fn emit(&self, kind: Kind, bytes: &[u8]) {
        let mut g = self.events.lock().unwrap_or_else(|e| e.into_inner());
        g.push((kind, bytes.to_vec()));
    }
}

/// How many files `convert` converts at once. Delegates to
/// [`crate::clihelp::default_file_concurrency`], the same runtime-scaled
/// default (and `ABGEN_FILE_CONCURRENCY` override) `live::corpus_file_jobs`
/// uses for the JIT path, so both hosts agree on how many cores/how much
/// memory justifies running that many files at once.
fn file_concurrency() -> usize {
    crate::clihelp::default_file_concurrency()
}

fn ext_of(name: &str) -> String {
    match name.rsplit('.').next() {
        Some(e) if e.len() < name.len() => format!(".{}", e.to_lowercase()),
        _ => String::new(),
    }
}

fn detect_entity_type(files: &[(String, Vec<u8>)]) -> &'static str {
    if files.iter().any(|(n, _)| {
        n.eq_ignore_ascii_case("scene.json") || n.to_lowercase().ends_with("/scene.json")
    }) {
        return "scene";
    }
    if files
        .iter()
        .any(|(n, _)| n.to_lowercase().ends_with("_emote.glb"))
    {
        return "emote";
    }
    "wearable"
}

fn target_of<'a>(platform: &'a str, sink: &dyn Sink) -> &'a str {
    match platform {
        "windows" | "mac" | "linux" | "webgl" => platform,
        other => {
            sink.emit_json(serde_json::json!({
                "ev": "note",
                "msg": format!("unknown platform {other:?}, using windows"),
            }));
            "windows"
        }
    }
}

fn entity_type_of(input: &Input) -> String {
    if input.entity_type.is_empty() {
        detect_entity_type(&input.files).to_string()
    } else {
        input.entity_type.clone()
    }
}

fn content_maps(
    files: &[(String, Vec<u8>)],
) -> (HashMap<String, String>, HashMap<String, &Vec<u8>>) {
    let mut content_by_file: HashMap<String, String> = HashMap::new();
    let mut bytes_by_hash: HashMap<String, &Vec<u8>> = HashMap::new();
    for (name, data) in files {
        let hash = sha256_hex(data);
        content_by_file.insert(name.to_lowercase(), hash.clone());
        bytes_by_hash.insert(hash, data);
    }
    (content_by_file, bytes_by_hash)
}

fn entity_hash_of(content_by_file: &HashMap<String, String>) -> String {
    let mut ids: Vec<String> = content_by_file
        .iter()
        .map(|(f, h)| format!("{f}:{h}"))
        .collect();
    ids.sort();
    sha256_hex(ids.join("\n").as_bytes())
}

fn glbs_of(files: &[(String, Vec<u8>)]) -> Vec<&(String, Vec<u8>)> {
    files
        .iter()
        .filter(|(n, _)| naming::GLTF_EXTENSIONS.contains(&ext_of(n).as_str()))
        .collect()
}

struct EntityCtx<'a> {
    target: &'a str,
    entity_type: &'a str,
    magenta: bool,
    content_by_file: &'a HashMap<String, String>,
    bytes_by_hash: &'a HashMap<String, &'a Vec<u8>>,
}

fn convert_one(
    ctx: &EntityCtx,
    name: &str,
    data: &[u8],
    emit_plan: bool,
    sink: &dyn Sink,
) -> Option<String> {
    let ext = ext_of(name);
    let hash = match ctx.content_by_file.get(&name.to_lowercase()) {
        Some(h) => h.clone(),
        None => {
            sink.emit_json(serde_json::json!({
                "ev": "file-error",
                "file": name,
                "error": "file missing from the content table",
            }));
            return None;
        }
    };

    if emit_plan {
        if let Ok(scene) = crate::gltf::parse_classify(data, &ext, None) {
            sink.emit_json(serde_json::json!({
                "ev": "plan",
                "file": name,
                "nodes": scene.nodes.len(),
                "materials": scene.materials.len(),
                "images": scene.images.len(),
                "skins": scene.skins.len(),
            }));
        }
    }

    let digest = match naming::deps_digest_for_glb(data, name, ctx.content_by_file, ctx.magenta) {
        Ok(d) => d,
        Err(e) => {
            sink.emit_json(serde_json::json!({
                "ev": "file-error",
                "file": name,
                "error": format!("dependency resolution: {e:#}"),
            }));
            return None;
        }
    };
    let bundle_name = match naming::canonical_filename(&hash, &ext, ctx.target, Some(&digest)) {
        Ok(n) => n,
        Err(e) => {
            sink.emit_json(serde_json::json!({
                "ev": "file-error",
                "file": name,
                "error": format!("{e:#}"),
            }));
            return None;
        }
    };

    sink.emit_json(serde_json::json!({
        "ev": "file-start",
        "file": name,
        "bytes": data.len(),
        "bundle": bundle_name,
    }));

    let resolve_fn = |uri: &str| -> Option<Vec<u8>> {
        let key = naming::resolve_uri_to_content_file(uri, name).ok()?;
        let h = ctx.content_by_file.get(&key.to_lowercase())?;
        ctx.bytes_by_hash.get(h).map(|b| (*b).clone())
    };
    let resolve_hash_fn = |uri: &str| -> Option<String> {
        let key = naming::resolve_uri_to_content_file(uri, name).ok()?;
        ctx.content_by_file.get(&key.to_lowercase()).cloned()
    };

    let opts = BuildOpts {
        source_file: Some(name),
        entity_type: Some(ctx.entity_type),
        resolve: Some(&resolve_fn),
        resolve_hash: Some(&resolve_hash_fn),
        magenta_missing: ctx.magenta,
        real_textures: true,
        ..Default::default()
    };

    match build_bundle(data, &bundle_name, &hash, &opts) {
        Ok(artifact) => {
            let findings =
                validate_bundle_parsed(&artifact.bundle, &bundle_name, &ValidateCtx::single_file());
            let fjson: Vec<serde_json::Value> = findings
                .iter()
                .map(|f| {
                    serde_json::json!({
                        "severity": match f.severity { Severity::Error => "error", Severity::Warn => "warn" },
                        "code": f.code,
                        "msg": f.msg,
                    })
                })
                .collect();
            sink.emit_json(serde_json::json!({
                "ev": "validate",
                "bundle": bundle_name,
                "findings": fjson,
            }));
            sink.emit_output(&bundle_name, &artifact.data);
            sink.emit_json(serde_json::json!({
                "ev": "file-done",
                "file": name,
                "bundle": bundle_name,
                "bytes": artifact.data.len(),
            }));
            Some(bundle_name)
        }
        Err(e) => {
            sink.emit_json(serde_json::json!({
                "ev": "file-error",
                "file": name,
                "error": format!("{e:#}"),
            }));
            None
        }
    }
}

pub fn convert(input: Input, sink: &dyn Sink, host: HostInfo) -> crate::Result<()> {
    convert_with_jobs(input, sink, host, file_concurrency())
}

/// `convert`'s body, with the file-level worker count passed in explicitly
/// instead of read from [`file_concurrency`] — lets tests exercise the
/// serial and parallel dispatch paths deterministically in one process,
/// where `file_concurrency`'s cached, env/runtime-derived value can't be
/// changed after the first call.
fn convert_with_jobs(
    input: Input,
    sink: &dyn Sink,
    host: HostInfo,
    jobs: usize,
) -> crate::Result<()> {
    let target = target_of(&input.platform, sink);
    let entity_type = entity_type_of(&input);
    let (content_by_file, bytes_by_hash) = content_maps(&input.files);
    let entity_hash = entity_hash_of(&content_by_file);
    let glbs = glbs_of(&input.files);

    sink.emit_json(serde_json::json!({
        "ev": "entity",
        "entityType": entity_type,
        "entityHash": entity_hash,
        "platform": target,
        "files": input.files.len(),
        "models": glbs.len(),
    }));

    if glbs.is_empty() {
        sink.emit_error("no .glb/.gltf files in the upload");
        return Ok(());
    }

    let ctx = EntityCtx {
        target,
        entity_type: &entity_type,
        magenta: input.magenta,
        content_by_file: &content_by_file,
        bytes_by_hash: &bytes_by_hash,
    };

    let mut built: Vec<String> = Vec::new();
    let mut failures = 0usize;

    let jobs = jobs.clamp(1, glbs.len().max(1));
    if jobs <= 1 {
        for (name, data) in glbs {
            match convert_one(&ctx, name, data, true, sink) {
                Some(bundle) => built.push(bundle),
                None => failures += 1,
            }
        }
    } else {
        // Bounded file-level parallelism: each of `jobs` worker threads
        // pulls the next unclaimed file index and converts it into its own
        // BufferedSink, so files run concurrently. Results are collected
        // into per-index slots and flushed to `sink` in original input
        // order afterward, so the event stream and `built`/`failures`
        // bookkeeping match the serial loop exactly.
        let next = AtomicUsize::new(0);
        let slots: Vec<Mutex<Option<(Option<String>, Vec<(Kind, Vec<u8>)>)>>> =
            (0..glbs.len()).map(|_| Mutex::new(None)).collect();
        std::thread::scope(|s| {
            for _ in 0..jobs {
                s.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((name, data)) = glbs.get(i) else {
                        break;
                    };
                    let buf = BufferedSink::default();
                    let bundle = convert_one(&ctx, name, data, true, &buf);
                    *slots[i].lock().unwrap_or_else(|e| e.into_inner()) =
                        Some((bundle, buf.into_events()));
                });
            }
        });
        for slot in slots {
            let (bundle, events) = slot
                .into_inner()
                .unwrap_or_else(|e| e.into_inner())
                .expect("every file index is claimed by exactly one worker");
            for (kind, bytes) in events {
                sink.emit(kind, &bytes);
            }
            match bundle {
                Some(b) => built.push(b),
                None => failures += 1,
            }
        }
    }

    if input.lod {
        if target == "webgl" {
            sink.emit_json(serde_json::json!({
                "ev": "note",
                "msg": "LOD skipped: webgl has no LOD lane (windows/mac/linux only)",
            }));
        } else {
            let (base, parcels) = scene_parcels(&input.files);
            let job = LodJob {
                sid: &entity_hash,
                target,
                entity_type: &entity_type,
                files: &input.files,
                content_by_file: &content_by_file,
                bytes_by_hash: &bytes_by_hash,
                base,
                parcels: &parcels,
                crop: input.crop,
                tri_cap: input.tri_cap,
            };
            if let Err(e) = bake_lod(&job, sink) {
                sink.emit_json(serde_json::json!({
                    "ev": "file-error",
                    "file": "LOD",
                    "error": format!("{e:#}"),
                }));
            }
        }
    }

    built.sort();
    built.dedup();
    let mut files_field = built.clone();
    files_field.push("dcl".to_string());
    let manifest = serde_json::json!({
        "version": host.manifest_version,
        "files": files_field,
        "exitCode": if failures == 0 { 0 } else { 12 },
        "contentServerUrl": host.content_server_url,
    });
    sink.emit(Kind::Manifest, manifest.to_string().as_bytes());
    Ok(())
}

pub fn scan(input: Input, sink: &dyn Sink) -> crate::Result<()> {
    let target = target_of(&input.platform, sink);
    let entity_type = entity_type_of(&input);
    let (content_by_file, _) = content_maps(&input.files);
    let entity_hash = entity_hash_of(&content_by_file);
    let glbs = glbs_of(&input.files);

    sink.emit_json(serde_json::json!({
        "ev": "entity",
        "entityType": entity_type,
        "entityHash": entity_hash,
        "platform": target,
        "files": input.files.len(),
        "models": glbs.len(),
    }));

    if glbs.is_empty() {
        sink.emit_error("no .glb/.gltf files in the upload");
        return Ok(());
    }

    let orig_by_lower: HashMap<String, &str> = input
        .files
        .iter()
        .map(|(n, _)| (n.to_lowercase(), n.as_str()))
        .collect();

    for (name, data) in glbs {
        let ext = ext_of(name);
        if let Ok(scene) = crate::gltf::parse_classify(data, &ext, None) {
            sink.emit_json(serde_json::json!({
                "ev": "plan",
                "file": name,
                "nodes": scene.nodes.len(),
                "materials": scene.materials.len(),
                "images": scene.images.len(),
                "skins": scene.skins.len(),
            }));
        }
        let mut deps: Vec<String> = naming::parse_gltf_dep_refs(data, &ext)
            .map(|uris| {
                uris.iter()
                    .filter_map(|u| naming::resolve_uri_to_content_file(u, name).ok())
                    .filter_map(|k| orig_by_lower.get(&k.to_lowercase()).map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        deps.sort();
        deps.dedup();
        sink.emit_json(serde_json::json!({
            "ev": "deps",
            "file": name,
            "deps": deps,
        }));
    }
    Ok(())
}

pub fn convert_only(input: Input, sink: &dyn Sink) -> crate::Result<()> {
    let target = target_of(&input.platform, sink);
    let entity_type = entity_type_of(&input);
    let (own_by_file, bytes_by_hash) = content_maps(&input.files);
    let content_by_file: HashMap<String, String> = match &input.content_table {
        Some(t) => t
            .iter()
            .map(|(n, h)| (n.to_lowercase(), h.clone()))
            .collect(),
        None => own_by_file,
    };
    let only = input.only_glb.clone().unwrap_or_default();

    let ctx = EntityCtx {
        target,
        entity_type: &entity_type,
        magenta: input.magenta,
        content_by_file: &content_by_file,
        bytes_by_hash: &bytes_by_hash,
    };
    match input.files.iter().find(|(n, _)| *n == only) {
        Some((name, data)) => {
            convert_one(&ctx, name, data, false, sink);
        }
        None => sink.emit_json(serde_json::json!({
            "ev": "file-error",
            "file": only,
            "error": "only_glb not present in the job files",
        })),
    }
    Ok(())
}

pub fn lod_only(input: Input, sink: &dyn Sink) -> crate::Result<()> {
    let target = target_of(&input.platform, sink);
    if target == "webgl" {
        sink.emit_json(serde_json::json!({
            "ev": "note",
            "msg": "LOD skipped: webgl has no LOD lane (windows/mac/linux only)",
        }));
        return Ok(());
    }
    let entity_type = entity_type_of(&input);
    let (content_by_file, bytes_by_hash) = content_maps(&input.files);
    let entity_hash = input
        .entity_hash
        .clone()
        .unwrap_or_else(|| entity_hash_of(&content_by_file));
    let (base, parcels) = scene_parcels(&input.files);
    let job = LodJob {
        sid: &entity_hash,
        target,
        entity_type: &entity_type,
        files: &input.files,
        content_by_file: &content_by_file,
        bytes_by_hash: &bytes_by_hash,
        base,
        parcels: &parcels,
        crop: input.crop,
        tri_cap: input.tri_cap,
    };
    if let Err(e) = bake_lod(&job, sink) {
        sink.emit_json(serde_json::json!({
            "ev": "file-error",
            "file": "LOD",
            "error": format!("{e:#}"),
        }));
    }
    Ok(())
}

fn parse_parcel_str(s: &str) -> Option<(i32, i32)> {
    let (x, y) = s.split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

fn scene_parcels(files: &[(String, Vec<u8>)]) -> ((i32, i32), Vec<(i32, i32)>) {
    for (name, data) in files {
        if !name.to_lowercase().ends_with("scene.json") {
            continue;
        }
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(data) {
            let base = v["scene"]["base"]
                .as_str()
                .and_then(parse_parcel_str)
                .unwrap_or((0, 0));
            let parcels: Vec<(i32, i32)> = v["scene"]["parcels"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|p| p.as_str().and_then(parse_parcel_str))
                        .collect()
                })
                .unwrap_or_default();
            let parcels = if parcels.is_empty() {
                vec![base]
            } else {
                parcels
            };
            return (base, parcels);
        }
    }
    ((0, 0), vec![(0, 0)])
}

fn lod_params_for(
    parcels: &[(i32, i32)],
    base: (i32, i32),
    sid: &str,
    level: u32,
) -> crate::builder::LodBuildParams {
    crate::builder::LodBuildParams {
        level,
        plane_clipping: crate::lods::plane_clipping(parcels),
        vertical_clipping: crate::lods::vertical_clipping(parcels.len()),
        root_position: crate::lods::root_position(base),
        main_asset: crate::lods::lod_main_asset(sid, level),
        timestamp: None,
        fidelity: false,
    }
}

struct LodJob<'a> {
    sid: &'a str,
    target: &'a str,
    entity_type: &'a str,
    files: &'a [(String, Vec<u8>)],
    content_by_file: &'a HashMap<String, String>,
    bytes_by_hash: &'a HashMap<String, &'a Vec<u8>>,
    base: (i32, i32),
    parcels: &'a [(i32, i32)],
    crop: bool,
    tri_cap: u32,
}

fn bake_lod(job: &LodJob, sink: &dyn Sink) -> crate::Result<()> {
    use crate::lodgen::{self, model as lmodel, placements};

    let level = 1u32;
    let sid = job.sid;
    let (target, base, parcels) = (job.target, job.base, job.parcels);
    let root_name = format!("{sid}_{level}");
    let iss = job
        .files
        .iter()
        .find(|(n, _)| n.ends_with(placements::ISS_SUFFIX));

    let mut merged;
    let sources;
    if let Some((_, iss_bytes)) = iss {
        let list = placements::parse_iss(iss_bytes)?;
        let mut file_by_hash: HashMap<&str, &str> = HashMap::new();
        for (name, _) in job.files {
            if let Some(h) = job.content_by_file.get(&name.to_lowercase()) {
                file_by_hash.entry(h.as_str()).or_insert(name.as_str());
            }
        }
        let fetch = |hash: &str| -> crate::Result<Vec<u8>> {
            job.bytes_by_hash
                .get(hash)
                .map(|b| (*b).clone())
                .ok_or_else(|| crate::anyhow!("content {hash} not in the upload"))
        };
        merged = lodgen::assemble::assemble_from(
            &root_name,
            job.content_by_file,
            &file_by_hash,
            &list,
            &fetch,
            lmodel::MatLane::default(),
        )?;
        sources = list.len();
        sink.emit_json(serde_json::json!({
            "ev": "note",
            "msg": format!("placements: {sources} from ISS"),
        }));
    } else {
        merged = lmodel::LodModel {
            root_name: root_name.clone(),
            ..Default::default()
        };
        let mut loaded = 0usize;
        let models = job
            .files
            .iter()
            .filter(|(n, _)| naming::GLTF_EXTENSIONS.contains(&ext_of(n).as_str()));
        for (name, bytes) in models {
            match lmodel::from_glb_bytes(bytes, name) {
                Ok(m) => {
                    let img_off = merged.images.len();
                    let mat_off = merged.materials.len();
                    merged.images.extend(m.images);
                    merged
                        .materials
                        .extend(m.materials.into_iter().map(|mut mat| {
                            if let Some(i) = mat.image.as_mut() {
                                *i += img_off;
                            }
                            mat
                        }));
                    merged
                        .primitives
                        .extend(m.primitives.into_iter().map(|mut p| {
                            p.material += mat_off;
                            p
                        }));
                    loaded += 1;
                }
                Err(e) => sink.emit_json(serde_json::json!({
                    "ev": "file-error",
                    "file": name,
                    "error": format!("lod load: {e:#}"),
                })),
            }
        }
        if loaded == 0 {
            sink.emit_json(serde_json::json!({
                "ev": "note",
                "msg": "LOD skipped: no model loaded",
            }));
            return Ok(());
        }
        sources = loaded;
    }

    sink.emit_json(serde_json::json!({
        "ev": "lod-start",
        "models": sources,
        "tris": merged.total_tris(),
        "parcels": parcels.len(),
    }));
    #[cfg(target_arch = "wasm32")]
    sink.emit_json(serde_json::json!({
        "ev": "note",
        "msg": "LOD lane: merge/ISS placements, optional parcel crop, atlas, optional meshopt \
                decimation, bundle; placements acquisition (manifest builder) and the gltfpack \
                backend stay native-only",
    }));

    if job.crop && job.entity_type == "scene" {
        let rects = lodgen::crop::crop_rects_rh(base, parcels);
        let report = lodgen::crop::crop(&mut merged, &rects);
        sink.emit_json(serde_json::json!({
            "ev": "lod-crop",
            "rects": report.rects,
            "trisIn": report.tris_in,
            "trisOut": report.tris_out,
            "trisClipped": report.tris_clipped,
            "trisDropped": report.tris_dropped,
            "primsDropped": report.prims_dropped,
            "vertsDropped": report.verts_dropped,
        }));
    }

    let atlased = lodgen::atlas::atlas(&merged, 1024, 2)?;
    for line in &atlased.log {
        sink.emit_json(serde_json::json!({ "ev": "note", "msg": format!("atlas: {line}") }));
    }
    sink.emit_json(serde_json::json!({
        "ev": "lod-atlas",
        "tris": atlased.total_tris(),
        "materials": atlased.materials.len(),
        "images": atlased.images.len(),
    }));

    let final_model = if job.tri_cap > 0 && atlased.total_tris() as u64 > job.tri_cap as u64 {
        let pre = lodgen::emit::emit_glb(&atlased)?;
        let reparsed = lmodel::from_glb_bytes(&pre, &root_name)?;
        let (sim, report) =
            lodgen::simplify_meshopt::simplify_model(&reparsed, job.tri_cap as u64, true)?;
        sink.emit_json(serde_json::json!({
            "ev": "lod-simplify",
            "trisBefore": report.tris_before,
            "trisAfter": report.tris_after,
            "sloppy": report.aggressive_final,
        }));
        sim
    } else {
        atlased
    };

    let glb = lodgen::emit::emit_glb(&final_model)?;
    let bundle_name = crate::lods::lod_bundle_name(sid, level, target);
    let root_hash = format!("{sid}_{level}");
    let src_name = format!("{sid}_{level}.glb");
    let params = lod_params_for(parcels, base, sid, level);
    let opts = BuildOpts {
        source_file: Some(&src_name),
        lod: Some(&params),
        real_textures: true,
        ..Default::default()
    };
    let data = build_bundle(&glb, &bundle_name, &root_hash, &opts)?.data;

    let checks = lodgen::self_gate_bundle(&data, sid, level, target)?;
    let cjson: Vec<serde_json::Value> = checks
        .iter()
        .map(|c| {
            serde_json::json!({
                "label": c.label,
                "ok": c.ok,
                "detail": c.detail,
            })
        })
        .collect();
    sink.emit_json(serde_json::json!({
        "ev": "gate",
        "bundle": bundle_name,
        "failures": lodgen::gate_failures(&checks),
        "checks": cjson,
    }));
    sink.emit_output(&bundle_name, &data);

    let rel = format!("LOD/{level}/{bundle_name}");
    sink.emit_output(
        &format!("{bundle_name}.br"),
        &crate::compress::brotli(&data)?,
    );
    let lod_manifest = serde_json::json!({
        "version": crate::manifest::DEFAULT_AB_VERSION,
        "sceneId": sid,
        "levels": [level],
        "files": [rel.as_str()],
        "exitCode": 0,
    });
    let text = serde_json::to_string_pretty(&lod_manifest)?;
    sink.emit_output("LOD.manifest.json", text.as_bytes());
    sink.emit_output(
        "LOD.manifest.json.br",
        &crate::compress::brotli(text.as_bytes())?,
    );

    sink.emit_json(serde_json::json!({
        "ev": "lod-done",
        "bundle": bundle_name,
        "bytes": data.len(),
        "servePath": rel,
    }));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::{parse_input, Collected, CollectingSink, HostInfo, InputBuilder};

    const TEST_HOST: HostInfo = HostInfo::new("v-abgen-test", "test://inline");

    fn tiny_gltf() -> Vec<u8> {
        const BUF_B64: &str = "AAAAAAAAAAAAAAAAAACAPwAAAAAAAAAAAAAAAAAAgD8AAAAAAAABAAIA";
        format!(
            "{{\"asset\":{{\"version\":\"2.0\"}},\
             \"scene\":0,\"scenes\":[{{\"nodes\":[0]}}],\
             \"nodes\":[{{\"mesh\":0,\"name\":\"tri\"}}],\
             \"meshes\":[{{\"primitives\":[{{\"attributes\":{{\"POSITION\":0}},\
             \"indices\":1,\"material\":0}}]}}],\
             \"materials\":[{{\"name\":\"mat_0\",\"pbrMetallicRoughness\":{{}}}}],\
             \"accessors\":[\
               {{\"bufferView\":0,\"componentType\":5126,\"count\":3,\"type\":\"VEC3\",\
                 \"min\":[0,0,0],\"max\":[1,1,0]}},\
               {{\"bufferView\":1,\"componentType\":5123,\"count\":3,\"type\":\"SCALAR\"}}],\
             \"bufferViews\":[\
               {{\"buffer\":0,\"byteOffset\":0,\"byteLength\":36}},\
               {{\"buffer\":0,\"byteOffset\":36,\"byteLength\":6}}],\
             \"buffers\":[{{\"byteLength\":42,\
               \"uri\":\"data:application/octet-stream;base64,{BUF_B64}\"}}]}}"
        )
        .into_bytes()
    }

    fn convert_with_sibling(sibling: &[u8]) -> Vec<(String, Vec<u8>)> {
        let blob = InputBuilder::new()
            .file("models/tri.gltf", tiny_gltf())
            .file(
                "scene.json",
                br#"{"scene":{"base":"0,0","parcels":["0,0"]}}"#.to_vec(),
            )
            .file("bin/game.js", sibling.to_vec())
            .platform("windows")
            .build();
        let input = parse_input(&blob).expect("request parses");
        let sink = CollectingSink::new();
        convert(input, &sink, TEST_HOST).expect("convert");
        let got = sink.take();
        assert!(got.errors.is_empty(), "unexpected errors: {:?}", got.errors);
        got.outputs
    }

    #[test]
    fn a_model_bundle_ignores_its_siblings() {
        let a = convert_with_sibling(b"console.log('one')");
        let b = convert_with_sibling(b"console.log('a different, longer program')");

        assert_eq!(a.len(), 1, "expected exactly one bundle, got {a:?}");
        let (name_a, data_a) = &a[0];
        let (name_b, data_b) = &b[0];
        assert_eq!(name_a, name_b, "bundle name moved with the sibling");
        assert!(data_a.starts_with(b"UnityFS"), "not a UnityFS bundle");
        assert_eq!(
            (sha256_hex(data_a), data_a.len()),
            (sha256_hex(data_b), data_b.len()),
            "bundle {name_a} body moved with an unrelated sibling file",
        );
    }

    #[test]
    fn converting_the_same_upload_twice_is_byte_identical() {
        let a = convert_with_sibling(b"console.log('one')");
        let b = convert_with_sibling(b"console.log('one')");
        let digest = |o: &[(String, Vec<u8>)]| -> Vec<(String, String)> {
            o.iter().map(|(n, d)| (n.clone(), sha256_hex(d))).collect()
        };
        assert_eq!(digest(&a), digest(&b));
    }

    /// A distinct-content tiny glTF per tag, so a multi-file scene produces
    /// distinct bundle names instead of deduping to one.
    fn tiny_gltf_tagged(tag: &str) -> Vec<u8> {
        let base = String::from_utf8(tiny_gltf()).expect("tiny_gltf is utf8");
        base.replace("\"tri\"", &format!("\"tri_{tag}\""))
            .into_bytes()
    }

    fn convert_multi_glb(jobs: usize) -> (Collected, usize) {
        let n = 5;
        let mut b = InputBuilder::new();
        for i in 0..n {
            b = b.file(
                format!("models/m{i}.gltf"),
                tiny_gltf_tagged(&format!("{i}")),
            );
        }
        let blob = b
            .file(
                "scene.json",
                br#"{"scene":{"base":"0,0","parcels":["0,0"]}}"#.to_vec(),
            )
            .platform("windows")
            .build();
        let input = parse_input(&blob).expect("request parses");
        let sink = CollectingSink::new();
        convert_with_jobs(input, &sink, TEST_HOST, jobs).expect("convert");
        (sink.take(), n)
    }

    /// The whole point of file-level parallelism is that it must be
    /// invisible from the outside: same bundles, same bytes, same manifest,
    /// and — because `convert` buffers each file's events and flushes them
    /// in input order — the exact same JSON event sequence, whether the
    /// files run one at a time or `jobs` at a time.
    #[test]
    fn concurrency_does_not_change_output_or_event_order() {
        let (serial, n) = convert_multi_glb(1);
        let (parallel, _) = convert_multi_glb(8);

        assert!(serial.errors.is_empty(), "{:?}", serial.errors);
        assert!(parallel.errors.is_empty(), "{:?}", parallel.errors);
        assert_eq!(serial.outputs.len(), n, "expected one bundle per model");

        assert_eq!(
            serial.events, parallel.events,
            "event stream diverged between jobs=1 and jobs=8"
        );
        let digest = |o: &[(String, Vec<u8>)]| -> Vec<(String, String)> {
            o.iter().map(|(n, d)| (n.clone(), sha256_hex(d))).collect()
        };
        assert_eq!(
            digest(&serial.outputs),
            digest(&parallel.outputs),
            "output bundle names/bytes diverged between jobs=1 and jobs=8"
        );
        assert_eq!(serial.manifest, parallel.manifest);
    }
}

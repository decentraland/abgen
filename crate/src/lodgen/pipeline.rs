use anyhow::{anyhow, bail, Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::catalyst::{CatalystClient, Scene};
use crate::lods;

use super::gate::{
    push_check, self_gate_bundle_with, tri_cap_check, GateCheck, BUNDLE_TEXTURE_MAX,
};
use super::model::LodModel;
use super::simplify_meshopt::SimplifyPolicy;
use super::{assemble, atlas, crop, emit, model, placements, reclamp, simplify, simplify_meshopt};

pub fn parse_parcel(s: &str) -> Result<(i32, i32)> {
    let parts: Vec<&str> = s.trim().split(',').collect();
    if parts.len() != 2 {
        bail!("bad parcel {s:?} (want X,Y)");
    }
    Ok((
        parts[0]
            .trim()
            .parse()
            .with_context(|| format!("parcel x in {s:?}"))?,
        parts[1]
            .trim()
            .parse()
            .with_context(|| format!("parcel y in {s:?}"))?,
    ))
}

pub fn scene_geometry(ent: &Scene) -> Result<((i32, i32), Vec<(i32, i32)>)> {
    let scene_meta = ent
        .metadata
        .get("scene")
        .ok_or_else(|| anyhow!("entity {} metadata has no scene block", ent.entity_id))?;
    let base = scene_meta
        .get("base")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("entity {} metadata.scene has no base", ent.entity_id))?;
    let parcels: Vec<(i32, i32)> = scene_meta
        .get("parcels")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("entity {} metadata.scene has no parcels", ent.entity_id))?
        .iter()
        .filter_map(|p| p.as_str())
        .filter_map(|p| parse_parcel(p).ok())
        .collect();
    if parcels.is_empty() {
        bail!("entity {} has no parseable parcels", ent.entity_id);
    }
    Ok((parse_parcel(base)?, parcels))
}

pub fn acquire_placements(
    client: &CatalystClient,
    ent: &Scene,
    iss: &str,
) -> Result<Vec<placements::Placement>> {
    acquire_placements_independently(client, ent, iss).map(|(placements, _)| placements)
}

fn scene_has_executable_path(ent: &Scene) -> bool {
    if ent.metadata.get("runtimeVersion").and_then(|v| v.as_str()) != Some("7") {
        return true;
    }
    ent.metadata
        .get("main")
        .and_then(|v| v.as_str())
        .is_some_and(|main| !main.eq_ignore_ascii_case("main.crdt"))
}

fn placement_suspicion(ent: &Scene, full: &placements::ManifestPlacements) -> Vec<&'static str> {
    let mut why = Vec::new();
    if scene_has_executable_path(ent) {
        why.push("executable scene entrypoint");
    }
    if full.placements.is_empty() {
        why.push("zero placements");
    }
    if full.skipped_mesh_renderer > 0 {
        why.push("mesh-renderer components");
    }
    if full.unresolved_src > 0 {
        why.push("unresolved glTF sources");
    }
    why
}

fn finish_auto_placements<F>(
    ent: &Scene,
    static_result: Result<placements::ManifestPlacements>,
    execute_sdk: F,
) -> Result<(Vec<placements::Placement>, &'static str)>
where
    F: FnOnce() -> Result<Option<placements::ManifestPlacements>>,
{
    let (static_full, suspicious) = match static_result {
        Ok(full) => {
            let suspicious = placement_suspicion(ent, &full);
            (Some(full), suspicious)
        }
        Err(error) => {
            eprintln!("static placements invalid ({error:#}); executing embedded SDK");
            (None, vec!["invalid main.crdt"])
        }
    };
    if suspicious.is_empty() {
        let static_full = static_full.expect("clean static result");
        eprintln!(
            "source: current-deployment CRDT ({} placements)",
            static_full.placements.len()
        );
        return Ok((static_full.placements, "static-crdt"));
    }
    if static_full.is_some() {
        eprintln!(
            "static placements suspicious ({}); executing embedded SDK",
            suspicious.join(", ")
        );
    }
    let full = execute_sdk()?.ok_or_else(|| {
        anyhow!(
            "scene {} emitted no renderer state; refusing to publish an empty LOD",
            ent.entity_id
        )
    })?;
    if full.placements.is_empty() {
        bail!(
            "scene {} produced zero placements after SDK execution; refusing to publish an empty LOD",
            ent.entity_id
        );
    }
    if full.unresolved_src > 0 {
        bail!(
            "scene {} SDK output is incomplete: {} unresolved glTF sources",
            ent.entity_id,
            full.unresolved_src
        );
    }
    eprintln!(
        "source: embedded-scene-runtime ({} placements, {} mesh-renderer-only skipped, {} unresolved src)",
        full.placements.len(),
        full.skipped_mesh_renderer,
        full.unresolved_src
    );
    Ok((full.placements, "embedded-sdk"))
}

fn acquire_placements_independently(
    client: &CatalystClient,
    ent: &Scene,
    iss: &str,
) -> Result<(Vec<placements::Placement>, &'static str)> {
    if iss != "auto" && iss != "off" {
        bail!(
            "--iss FILE cannot supply generated placements; derive independently and use --diff-iss FILE for comparison"
        );
    }
    let static_result = crate::lodgen::scenerun::static_scene_placements(client, ent);
    finish_auto_placements(ent, static_result, || {
        crate::lodgen::scenerun::run_scene_placements(client, ent)
    })
}

pub fn write_iss_descriptor(
    out_dir: &Path,
    scene_id: &str,
    list: &[placements::Placement],
    content_by_file: &HashMap<String, String>,
) -> Result<(PathBuf, usize, usize)> {
    let mut assets: Vec<(String, &placements::Placement)> = Vec::new();
    let mut skipped = 0usize;
    for p in list {
        match assemble::resolve_placement_hash(p, content_by_file) {
            Ok(h) => assets.push((h, p)),
            Err(_) => skipped += 1,
        }
    }
    let doc = placements::iss_descriptor(scene_id, &assets);
    let text = serde_json::to_string_pretty(&doc)?;
    let dir = out_dir.join(scene_id);
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let path = dir.join(format!("{scene_id}{}", placements::ISS_SUFFIX));
    lods::write_atomic(&path, text.as_bytes())?;
    Ok((path, assets.len(), skipped))
}

pub fn staged_glb_name(scene_id: &str, level: u32) -> String {
    format!("{}_{}.glb", scene_id.to_lowercase(), level)
}

pub fn expected_rel_path(scene_id: &str, level: u32, platform: &str) -> String {
    format!(
        "LOD/{}/{}",
        level,
        lods::lod_bundle_name(scene_id, level, platform)
    )
}

#[derive(Clone, Debug)]
pub struct GenerateParams {
    pub scene: String,
    pub out_dir: String,
    pub platform: String,
    pub platforms: Vec<String>,
    pub levels: Vec<u32>,
    /// `-si` ratio for the budget lanes; `simplify_policy` carries its own.
    pub ratio: f64,
    /// Level-1 decimation policy (default production `gltfpack -si 0.1 -se 0.01`).
    pub simplify_policy: SimplifyPolicy,
    /// Budget-policy cap; consulted only under `SimplifyPolicy::Budget`.
    pub tri_cap: Option<u64>,
    /// Budget policy: cap at 500 x parcels when no explicit `tri_cap`.
    pub tri_cap_auto: bool,
    /// Atlas canvas ceiling (MeshBaker's AutoSizeAtlas tops out at 2048).
    pub atlas_max: u32,
    pub atlas_padding: u32,
    pub atlas_mode: atlas::AtlasMode,
    pub crop: bool,
    pub catalyst: String,
    pub iss: String,
    pub workdir: Option<PathBuf>,
    pub cache: Option<PathBuf>,
    pub simplifier: simplify::SimplifierBackend,
    pub gltfpack: Option<PathBuf>,
    pub allow_unsimplified: bool,
    pub keep_glb: bool,
    pub uv_reclamp: bool,
    pub bake_after_simplify: bool,
    pub emissive_channel: bool,
    pub fidelity: bool,
}

impl Default for GenerateParams {
    fn default() -> Self {
        GenerateParams {
            scene: String::new(),
            out_dir: "lodgen-out".to_string(),
            platform: "windows".to_string(),
            platforms: Vec::new(),
            levels: vec![1],
            ratio: 0.1,
            simplify_policy: SimplifyPolicy::default(),
            tri_cap: None,
            tri_cap_auto: true,
            atlas_max: 2048,
            atlas_padding: 2,
            atlas_mode: atlas::AtlasMode::MeshBaker,
            crop: true,
            catalyst: "https://peer.decentraland.org/content".to_string(),
            iss: "auto".to_string(),
            workdir: None,
            cache: None,
            simplifier: simplify::SimplifierBackend::from_env(),
            gltfpack: None,
            allow_unsimplified: false,
            keep_glb: false,
            uv_reclamp: true,
            bake_after_simplify: false,
            emissive_channel: false,
            fidelity: false,
        }
    }
}

#[derive(Debug)]
pub struct LevelBuild {
    pub level: u32,
    pub rel_path: String,
    pub bundle_path: PathBuf,
    pub bundle_bytes: usize,
    pub simplify: simplify::SimplifyReport,
    pub glb_path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PlacementStats {
    pub count: usize,
    pub rotated: usize,
    pub non_uniform_scale: usize,
    pub mirrored: usize,
    pub extreme_scale: usize,
}

fn placement_stats(placements: &[placements::Placement]) -> PlacementStats {
    let mut stats = PlacementStats {
        count: placements.len(),
        ..Default::default()
    };
    for placement in placements {
        let [x, y, z, w] = placement.rotation;
        if x.abs() > 1e-6 || y.abs() > 1e-6 || z.abs() > 1e-6 || (w.abs() - 1.0).abs() > 1e-6 {
            stats.rotated += 1;
        }
        let [x, y, z] = placement.scale;
        if (x.abs() - y.abs()).abs() > 1e-6 || (y.abs() - z.abs()).abs() > 1e-6 {
            stats.non_uniform_scale += 1;
        }
        if x * y * z < 0.0 {
            stats.mirrored += 1;
        }
        if [x, y, z].iter().any(|v| v.abs() < 0.01 || v.abs() > 100.0) {
            stats.extreme_scale += 1;
        }
    }
    stats
}

#[derive(Debug)]
pub struct GenerateOutcome {
    pub entity_id: String,
    pub scene_id: String,
    pub source_tris: usize,
    pub placement_stats: PlacementStats,
    pub levels: Vec<LevelBuild>,
    pub gate: Vec<GateCheck>,
    pub log: Vec<String>,
}

pub fn normalize_levels(levels: &[u32]) -> Result<Vec<u32>> {
    if levels.is_empty() {
        bail!("generate needs at least one LOD level");
    }
    let mut out: Vec<u32> = Vec::new();
    for &l in levels {
        if l >= 2 {
            bail!(
                "LOD level {l} refused: production stopped emitting level 2 (~2024-04); \
                 only levels 0/1 are generated"
            );
        }
        if !out.contains(&l) {
            out.push(l);
        }
    }
    Ok(out)
}

pub const TRIS_PER_PARCEL: u64 = 500;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SimplifyLane {
    Passthrough,
    /// Production policy: one `-si ratio -se target_error` pass, no cap.
    GltfpackSi {
        ratio: f32,
        target_error: f32,
    },
    Uncapped {
        ratio: f64,
    },
    Capped {
        ratio: f64,
        cap: u64,
    },
}

/// The tri cap in force at `level`: `None` at level 0 and under the
/// gltfpack-si policy (which never caps); under the budget policy the
/// parcel threshold when `tri_cap_auto`, else the explicit cap.
pub fn effective_tri_cap(
    level: u32,
    policy: SimplifyPolicy,
    tri_cap: Option<u64>,
    tri_cap_auto: bool,
    threshold: u64,
) -> Option<u64> {
    if level == 0 || policy != SimplifyPolicy::Budget {
        return None;
    }
    if tri_cap_auto {
        Some(threshold)
    } else {
        tri_cap
    }
}

pub fn choose_lane(
    level: u32,
    policy: SimplifyPolicy,
    tri_cap: Option<u64>,
    tri_cap_auto: bool,
    ratio: f64,
    source_tris: usize,
    threshold: u64,
) -> SimplifyLane {
    if level == 0 {
        return SimplifyLane::Passthrough;
    }
    if let SimplifyPolicy::GltfpackSi {
        ratio,
        target_error,
    } = policy
    {
        return if source_tris == 0 {
            SimplifyLane::Passthrough
        } else {
            SimplifyLane::GltfpackSi {
                ratio,
                target_error,
            }
        };
    }
    match effective_tri_cap(level, policy, tri_cap, tri_cap_auto, threshold) {
        Some(cap) if source_tris as u64 <= cap => SimplifyLane::Passthrough,
        Some(cap) => SimplifyLane::Capped { ratio, cap },
        None if source_tris as u64 <= threshold => SimplifyLane::Passthrough,
        None => SimplifyLane::Uncapped { ratio },
    }
}

fn run_gltfpack_lane(
    pre: &Path,
    out: &Path,
    params: &GenerateParams,
    ratio: f64,
    cap: Option<u64>,
) -> Result<simplify::SimplifyReport> {
    let granularity = if params.bake_after_simplify {
        simplify::RescueGranularity::AlphaClass
    } else {
        simplify::RescueGranularity::Material
    };
    match simplify::resolve_gltfpack(params.gltfpack.as_deref()) {
        Ok(bin) => match simplify::simplify_with(pre, out, ratio, cap, &bin, granularity) {
            Ok(r) => Ok(r),
            Err(e) if params.allow_unsimplified => {
                eprintln!("WARNING: gltfpack failed ({e:#}); --allow-unsimplified passthrough");
                simplify::copy_unsimplified(pre, out)
            }
            Err(e) => Err(e),
        },
        Err(e) if params.allow_unsimplified => {
            eprintln!("WARNING: {e:#}; --allow-unsimplified passthrough");
            simplify::copy_unsimplified(pre, out)
        }
        Err(e) => Err(e),
    }
}

fn run_gltfpack_si_lane(
    pre: &Path,
    out: &Path,
    params: &GenerateParams,
    ratio: f32,
    target_error: f32,
) -> Result<simplify::SimplifyReport> {
    match simplify::resolve_gltfpack(params.gltfpack.as_deref()) {
        Ok(bin) => match simplify::simplify_si(pre, out, ratio, target_error, &bin) {
            Ok(r) => Ok(r),
            Err(e) if params.allow_unsimplified => {
                eprintln!("WARNING: gltfpack failed ({e:#}); --allow-unsimplified passthrough");
                simplify::copy_unsimplified(pre, out)
            }
            Err(e) => Err(e),
        },
        Err(e) if params.allow_unsimplified => {
            eprintln!("WARNING: {e:#}; --allow-unsimplified passthrough");
            simplify::copy_unsimplified(pre, out)
        }
        Err(e) => Err(e),
    }
}

fn run_meshopt_si_lane(
    model: &LodModel,
    pre: &Path,
    out: &Path,
    params: &GenerateParams,
    ratio: f32,
    target_error: f32,
) -> Result<simplify::SimplifyReport> {
    let attempt = || -> Result<simplify::SimplifyReport> {
        let (m, report) = simplify_meshopt::simplify_model_si(model, ratio, target_error)?;
        let glb = emit::emit_glb(&m)?;
        std::fs::write(out, &glb).with_context(|| format!("write {}", out.display()))?;
        Ok(report)
    };
    match attempt() {
        Ok(r) => Ok(r),
        Err(e) if params.allow_unsimplified => {
            eprintln!("WARNING: meshopt simplify failed ({e:#}); --allow-unsimplified passthrough");
            simplify::copy_unsimplified(pre, out)
        }
        Err(e) => Err(e),
    }
}

fn run_meshopt_lane(
    model: &LodModel,
    pre: &Path,
    out: &Path,
    params: &GenerateParams,
    target_tris: u64,
    enforce_cap: bool,
) -> Result<simplify::SimplifyReport> {
    let attempt = || -> Result<simplify::SimplifyReport> {
        let (m, report) = simplify_meshopt::simplify_model(model, target_tris, enforce_cap)?;
        let glb = emit::emit_glb(&m)?;
        std::fs::write(out, &glb).with_context(|| format!("write {}", out.display()))?;
        Ok(report)
    };
    match attempt() {
        Ok(r) => Ok(r),
        Err(e) if params.allow_unsimplified => {
            eprintln!("WARNING: meshopt simplify failed ({e:#}); --allow-unsimplified passthrough");
            simplify::copy_unsimplified(pre, out)
        }
        Err(e) => Err(e),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_simplify(
    model: &LodModel,
    pre: &Path,
    out: &Path,
    params: &GenerateParams,
    level: u32,
    source_tris: usize,
    parcel_count: usize,
    log: &mut Vec<String>,
) -> Result<simplify::SimplifyReport> {
    let threshold = TRIS_PER_PARCEL * parcel_count as u64;
    let lane = choose_lane(
        level,
        params.simplify_policy,
        params.tri_cap,
        params.tri_cap_auto,
        params.ratio,
        source_tris,
        threshold,
    );
    match lane {
        SimplifyLane::Passthrough => {
            if level == 0 {
                if params.tri_cap.is_some() {
                    eprintln!(
                        "WARNING: --tri-cap is ignored at level 0; level 0 is always the \
                         un-decimated pass-through bake"
                    );
                }
                log.push(format!(
                    "simplify-lane[0]: level-0 pass-through ({source_tris} tris, ratio 1.0, no gltfpack)"
                ));
            } else {
                match effective_tri_cap(
                    level,
                    params.simplify_policy,
                    params.tri_cap,
                    params.tri_cap_auto,
                    threshold,
                ) {
                    Some(cap) => log.push(format!(
                        "simplify-lane[{level}]: pass-through under cap ({source_tris} tris <= cap {cap})"
                    )),
                    None if params.simplify_policy != SimplifyPolicy::Budget => log.push(format!(
                        "simplify-lane[{level}]: pass-through (empty source, {})",
                        params.simplify_policy.name()
                    )),
                    None => log.push(format!(
                        "simplify-lane[{level}]: pass-through ({source_tris} tris <= {threshold} = {TRIS_PER_PARCEL} x {parcel_count} parcels)"
                    )),
                }
            }
            simplify::passthrough(pre, out)
        }
        SimplifyLane::GltfpackSi {
            ratio,
            target_error,
        } => {
            log.push(format!(
                "simplify-lane[{level}]: gltfpack-si ratio {ratio} -se {target_error} uncapped ({source_tris} tris, {})",
                params.simplifier.name()
            ));
            match params.simplifier {
                simplify::SimplifierBackend::Gltfpack => {
                    run_gltfpack_si_lane(pre, out, params, ratio, target_error)
                }
                simplify::SimplifierBackend::Meshopt => {
                    run_meshopt_si_lane(model, pre, out, params, ratio, target_error)
                }
            }
        }
        SimplifyLane::Uncapped { ratio } => {
            log.push(format!(
                "simplify-lane[{level}]: ratio {ratio} uncapped ({source_tris} tris > {threshold} = {TRIS_PER_PARCEL} x {parcel_count} parcels, {})",
                params.simplifier.name()
            ));
            match params.simplifier {
                simplify::SimplifierBackend::Gltfpack => {
                    run_gltfpack_lane(pre, out, params, ratio, None)
                }
                simplify::SimplifierBackend::Meshopt => {
                    let target = (source_tris as f64 * ratio.clamp(1e-3, 1.0)).round() as u64;
                    run_meshopt_lane(model, pre, out, params, target, false)
                }
            }
        }
        SimplifyLane::Capped { ratio, cap } => {
            log.push(format!(
                "simplify-lane[{level}]: capped (tri cap {cap}, {})",
                params.simplifier.name()
            ));
            match params.simplifier {
                simplify::SimplifierBackend::Gltfpack => {
                    run_gltfpack_lane(pre, out, params, ratio, Some(cap))
                }
                simplify::SimplifierBackend::Meshopt => {
                    run_meshopt_lane(model, pre, out, params, cap, true)
                }
            }
        }
    }
}

pub fn generate(params: &GenerateParams) -> Result<GenerateOutcome> {
    let levels = normalize_levels(&params.levels)?;
    if params.scene.is_empty() {
        bail!("generate needs --scene <pointer|entityId>");
    }
    let mut platforms: Vec<String> = if params.platforms.is_empty() {
        vec![params.platform.clone()]
    } else {
        params.platforms.clone()
    };
    let mut seen = HashSet::new();
    platforms.retain(|p| seen.insert(p.clone()));
    for p in &platforms {
        lods::validate_lod_platform(p)?;
    }
    let primary = platforms[0].clone();
    let client =
        CatalystClient::from_args(&params.catalyst, None).with_content_cache(params.cache.clone());
    let ent = client
        .resolve_scene(&params.scene)
        .with_context(|| format!("resolve scene {:?}", params.scene))?;
    let sid = ent.entity_id.to_lowercase();
    let mut log: Vec<String> = Vec::new();
    log.push(format!("entity: {}", ent.entity_id));
    let (base, parcels) = scene_geometry(&ent)?;
    let parcel_count = parcels.len();
    log.push(format!("base={},{} parcels={parcel_count}", base.0, base.1));

    let t_total = std::time::Instant::now();
    let t = std::time::Instant::now();
    let (placements, placement_source) =
        acquire_placements_independently(&client, &ent, &params.iss)?;
    let placements_ms = t.elapsed().as_millis();
    log.push(format!("placement-source: {placement_source}"));
    log.push(format!("placements: {}", placements.len()));
    if placements.is_empty() {
        bail!(
            "scene {} produced zero placements; refusing to publish an empty LOD",
            ent.entity_id
        );
    }
    if placements.iter().all(|p| p.scale.iter().all(|s| *s == 0.0)) {
        bail!(
            "scene {} produced {} placements, all scaled to zero; refusing to emit an invisible bundle",
            ent.entity_id,
            placements.len()
        );
    }
    let placement_stats = placement_stats(&placements);

    if let Some(dir) = params.cache.as_deref() {
        std::fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
    }
    let staging = params
        .workdir
        .clone()
        .unwrap_or_else(|| PathBuf::from(&params.out_dir).join(".work"));
    std::fs::create_dir_all(&staging).with_context(|| format!("mkdir {}", staging.display()))?;
    let pre = staging.join(format!("{sid}_pre.glb"));

    let assemble_ms;
    let mut atlas_ms = 0;
    let mut emit_ms;
    let mut simplify_ms = 0;
    let mut crop_stats: Option<crop::UnionStats> = None;
    let source_tris;
    let mut staged: Vec<(u32, PathBuf, simplify::SimplifyReport, Option<u32>)> = Vec::new();
    {
        let t = std::time::Instant::now();
        let mut model = assemble::assemble(
            &client,
            &ent,
            &placements,
            levels[0],
            params.cache.as_deref(),
            model::MatLane {
                emissive_channel: params.emissive_channel,
                fidelity: params.fidelity,
                ..Default::default()
            },
        )?;
        assemble_ms = t.elapsed().as_millis();
        if params.crop {
            let rects = crop::crop_rects_rh(base, &parcels);
            let report = crop::crop(&mut model, &rects);
            eprintln!("crop: {}", report.summary());
            if !model.primitives.is_empty() {
                crop_stats = Some(crop::union_stats(&model, &rects, 1e-3));
            }
        }
        let t = std::time::Instant::now();
        let mode = params.atlas_mode;
        if params.bake_after_simplify {
            log.extend(model.log.iter().cloned());
            let root_name = model.root_name.clone();
            source_tris = model.total_tris();
            let t = std::time::Instant::now();
            let glb = emit::emit_glb(&model)?;
            std::fs::write(&pre, &glb).with_context(|| format!("write {}", pre.display()))?;
            emit_ms = t.elapsed().as_millis();

            for &level in &levels {
                let out = staging.join(staged_glb_name(&sid, level));
                let dec = staging.join(format!("{}_{}.dec.glb", sid, level));
                let t = std::time::Instant::now();
                let sim = run_simplify(
                    &model,
                    &pre,
                    &dec,
                    params,
                    level,
                    source_tris,
                    parcel_count,
                    &mut log,
                )?;
                simplify_ms += t.elapsed().as_millis();
                log.push(format!("simplify[{level}]: {}", sim.summary()));

                let t = std::time::Instant::now();
                let bytes =
                    std::fs::read(&dec).with_context(|| format!("read {}", dec.display()))?;
                let decimated =
                    model::from_glb_bytes_with(&bytes, &root_name, params.emissive_channel)?;
                let atlased = atlas::atlas_with(
                    &decimated,
                    params.atlas_max,
                    params.atlas_padding,
                    mode,
                    params.fidelity,
                )?;
                atlas_ms += t.elapsed().as_millis();
                log.extend(atlased.log.iter().cloned());

                let t = std::time::Instant::now();
                let glb = emit::emit_glb(&atlased)?;
                std::fs::write(&out, &glb).with_context(|| format!("write {}", out.display()))?;
                emit_ms += t.elapsed().as_millis();
                if !params.keep_glb {
                    let _ = std::fs::remove_file(&dec);
                }
                staged.push((level, out, sim, atlas::max_image_side(&atlased)));
            }
        } else {
            let (model, atlas_rects) = atlas::atlas_with_rects(
                &model,
                params.atlas_max,
                params.atlas_padding,
                mode,
                params.fidelity,
            )?;
            atlas_ms = t.elapsed().as_millis();
            log.extend(model.log.iter().cloned());
            source_tris = model.total_tris();
            let atlas_side = atlas::max_image_side(&model);

            let t = std::time::Instant::now();
            let glb = emit::emit_glb(&model)?;
            std::fs::write(&pre, &glb).with_context(|| format!("write {}", pre.display()))?;
            emit_ms = t.elapsed().as_millis();

            for &level in &levels {
                let out = staging.join(staged_glb_name(&sid, level));
                let t = std::time::Instant::now();
                let sim = run_simplify(
                    &model,
                    &pre,
                    &out,
                    params,
                    level,
                    source_tris,
                    parcel_count,
                    &mut log,
                )?;
                simplify_ms += t.elapsed().as_millis();
                log.push(format!("simplify[{level}]: {}", sim.summary()));
                if params.uv_reclamp && !sim.passthrough {
                    let bytes =
                        std::fs::read(&out).with_context(|| format!("read {}", out.display()))?;
                    let mut clamped =
                        model::from_glb_bytes_with(&bytes, "reclamp", params.emissive_channel)?;
                    let rep = reclamp::reclamp_model(&mut clamped, &atlas_rects);
                    if rep.reclamped > 0 {
                        std::fs::write(&out, emit::emit_glb(&clamped)?)
                            .with_context(|| format!("write {}", out.display()))?;
                    }
                    log.push(format!(
                        "reclamp[{level}]: {} of {} tris crossed atlas tile rects; snapped to majority tile",
                        rep.reclamped, rep.scanned
                    ));
                }
                staged.push((level, out, sim, atlas_side));
            }
        }
    }

    let opts = lods::LodOptions {
        platform: primary.clone(),
        lod: Some(lods::LodGenMeta {
            parcels,
            base,
            timestamp: None,
            vertical_override: None,
            fidelity: params.fidelity,
        }),
        ..Default::default()
    };
    let sources: Vec<String> = staged
        .iter()
        .map(|(_, p, _, _)| p.to_string_lossy().into_owned())
        .collect();
    let t_package = std::time::Instant::now();
    let scene_dir = PathBuf::from(&params.out_dir).join(&sid);
    let (conv, mut published, bundle_ms) = std::thread::scope(|scope| -> Result<_> {
        let jobs: Vec<_> = staged
            .iter()
            .filter(|(level, _, _, _)| *level >= 1)
            .map(|(level, staged_glb, _, _)| {
                let level = *level;
                let scene_dir = &scene_dir;
                let sid = &sid;
                (
                    level,
                    scope.spawn(move || -> Result<(PathBuf, u64, usize)> {
                        let float_glb = std::fs::read(staged_glb)
                            .with_context(|| format!("read staged glb {}", staged_glb.display()))?;
                        let path = lods::write_published_glb(scene_dir, sid, level, &float_glb)?;
                        let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                        Ok((path, bytes, float_glb.len()))
                    }),
                )
            })
            .collect();
        let t_bundle = std::time::Instant::now();
        let conv =
            lods::convert_lods_platforms(&client, &sources, &params.out_dir, &opts, &platforms)?;
        let bundle_ms = t_bundle.elapsed().as_millis();
        let mut published = HashMap::with_capacity(jobs.len());
        for (level, job) in jobs {
            let result = job
                .join()
                .map_err(|_| anyhow!("published GLB worker panicked"))??;
            published.insert(level, result);
        }
        Ok((conv, published, bundle_ms))
    })?;
    let package_ms = t_package.elapsed().as_millis();
    let t_finalize = std::time::Instant::now();
    if !conv.skipped.is_empty() {
        bail!("convert_lods skipped sources: {:?}", conv.skipped);
    }
    if conv.results.is_empty() {
        bail!("convert_lods produced no result");
    }
    let mut gate: Vec<GateCheck> = Vec::new();
    push_check(
        &mut gate,
        "scene-id",
        conv.scene_id == sid,
        format!("got {} want {sid}", conv.scene_id),
    );
    let manifest = PathBuf::from(&params.out_dir)
        .join(&conv.scene_id)
        .join("LOD.manifest.json");
    push_check(
        &mut gate,
        "lod-manifest",
        manifest.is_file(),
        manifest.display().to_string(),
    );
    let (iss_path, iss_assets, iss_skipped) = write_iss_descriptor(
        Path::new(&params.out_dir),
        &conv.scene_id,
        &placements,
        &ent.content_by_file(),
    )?;
    if iss_skipped > 0 {
        eprintln!(
            "WARNING: {iss_skipped} placement(s) resolved to no content hash; omitted from the ISS descriptor"
        );
    }
    log.push(format!(
        "iss-descriptor: {} ({iss_assets} assets, {iss_skipped} skipped)",
        iss_path.display()
    ));
    let iss_roundtrip = std::fs::read(&iss_path)
        .ok()
        .and_then(|b| placements::parse_iss(&b).ok())
        .map(|l| l.len());
    push_check(
        &mut gate,
        "iss-descriptor",
        iss_roundtrip == Some(iss_assets),
        format!(
            "{} parse_iss count {iss_roundtrip:?} want {iss_assets}",
            iss_path.display()
        ),
    );
    if let Some(s) = &crop_stats {
        let frac = s.outside_fraction();
        push_check(
            &mut gate,
            "crop-bounds",
            frac < 0.01,
            format!(
                "{} of {} referenced verts outside {}-rect parcel union (fraction {:.5}, margin 0.051)",
                s.outside, s.referenced_verts, s.rects, frac
            ),
        );
        push_check(
            &mut gate,
            "crop-orphans",
            s.referenced_verts == s.buffer_verts,
            format!(
                "referenced {} of {} buffer verts",
                s.referenced_verts, s.buffer_verts
            ),
        );
    }
    let mut level_builds: Vec<LevelBuild> = Vec::new();
    for (level, staged_glb, sim, atlas_side) in staged {
        if let Some(cap) = effective_tri_cap(
            level,
            params.simplify_policy,
            params.tri_cap,
            params.tri_cap_auto,
            TRIS_PER_PARCEL * parcel_count as u64,
        ) {
            let c = tri_cap_check(cap, sim.tris_after, sim.unsimplified);
            push_check(&mut gate, format!("L{level}:{}", c.label), c.ok, c.detail);
        }
        let mut primary_path = PathBuf::new();
        let mut primary_bytes = 0usize;
        let gate_budget = atlas_side.map(|s| s.next_power_of_two().min(BUNDLE_TEXTURE_MAX));
        for plat in &platforms {
            let rel = expected_rel_path(&sid, level, plat);
            let path = PathBuf::from(&params.out_dir)
                .join(&conv.scene_id)
                .join(&rel);
            let data = std::fs::read(&path)
                .with_context(|| format!("read built bundle {}", path.display()))?;
            let checks = self_gate_bundle_with(
                &data,
                &sid,
                level,
                plat,
                true,
                gate_budget,
                params.fidelity,
            )?;
            for c in checks {
                push_check(
                    &mut gate,
                    format!("L{level}:{plat}:{}", c.label),
                    c.ok,
                    c.detail,
                );
            }
            push_check(
                &mut gate,
                format!("L{level}:{plat}:rel-path"),
                conv.results.iter().any(|r| r.rel_path == rel),
                rel.clone(),
            );
            if plat == &primary {
                primary_bytes = data.len();
                primary_path = path;
            }
        }
        let published = published.remove(&level);
        if level >= 1 && published.is_none() {
            bail!(
                "published GLB worker returned no result for scene {} level {level}",
                conv.scene_id
            );
        }
        if let Some((published, published_bytes, float_len)) = published {
            push_check(
                &mut gate,
                format!("L{level}:published-glb"),
                published_bytes > 0,
                format!("{} ({published_bytes} bytes)", published.display()),
            );
            log.push(format!(
                "published-glb[{level}]: {} ({published_bytes} bytes; float glb {} bytes)",
                published.display(),
                float_len
            ));
        }
        let glb_path = if params.keep_glb {
            Some(staged_glb)
        } else {
            let _ = std::fs::remove_file(&staged_glb);
            None
        };
        level_builds.push(LevelBuild {
            level,
            rel_path: expected_rel_path(&sid, level, &primary),
            bundle_path: primary_path,
            bundle_bytes: primary_bytes,
            simplify: sim,
            glb_path,
        });
    }
    if !params.keep_glb {
        let _ = std::fs::remove_file(&pre);
    }
    let finalize_ms = t_finalize.elapsed().as_millis();
    let io = client.io_stats();
    log.push(format!(
        "io: network_requests={} network_bytes={} cache_hits={} cache_bytes={}",
        io.network_requests, io.network_bytes, io.cache_hits, io.cache_bytes
    ));
    log.push(format!(
        "timing: placements_ms={placements_ms} assemble_ms={assemble_ms} atlas_ms={atlas_ms} emit_ms={emit_ms} simplify_ms={simplify_ms} bundle_ms={bundle_ms} package_ms={package_ms} finalize_ms={finalize_ms} total_ms={}",
        t_total.elapsed().as_millis()
    ));

    Ok(GenerateOutcome {
        entity_id: ent.entity_id.clone(),
        scene_id: conv.scene_id.clone(),
        source_tris,
        placement_stats,
        levels: level_builds,
        gate,
        log,
    })
}

#[cfg(test)]
mod placement_policy_tests {
    use super::*;

    fn manifest(count: usize) -> placements::ManifestPlacements {
        placements::ManifestPlacements {
            placements: vec![placements::Placement::default(); count],
            ..Default::default()
        }
    }

    fn scene(runtime: &str, main: Option<&str>) -> Scene {
        let mut metadata = serde_json::json!({"runtimeVersion": runtime});
        if let Some(main) = main {
            metadata["main"] = serde_json::Value::String(main.to_string());
        }
        Scene {
            entity_id: "scene".into(),
            entity_type: "scene".into(),
            pointers: Vec::new(),
            content: Vec::new(),
            metadata,
        }
    }

    #[test]
    fn clean_declarative_state_avoids_sdk_without_a_persistent_baseline() {
        assert!(placement_suspicion(&scene("7", None), &manifest(12)).is_empty());
        assert!(placement_suspicion(&scene("7", Some("main.crdt")), &manifest(12)).is_empty());
        assert_eq!(
            placement_suspicion(&scene("7", Some("bin/index.js")), &manifest(12)),
            ["executable scene entrypoint"]
        );
        assert_eq!(
            placement_suspicion(&scene("6", None), &manifest(12)),
            ["executable scene entrypoint"]
        );
    }

    #[test]
    fn incomplete_static_state_requests_sdk_execution() {
        let mut empty = manifest(0);
        assert_eq!(
            placement_suspicion(&scene("7", None), &empty),
            ["zero placements"]
        );

        empty.placements.push(Default::default());
        empty.skipped_mesh_renderer = 2;
        empty.unresolved_src = 1;
        assert_eq!(
            placement_suspicion(&scene("7", None), &empty),
            ["mesh-renderer components", "unresolved glTF sources"]
        );
    }

    #[test]
    fn authoritative_static_output_never_executes_sdk() {
        let result = finish_auto_placements(&scene("7", None), Ok(manifest(2)), || {
            panic!("SDK must not execute for authoritative declarative CRDT")
        })
        .unwrap();
        assert_eq!(result.0.len(), 2);
        assert_eq!(result.1, "static-crdt");
    }

    #[test]
    fn executable_and_invalid_static_scenes_use_sdk_output() {
        let result =
            finish_auto_placements(&scene("7", Some("bin/index.js")), Ok(manifest(2)), || {
                Ok(Some(manifest(3)))
            })
            .unwrap();
        assert_eq!(result.0.len(), 3);
        assert_eq!(result.1, "embedded-sdk");

        let result = finish_auto_placements(
            &scene("7", None),
            Err(anyhow!("truncated deployment CRDT")),
            || Ok(Some(manifest(1))),
        )
        .unwrap();
        assert_eq!(result.1, "embedded-sdk");
    }

    #[test]
    fn sdk_failure_empty_and_incomplete_results_are_rejected() {
        let executable = scene("7", Some("bin/index.js"));
        assert!(finish_auto_placements(&executable, Ok(manifest(1)), || {
            Err(anyhow!("runtime failed"))
        })
        .unwrap_err()
        .to_string()
        .contains("runtime failed"));
        assert!(
            finish_auto_placements(&executable, Ok(manifest(1)), || Ok(None))
                .unwrap_err()
                .to_string()
                .contains("no renderer state")
        );
        assert!(
            finish_auto_placements(&executable, Ok(manifest(1)), || { Ok(Some(manifest(0))) })
                .unwrap_err()
                .to_string()
                .contains("zero placements")
        );
        let mut incomplete = manifest(1);
        incomplete.unresolved_src = 1;
        assert!(
            finish_auto_placements(&executable, Ok(manifest(1)), || { Ok(Some(incomplete)) })
                .unwrap_err()
                .to_string()
                .contains("incomplete")
        );
        let mut runtime_primitives = manifest(1);
        runtime_primitives.skipped_mesh_renderer = 3;
        assert_eq!(
            finish_auto_placements(&executable, Ok(manifest(1)), || {
                Ok(Some(runtime_primitives))
            })
            .unwrap()
            .0
            .len(),
            1
        );
    }

    #[test]
    fn descriptor_input_cannot_authorize_generated_placements() {
        let client = CatalystClient::new("http://unused");
        let error = acquire_placements_independently(&client, &scene("7", None), "upstream.json")
            .unwrap_err()
            .to_string();
        assert!(error.contains("cannot supply generated placements"));
    }

    #[test]
    fn placement_metrics_identify_explorer_transform_outliers() {
        let placements = vec![
            placements::Placement::default(),
            placements::Placement {
                rotation: [0.0, 0.707, 0.0, 0.707],
                scale: [-2.0, 3.0, 200.0],
                ..Default::default()
            },
        ];
        let stats = placement_stats(&placements);
        assert_eq!(stats.count, 2);
        assert_eq!(stats.rotated, 1);
        assert_eq!(stats.non_uniform_scale, 1);
        assert_eq!(stats.mirrored, 1);
        assert_eq!(stats.extreme_scale, 1);
    }
}

#[cfg(not(target_arch = "wasm32"))]
use anyhow::Context;
use anyhow::{anyhow, bail, Result};
use std::collections::HashMap;
#[cfg(not(target_arch = "wasm32"))]
use std::path::Path;

use super::model::{LodModel, LodPrimitive};
use super::simplify_report::SimplifyReport;

const LOOSE_TARGET_ERROR: f32 = 1.0;
const TIGHT_TARGET_ERROR: f32 = 0.01;

/// gltfpack's `-si` default ratio and `-se` default error bound: the
/// production LOD-1 recipe (`gltfpack -si 0.1 -kn`).
pub const DEFAULT_SI_RATIO: f32 = 0.1;
pub const DEFAULT_SI_TARGET_ERROR: f32 = TIGHT_TARGET_ERROR;

/// How a decimation pass picks its triangle target.
///
/// `GltfpackSi` is the production policy: one topology-preserving
/// meshoptimizer pass per primitive at `-si ratio -se target_error`, no cap
/// and no sloppy fallback, so the output scales with the source exactly like
/// the lod-generator-unity `_1.glb`s. `Budget` is the parcel-scaled tri-cap
/// lane (`simplify_model`), selected by `--tri-cap`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SimplifyPolicy {
    GltfpackSi { ratio: f32, target_error: f32 },
    Budget,
}

impl Default for SimplifyPolicy {
    fn default() -> Self {
        SimplifyPolicy::GltfpackSi {
            ratio: DEFAULT_SI_RATIO,
            target_error: DEFAULT_SI_TARGET_ERROR,
        }
    }
}

impl SimplifyPolicy {
    pub fn name(self) -> &'static str {
        match self {
            SimplifyPolicy::GltfpackSi { .. } => "gltfpack-si",
            SimplifyPolicy::Budget => "budget",
        }
    }

    pub fn parse(s: &str) -> Result<SimplifyPolicy> {
        match s.trim().to_ascii_lowercase().as_str() {
            "gltfpack-si" | "gltfpack_si" | "si" => Ok(SimplifyPolicy::default()),
            "budget" => Ok(SimplifyPolicy::Budget),
            other => bail!("unknown simplify policy {other:?} (want gltfpack-si|budget)"),
        }
    }

    /// Same policy with the `-si` ratio replaced (no-op for `Budget`).
    pub fn with_ratio(self, ratio: f32) -> SimplifyPolicy {
        match self {
            SimplifyPolicy::GltfpackSi { target_error, .. } => SimplifyPolicy::GltfpackSi {
                ratio,
                target_error,
            },
            SimplifyPolicy::Budget => SimplifyPolicy::Budget,
        }
    }
}

/// gltfpack's `-si` index target: `ceil(index_count * ratio / 3) * 3`. The
/// ratio is snapped to seven decimals so an f32 `0.1` behaves as one tenth.
pub fn si_target_index_count(index_count: usize, ratio: f32) -> usize {
    let r = (ratio.clamp(0.0, 1.0) as f64 * 1e7).round() / 1e7;
    let tris = (index_count as f64 * r / 3.0 - 1e-9).ceil().max(0.0) as usize;
    (tris * 3).min(index_count)
}

pub fn apportion(counts: &[usize], cap: u64) -> Vec<u64> {
    let total: u64 = counts.iter().map(|&c| c as u64).sum();
    if total <= cap {
        return counts.iter().map(|&c| c as u64).collect();
    }
    let mut shares: Vec<u64> = Vec::with_capacity(counts.len());
    let mut remainders: Vec<(u128, usize)> = Vec::with_capacity(counts.len());
    let mut assigned: u64 = 0;
    for (i, &c) in counts.iter().enumerate() {
        let num = c as u128 * cap as u128;
        let share = (num / total as u128) as u64;
        shares.push(share);
        assigned += share;
        remainders.push((num % total as u128, i));
    }
    remainders.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut left = cap - assigned;
    for &(_, i) in &remainders {
        if left == 0 {
            break;
        }
        shares[i] += 1;
        left -= 1;
    }
    shares
}

fn simplify_prim(
    prim: &LodPrimitive,
    target_tris: u64,
    target_error: f32,
) -> Result<(LodPrimitive, bool)> {
    let target_indices = (target_tris as usize).saturating_mul(3);
    if prim.indices.len() <= target_indices {
        return Ok((prim.clone(), false));
    }
    let bytes = meshopt::typed_to_bytes(&prim.positions);
    let adapter = meshopt::VertexDataAdapter::new(bytes, 12, 0)
        .map_err(|e| anyhow!("meshopt vertex adapter: {e}"))?;
    let mut indices = meshopt::simplify(
        &prim.indices,
        &adapter,
        target_indices,
        target_error,
        meshopt::SimplifyOptions::empty(),
        None,
    );
    let mut sloppy = false;
    if indices.len() > target_indices {
        let alt =
            meshopt::simplify_sloppy(&prim.indices, &adapter, target_indices, target_error, None);
        if alt.len() < indices.len() {
            indices = alt;
            sloppy = true;
        }
    }
    let mut out = LodPrimitive {
        positions: prim.positions.clone(),
        normals: prim.normals.clone(),
        uvs: prim.uvs.clone(),
        tangents: prim.tangents.clone(),
        colors: prim.colors.clone(),
        indices,
        material: prim.material,
    };
    out.compact_orphans();
    Ok((out, sloppy))
}

fn run_pass(
    model: &LodModel,
    budget: u64,
    target_error: f32,
) -> Result<(Vec<LodPrimitive>, usize, bool)> {
    let counts: Vec<usize> = model
        .primitives
        .iter()
        .map(|p| p.indices.len() / 3)
        .collect();
    let targets = apportion(&counts, budget);
    let mut primitives = Vec::with_capacity(model.primitives.len());
    let mut sloppy_any = false;
    let mut total = 0usize;
    for (prim, &target) in model.primitives.iter().zip(targets.iter()) {
        let (p, sloppy) = simplify_prim(prim, target, target_error)?;
        sloppy_any |= sloppy;
        if !p.indices.is_empty() {
            total += p.indices.len() / 3;
            primitives.push(p);
        }
    }
    Ok((primitives, total, sloppy_any))
}

pub fn simplify_model(
    model: &LodModel,
    target_tris: u64,
    enforce_cap: bool,
) -> Result<(LodModel, SimplifyReport)> {
    let tris_before = model.total_tris();
    let mut ratios_run: Vec<f64> = Vec::new();
    ratios_run.push(if tris_before == 0 {
        1.0
    } else {
        target_tris as f64 / tris_before as f64
    });
    let (mut prims, mut total, mut sloppy) = run_pass(model, target_tris, LOOSE_TARGET_ERROR)?;
    if enforce_cap && total as u64 > target_tris {
        bail!(
            "meshopt simplify missed the tri cap: {total} tris > cap {target_tris} \
             (topology-preserving and sloppy passes exhausted)"
        );
    }
    if enforce_cap {
        let floor = (target_tris as f64 * 0.8) as usize;
        let (mut lo, mut hi) = (TIGHT_TARGET_ERROR, LOOSE_TARGET_ERROR);
        for _ in 0..6 {
            if total >= floor {
                break;
            }
            let mid = (lo + hi) / 2.0;
            let (p, t, s) = run_pass(model, target_tris, mid)?;
            ratios_run.push(mid as f64);
            if t as u64 <= target_tris {
                hi = mid;
                if t > total {
                    prims = p;
                    total = t;
                    sloppy = s;
                }
            } else {
                lo = mid;
            }
        }
    }
    let out = LodModel {
        root_name: model.root_name.clone(),
        primitives: prims,
        materials: model.materials.clone(),
        images: model.images.clone(),
        log: Vec::new(),
    };
    Ok((
        out,
        SimplifyReport {
            tris_before,
            tris_after: total,
            ratios_run,
            aggressive_final: sloppy,
            policy: "budget",
            ..Default::default()
        },
    ))
}

const MAX_GEOMETRIC_ERROR_METERS: f32 = 0.5;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct WeldKey {
    position: [u32; 3],
    uv: Option<[u32; 2]>,
    color: Option<[u32; 4]>,
}

fn bits3(v: [f32; 3]) -> [u32; 3] {
    v.map(f32::to_bits)
}

fn bits2(v: [f32; 2]) -> [u32; 2] {
    v.map(f32::to_bits)
}

fn bits4(v: [f32; 4]) -> [u32; 4] {
    v.map(f32::to_bits)
}

/// Match the reliable Node lane's topology preparation: shading-only normal
/// and tangent seams must not prevent geometric simplification. UV and color
/// seams remain real boundaries because welding across either corrupts pixels.
fn weld_for_simplification(prim: &LodPrimitive) -> LodPrimitive {
    let has_uvs = prim.uvs.len() == prim.positions.len();
    let has_colors = prim.colors.len() == prim.positions.len();
    let mut vertices: HashMap<WeldKey, u32> = HashMap::new();
    let mut positions = Vec::new();
    let mut uvs = Vec::new();
    let mut colors = Vec::new();
    let mut remap = vec![0u32; prim.positions.len()];

    for i in 0..prim.positions.len() {
        let key = WeldKey {
            position: bits3(prim.positions[i]),
            uv: has_uvs.then(|| bits2(prim.uvs[i])),
            color: has_colors.then(|| bits4(prim.colors[i])),
        };
        let next = positions.len() as u32;
        let index = *vertices.entry(key).or_insert_with(|| {
            positions.push(prim.positions[i]);
            if has_uvs {
                uvs.push(prim.uvs[i]);
            }
            if has_colors {
                colors.push(prim.colors[i]);
            }
            next
        });
        remap[i] = index;
    }

    LodPrimitive {
        positions,
        normals: Vec::new(),
        uvs,
        tangents: Vec::new(),
        colors,
        indices: prim.indices.iter().map(|&i| remap[i as usize]).collect(),
        material: prim.material,
    }
}

fn positions_radius(mut positions: impl Iterator<Item = [f32; 3]>) -> f32 {
    let Some(mut lo) = positions.next() else {
        return 0.0;
    };
    let mut hi = lo;
    for p in positions {
        for axis in 0..3 {
            lo[axis] = lo[axis].min(p[axis]);
            hi[axis] = hi[axis].max(p[axis]);
        }
    }
    let d = [hi[0] - lo[0], hi[1] - lo[1], hi[2] - lo[2]];
    0.5 * (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
}

fn bbox_radius(positions: &[[f32; 3]]) -> f32 {
    positions_radius(positions.iter().copied())
}

fn recompute_smooth_normals(prim: &mut LodPrimitive) {
    let mut normals = vec![[0.0f32; 3]; prim.positions.len()];
    for tri in prim.indices.chunks_exact(3) {
        let a = prim.positions[tri[0] as usize];
        let b = prim.positions[tri[1] as usize];
        let c = prim.positions[tri[2] as usize];
        let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let ac = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
        let n = [
            ab[1] * ac[2] - ab[2] * ac[1],
            ab[2] * ac[0] - ab[0] * ac[2],
            ab[0] * ac[1] - ab[1] * ac[0],
        ];
        for &index in tri {
            let dst = &mut normals[index as usize];
            for axis in 0..3 {
                dst[axis] += n[axis];
            }
        }
    }
    for n in &mut normals {
        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
        if len > 1e-12 {
            *n = [n[0] / len, n[1] / len, n[2] / len];
        } else {
            *n = [0.0, 1.0, 0.0];
        }
    }
    prim.normals = normals;
}

fn simplify_prim_si(
    prim: &LodPrimitive,
    ratio: f32,
    target_error: f32,
    scene_radius: f32,
) -> Result<LodPrimitive> {
    let target_indices = si_target_index_count(prim.indices.len(), ratio);
    if prim.indices.len() <= target_indices {
        return Ok(prim.clone());
    }

    let prepared = weld_for_simplification(prim);
    let primitive_radius = bbox_radius(&prepared.positions).max(1e-6);
    let error_meters = (target_error * scene_radius).min(MAX_GEOMETRIC_ERROR_METERS);
    let relative_error = (error_meters / primitive_radius).clamp(0.0, 1.0);
    let bytes = meshopt::typed_to_bytes(&prepared.positions);
    let adapter = meshopt::VertexDataAdapter::new(bytes, 12, 0)
        .map_err(|e| anyhow!("meshopt vertex adapter: {e}"))?;
    let indices = meshopt::simplify(
        &prepared.indices,
        &adapter,
        target_indices,
        relative_error,
        meshopt::SimplifyOptions::empty(),
        None,
    );
    let mut out = LodPrimitive {
        indices,
        ..prepared
    };
    out.compact_orphans();
    recompute_smooth_normals(&mut out);
    Ok(out)
}

/// The `SimplifyPolicy::GltfpackSi` pass: every primitive gets exactly one
/// topology-preserving pass toward `ceil(tris * ratio)` bounded by
/// `target_error` (relative to the primitive extent), the way
/// `gltfpack -si <ratio> -se <target_error>` treats each mesh. The error
/// bound wins over the count, there is no sloppy retry and no cap, so the
/// result may land above the ratio; primitives that empty out are dropped.
pub fn simplify_model_si(
    model: &LodModel,
    ratio: f32,
    target_error: f32,
) -> Result<(LodModel, SimplifyReport)> {
    let tris_before = model.total_tris();
    let scene_radius = positions_radius(
        model
            .primitives
            .iter()
            .flat_map(|p| p.positions.iter().copied()),
    )
    .max(1e-6);
    let mut primitives = Vec::with_capacity(model.primitives.len());
    let mut total = 0usize;
    for prim in &model.primitives {
        let p = simplify_prim_si(prim, ratio, target_error, scene_radius)?;
        if !p.indices.is_empty() {
            total += p.indices.len() / 3;
            primitives.push(p);
        }
    }
    let out = LodModel {
        root_name: model.root_name.clone(),
        primitives,
        materials: model.materials.clone(),
        images: model.images.clone(),
        log: Vec::new(),
    };
    Ok((
        out,
        SimplifyReport {
            tris_before,
            tris_after: total,
            ratios_run: vec![ratio as f64],
            policy: "gltfpack-si",
            ..Default::default()
        },
    ))
}

#[cfg(not(target_arch = "wasm32"))]
pub fn simplify_file_si(
    input: &Path,
    output: &Path,
    ratio: f32,
    target_error: f32,
) -> Result<SimplifyReport> {
    let bytes = std::fs::read(input).with_context(|| format!("read {}", input.display()))?;
    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("lod")
        .to_string();
    let model = super::model::from_glb_bytes(&bytes, &stem)
        .with_context(|| format!("parse {}", input.display()))?;
    if ratio >= 1.0 {
        let tris = model.total_tris();
        if input != output {
            std::fs::copy(input, output)
                .with_context(|| format!("copy {} -> {}", input.display(), output.display()))?;
        }
        return Ok(SimplifyReport {
            tris_before: tris,
            tris_after: tris,
            passthrough: true,
            policy: "gltfpack-si",
            ..Default::default()
        });
    }
    let (out_model, report) = simplify_model_si(&model, ratio, target_error)?;
    let glb = super::emit::emit_glb(&out_model)?;
    std::fs::write(output, &glb).with_context(|| format!("write {}", output.display()))?;
    Ok(report)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn simplify_file(
    input: &Path,
    output: &Path,
    ratio: f64,
    tri_cap: Option<u64>,
) -> Result<SimplifyReport> {
    let bytes = std::fs::read(input).with_context(|| format!("read {}", input.display()))?;
    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("lod")
        .to_string();
    let model = super::model::from_glb_bytes(&bytes, &stem)
        .with_context(|| format!("parse {}", input.display()))?;
    let tris = model.total_tris();
    let under_cap = tri_cap.is_none_or(|c| tris as u64 <= c);
    if ratio >= 1.0 && under_cap {
        if input != output {
            std::fs::copy(input, output)
                .with_context(|| format!("copy {} -> {}", input.display(), output.display()))?;
        }
        return Ok(SimplifyReport {
            tris_before: tris,
            tris_after: tris,
            passthrough: true,
            ..Default::default()
        });
    }
    let ratio_target = (tris as f64 * ratio.clamp(0.0, 1.0)).round() as u64;
    let (target, enforce) = match tri_cap {
        Some(cap) if (tris as u64) > cap => (cap, true),
        Some(cap) => (ratio_target.min(cap), true),
        None => (ratio_target, false),
    };
    let (out_model, report) = simplify_model(&model, target, enforce)?;
    let glb = super::emit::emit_glb(&out_model)?;
    std::fs::write(output, &glb).with_context(|| format!("write {}", output.display()))?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lodgen::emit::emit_glb;
    use crate::lodgen::model::{from_glb_bytes, AlphaClass, LodMaterial};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "abgen-lod-meshopt-test-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn grid_model(n: u32) -> LodModel {
        let mut positions = Vec::new();
        let mut normals = Vec::new();
        let mut uvs = Vec::new();
        for j in 0..=n {
            for i in 0..=n {
                let x = i as f32 / n as f32;
                let z = j as f32 / n as f32;
                let y = 0.05
                    * ((x * 12.0).sin() + (z * 12.0).cos())
                    * (1.0 + 0.3 * ((x * 5.0 + z * 7.0).sin()));
                positions.push([x * 10.0, y, z * 10.0]);
                normals.push([0.0, 1.0, 0.0]);
                uvs.push([x, z]);
            }
        }
        let mut indices = Vec::new();
        for j in 0..n {
            for i in 0..n {
                let a = j * (n + 1) + i;
                let b = a + 1;
                let c = a + n + 1;
                let d = c + 1;
                indices.extend_from_slice(&[a, c, b, b, c, d]);
            }
        }
        LodModel {
            root_name: "grid".to_string(),
            primitives: vec![LodPrimitive {
                positions,
                normals,
                uvs,
                indices,
                material: 0,
                ..Default::default()
            }],
            materials: vec![LodMaterial {
                name: "m".to_string(),
                class: AlphaClass::Opaque,
                base_color: [1.0, 1.0, 1.0, 1.0],
                cutoff: 0.5,
                image: None,
                double_sided: false,
                ..Default::default()
            }],
            images: Vec::new(),
            log: Vec::new(),
        }
    }

    #[test]
    fn apportion_is_exact_and_proportional() {
        assert_eq!(apportion(&[800, 150, 50], 500), vec![400, 75, 25]);
        let shares = apportion(&[700, 200, 100], 501);
        assert_eq!(shares.iter().sum::<u64>(), 501);
        assert_eq!(shares, vec![351, 100, 50]);
        assert_eq!(apportion(&[100, 200], 400), vec![100, 200]);
        assert_eq!(apportion(&[0, 300], 100), vec![0, 100]);
        assert_eq!(apportion(&[3, 3, 3], 7), vec![3, 2, 2]);
        assert_eq!(apportion(&[], 100), Vec::<u64>::new());
    }

    #[test]
    fn capped_grid_lands_in_the_prod_window() {
        let model = grid_model(32);
        assert_eq!(model.total_tris(), 2048);
        let (out, report) = simplify_model(&model, 500, true).unwrap();
        assert_eq!(report.tris_before, 2048);
        assert_eq!(report.tris_after, out.total_tris());
        assert!(report.tris_after <= 500, "{}", report.tris_after);
        assert!(report.tris_after >= 400, "{}", report.tris_after);
        assert!(!report.passthrough);
        assert!(!report.unsimplified);
        assert_eq!(report.ratios_run.len(), 1);
        let orphans = out
            .primitives
            .iter()
            .map(|p| {
                let mut q = p.clone();
                q.compact_orphans()
            })
            .sum::<usize>();
        assert_eq!(orphans, 0);
    }

    #[test]
    fn simplify_is_deterministic_at_the_byte_level() {
        let model = grid_model(32);
        let (a, _) = simplify_model(&model, 500, true).unwrap();
        let (b, _) = simplify_model(&model, 500, true).unwrap();
        assert_eq!(emit_glb(&a).unwrap(), emit_glb(&b).unwrap());
    }

    #[test]
    fn zero_target_drops_the_primitive_via_sloppy_fallback() {
        let model = grid_model(8);
        let (out, report) = simplify_model(&model, 0, true).unwrap();
        assert_eq!(report.tris_after, 0);
        assert!(out.primitives.is_empty());
        assert_eq!(out.materials.len(), 1);
    }

    #[test]
    fn under_target_prims_pass_through_untouched() {
        let model = grid_model(8);
        let (out, report) = simplify_model(&model, 10_000, false).unwrap();
        assert_eq!(report.tris_before, report.tris_after);
        assert_eq!(out.primitives[0].indices, model.primitives[0].indices);
        assert!(!report.aggressive_final);
    }

    #[test]
    fn si_policy_parse_names_and_default() {
        assert_eq!(
            SimplifyPolicy::default(),
            SimplifyPolicy::GltfpackSi {
                ratio: 0.1,
                target_error: 0.01
            }
        );
        assert_eq!(
            SimplifyPolicy::parse(" Gltfpack-SI ").unwrap(),
            SimplifyPolicy::default()
        );
        assert_eq!(
            SimplifyPolicy::parse("budget").unwrap(),
            SimplifyPolicy::Budget
        );
        let msg = format!("{:#}", SimplifyPolicy::parse("pixyz").unwrap_err());
        assert!(msg.contains("gltfpack-si|budget"), "{msg}");
        assert_eq!(SimplifyPolicy::default().name(), "gltfpack-si");
        assert_eq!(SimplifyPolicy::Budget.name(), "budget");
        assert_eq!(
            SimplifyPolicy::default().with_ratio(0.25),
            SimplifyPolicy::GltfpackSi {
                ratio: 0.25,
                target_error: 0.01
            }
        );
        assert_eq!(
            SimplifyPolicy::Budget.with_ratio(0.25),
            SimplifyPolicy::Budget
        );
        assert_eq!(si_target_index_count(6144, 0.1), 615);
        assert_eq!(si_target_index_count(30, 0.1), 3);
        assert_eq!(si_target_index_count(33, 0.1), 6);
        assert_eq!(si_target_index_count(30, 1.0), 30);
        assert_eq!(si_target_index_count(0, 0.1), 0);
    }

    #[test]
    fn si_policy_scales_with_the_source_and_never_caps() {
        let model = grid_model(32);
        assert_eq!(model.total_tris(), 2048);
        let (out, report) = simplify_model_si(&model, 0.1, 0.01).unwrap();
        assert_eq!(report.policy, "gltfpack-si");
        assert_eq!(report.tris_before, 2048);
        assert_eq!(report.tris_after, out.total_tris());
        assert!(!report.passthrough && !report.unsimplified && !report.aggressive_final);
        // target is ceil(2048 x 0.1) = 205 tris; meshopt may land a few under it
        assert!(report.tris_after >= 190, "{}", report.tris_after);
        assert!(report.tris_after <= 2048 / 4, "{}", report.tris_after);
        let (big, big_report) = simplify_model_si(&grid_model(64), 0.1, 0.01).unwrap();
        assert!(
            big_report.tris_after > report.tris_after,
            "{}",
            big_report.tris_after
        );
        assert_eq!(big_report.tris_after, big.total_tris());
        let (same, pass) = simplify_model_si(&model, 1.0, 0.01).unwrap();
        assert_eq!(pass.tris_after, 2048);
        assert_eq!(same.primitives[0].indices, model.primitives[0].indices);
        let (a, _) = simplify_model_si(&model, 0.1, 0.01).unwrap();
        assert_eq!(emit_glb(&a).unwrap(), emit_glb(&out).unwrap());
    }

    #[test]
    fn file_lane_si_policy_roundtrip() {
        let dir = temp_dir("file-si");
        let glb = emit_glb(&grid_model(32)).unwrap();
        let input = dir.join("in.glb");
        let output = dir.join("out.glb");
        std::fs::write(&input, &glb).unwrap();
        let report = simplify_file_si(&input, &output, 1.0, 0.01).unwrap();
        assert!(report.passthrough);
        assert_eq!(std::fs::read(&output).unwrap(), glb);
        let report = simplify_file_si(&input, &output, 0.1, 0.01).unwrap();
        assert_eq!(report.policy, "gltfpack-si");
        let back = from_glb_bytes(&std::fs::read(&output).unwrap(), "grid").unwrap();
        assert_eq!(back.total_tris(), report.tris_after);
        assert!(
            report.summary().contains("policy gltfpack-si"),
            "{}",
            report.summary()
        );
    }

    #[test]
    fn file_lane_passthrough_and_capped_window() {
        let dir = temp_dir("file");
        let glb = emit_glb(&grid_model(32)).unwrap();
        let input = dir.join("in.glb");
        let output = dir.join("out.glb");
        std::fs::write(&input, &glb).unwrap();

        let report = simplify_file(&input, &output, 1.0, Some(1_000_000)).unwrap();
        assert!(report.passthrough);
        assert_eq!(report.tris_before, 2048);
        assert_eq!(std::fs::read(&output).unwrap(), glb);

        let report = simplify_file(&input, &output, 1.0, Some(500)).unwrap();
        assert!(!report.passthrough);
        assert!(report.tris_after <= 500 && report.tris_after >= 400);
        let back = from_glb_bytes(&std::fs::read(&output).unwrap(), "grid").unwrap();
        assert_eq!(back.total_tris(), report.tris_after);

        let report = simplify_file(&input, &output, 0.25, None).unwrap();
        assert!(report.tris_after <= 512 && report.tris_after >= 400);
    }

    #[test]
    fn shading_seams_do_not_lock_simplification_topology() {
        let prim = LodPrimitive {
            positions: vec![
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0],
                [1.0, 0.0, 0.0],
                [1.0, 0.0, 1.0],
                [0.0, 0.0, 1.0],
            ],
            normals: vec![
                [0.0, 1.0, 0.0],
                [0.0, 1.0, 0.0],
                [0.0, 1.0, 0.0],
                [1.0, 0.0, 0.0],
                [1.0, 0.0, 0.0],
                [1.0, 0.0, 0.0],
            ],
            uvs: vec![
                [0.0, 0.0],
                [1.0, 0.0],
                [0.0, 1.0],
                [1.0, 0.0],
                [1.0, 1.0],
                [0.0, 1.0],
            ],
            indices: vec![0, 1, 2, 3, 4, 5],
            ..Default::default()
        };
        let mut welded = weld_for_simplification(&prim);
        assert_eq!(welded.positions.len(), 4);
        assert_eq!(welded.indices, [0, 1, 2, 1, 3, 2]);
        assert!(welded.normals.is_empty() && welded.tangents.is_empty());
        recompute_smooth_normals(&mut welded);
        assert!(welded.normals.iter().all(|n| (n[1] + 1.0).abs() < 1e-6));
    }
}

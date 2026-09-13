use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

use super::primitives::{self, MaterialFields, PrimitivePlacement, PrimitiveSpec};

#[cfg(not(target_arch = "wasm32"))]
#[path = "placements_native.rs"]
mod placements_native;
#[cfg(not(target_arch = "wasm32"))]
pub use placements_native::*;

pub const ISS_MANIFEST_BASE: &str =
    "https://lod-generator-unity-cdn.decentraland.org/lods-unity/manifests";
pub const ISS_SUFFIX: &str = "_InitialSceneState.json";

/// GltfContainer srcs the production ManifestParser drops before grouping
/// (`ManifestParser.ExcludedGltfSrcs`, compared case-insensitively on the
/// whole path): the Genesis Plaza live-events boards are animated at runtime,
/// so a baked copy would sit as a stale duplicate under the live board.
pub const EXCLUDED_GLTF_SRCS: [&str; 2] = [
    "assets/models/out/models/live_events.glb",
    "assets/models/out/models/next_live_events.glb",
];

const IDENTITY_ROTATION: [f64; 4] = [0.0, 0.0, 0.0, 1.0];

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Placement {
    pub glb_hash: Option<String>,
    pub glb_file: Option<String>,
    pub position: [f64; 3],
    pub rotation: [f64; 4],
    pub scale: [f64; 3],
}

impl Default for Placement {
    fn default() -> Self {
        Placement {
            glb_hash: None,
            glb_file: None,
            position: [0.0; 3],
            rotation: IDENTITY_ROTATION,
            scale: [1.0; 3],
        }
    }
}

fn cmp_f64s(a: &[f64], b: &[f64]) -> std::cmp::Ordering {
    for (x, y) in a.iter().zip(b.iter()) {
        let o = x.total_cmp(y);
        if o != std::cmp::Ordering::Equal {
            return o;
        }
    }
    std::cmp::Ordering::Equal
}

/// Canonical ordering for listings that need to be order-independent. Both
/// placement lanes and `parse_iss` keep the descriptor order (first-seen gltf
/// src, then entity insertion order); a placement list is a multiset and
/// `diff_iss` compares it as one.
pub fn sort_placements(list: &mut [Placement]) {
    list.sort_by(|a, b| {
        (a.glb_hash.as_deref(), a.glb_file.as_deref())
            .cmp(&(b.glb_hash.as_deref(), b.glb_file.as_deref()))
            .then_with(|| cmp_f64s(&a.position, &b.position))
            .then_with(|| cmp_f64s(&a.rotation, &b.rotation))
            .then_with(|| cmp_f64s(&a.scale, &b.scale))
    });
}

fn num_or(v: Option<&serde_json::Value>, default: f64) -> f64 {
    v.and_then(|x| x.as_f64()).unwrap_or(default)
}

fn vec3_or(v: Option<&serde_json::Value>, default: [f64; 3]) -> [f64; 3] {
    match v {
        Some(m) => [
            num_or(m.get("x"), default[0]),
            num_or(m.get("y"), default[1]),
            num_or(m.get("z"), default[2]),
        ],
        None => default,
    }
}

fn quat_or_identity(v: Option<&serde_json::Value>) -> [f64; 4] {
    match v {
        Some(m) => [
            num_or(m.get("x"), 0.0),
            num_or(m.get("y"), 0.0),
            num_or(m.get("z"), 0.0),
            num_or(m.get("w"), 1.0),
        ],
        None => IDENTITY_ROTATION,
    }
}

/// Reads a `{sceneId}_InitialSceneState.json` descriptor in file order.
pub fn parse_iss(bytes: &[u8]) -> Result<Vec<Placement>> {
    let v: serde_json::Value =
        serde_json::from_slice(bytes).context("ISS descriptor is not JSON")?;
    let assets = v
        .get("assets")
        .and_then(|a| a.as_array())
        .ok_or_else(|| anyhow!("ISS descriptor has no assets array"))?;
    let mut out = Vec::new();
    for a in assets {
        let Some(hash) = a.get("hash").and_then(|h| h.as_str()) else {
            continue;
        };
        out.push(Placement {
            glb_hash: Some(hash.to_string()),
            glb_file: None,
            position: vec3_or(a.get("position"), [0.0; 3]),
            rotation: quat_or_identity(a.get("rotation")),
            scale: vec3_or(a.get("scale"), [1.0; 3]),
        });
    }
    Ok(out)
}

pub fn iss_descriptor(scene_id: &str, placements: &[(String, &Placement)]) -> serde_json::Value {
    let assets: Vec<serde_json::Value> = placements
        .iter()
        .map(|(hash, p)| {
            serde_json::json!({
                "hash": hash,
                "position": {"x": p.position[0], "y": p.position[1], "z": p.position[2]},
                "rotation": {"x": p.rotation[0], "y": p.rotation[1], "z": p.rotation[2], "w": p.rotation[3]},
                "scale": {"x": p.scale[0], "y": p.scale[1], "z": p.scale[2]},
            })
        })
        .collect();
    serde_json::json!({
        "version": 1,
        "sceneId": scene_id,
        "assets": assets,
    })
}

#[derive(Clone, Debug)]
pub(crate) struct Trs {
    pub(crate) position: [f64; 3],
    pub(crate) rotation: [f64; 4],
    pub(crate) scale: [f64; 3],
    pub(crate) parent: i64,
}

impl Default for Trs {
    fn default() -> Self {
        Trs {
            position: [0.0; 3],
            rotation: IDENTITY_ROTATION,
            scale: [1.0; 3],
            parent: 0,
        }
    }
}

/// Row-major 4x4 (`m[row][col]`), column 3 = translation, like `Matrix4x4`.
type Mat4 = [[f64; 4]; 4];

const MAT4_IDENTITY: Mat4 = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
];

/// `Matrix4x4.TRS(position, rotation, scale)`: the quaternion is expanded as
/// given (Unity does not renormalise it), each rotation column is scaled by
/// its axis, column 3 carries the translation.
fn mat_trs(t: &Trs) -> Mat4 {
    let [x, y, z, w] = t.rotation;
    let (xx, yy, zz) = (x * x, y * y, z * z);
    let (xy, xz, yz) = (x * y, x * z, y * z);
    let (wx, wy, wz) = (w * x, w * y, w * z);
    let r = [
        [1.0 - 2.0 * (yy + zz), 2.0 * (xy - wz), 2.0 * (xz + wy)],
        [2.0 * (xy + wz), 1.0 - 2.0 * (xx + zz), 2.0 * (yz - wx)],
        [2.0 * (xz - wy), 2.0 * (yz + wx), 1.0 - 2.0 * (xx + yy)],
    ];
    let mut m = MAT4_IDENTITY;
    for row in 0..3 {
        for col in 0..3 {
            m[row][col] = r[row][col] * t.scale[col];
        }
        m[row][3] = t.position[row];
    }
    m
}

fn mat_mul(a: &Mat4, b: &Mat4) -> Mat4 {
    let mut out = [[0.0; 4]; 4];
    for (row, out_row) in out.iter_mut().enumerate() {
        for (col, cell) in out_row.iter_mut().enumerate() {
            *cell = (0..4).map(|k| a[row][k] * b[k][col]).sum();
        }
    }
    out
}

/// `TransformResolver.ResolveWorldTransform`: `world(parent) * local`; an
/// entity without a Transform is the identity (so a child of an unknown
/// parent is placed by its own local TRS), `parent == 0` and `parent == self`
/// end the chain. Production recurses forever on a longer cycle; here the
/// entity that closes the cycle is treated as a root.
fn world_matrix(eid: i64, transforms: &HashMap<i64, Trs>, visiting: &mut HashSet<i64>) -> Mat4 {
    let Some(local) = transforms.get(&eid) else {
        return MAT4_IDENTITY;
    };
    let m = mat_trs(local);
    if local.parent == 0 || local.parent == eid || !visiting.insert(eid) {
        return m;
    }
    let parent = world_matrix(local.parent, transforms, visiting);
    visiting.remove(&eid);
    mat_mul(&parent, &m)
}

/// Unity's `MatrixToQuaternion` (`Matrix4x4.rotation` on an orthonormal 3x3,
/// `r[row][col]`): with a positive trace `w = sqrt(1 + trace) / 2` is
/// positive; otherwise the quaternion component of the largest diagonal
/// element is positive and `w` takes whatever sign the off-diagonals give it,
/// so a production descriptor legitimately carries `w < 0` rotations.
pub(crate) fn matrix_to_quaternion(r: &[[f64; 3]; 3]) -> [f64; 4] {
    let trace = r[0][0] + r[1][1] + r[2][2];
    let mut q = [0.0f64; 4];
    if trace > 0.0 {
        let mut root = (trace + 1.0).sqrt();
        q[3] = 0.5 * root;
        root = 0.5 / root;
        q[0] = (r[2][1] - r[1][2]) * root;
        q[1] = (r[0][2] - r[2][0]) * root;
        q[2] = (r[1][0] - r[0][1]) * root;
    } else {
        const NEXT: [usize; 3] = [1, 2, 0];
        let mut i = 0;
        if r[1][1] > r[0][0] {
            i = 1;
        }
        if r[2][2] > r[i][i] {
            i = 2;
        }
        let j = NEXT[i];
        let k = NEXT[j];
        let mut root = (r[i][i] - r[j][j] - r[k][k] + 1.0).sqrt();
        q[i] = 0.5 * root;
        root = 0.5 / root;
        q[3] = (r[k][j] - r[j][k]) * root;
        q[j] = (r[j][i] + r[i][j]) * root;
        q[k] = (r[k][i] + r[i][k]) * root;
    }
    let n = q.iter().map(|v| v * v).sum::<f64>().sqrt();
    q.map(|v| v / n)
}

struct Decomposed {
    position: [f64; 3],
    rotation: [f64; 4],
    scale: [f64; 3],
}

/// `TransformResolver.Decompose` + the NaN guard of
/// `StaticSceneDescriptorBuilder.Build`: scale = column magnitudes, `scale.x`
/// negated when the determinant is negative (an odd number of reflections),
/// rotation from the columns divided by those signed scales; a zero scale
/// axis makes the rotation NaN, which becomes the identity while the scale is
/// kept as measured.
fn decompose_unity(m: &Mat4) -> Decomposed {
    let col = |c: usize| [m[0][c], m[1][c], m[2][c]];
    let c = [col(0), col(1), col(2)];
    let len = |v: [f64; 3]| (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    let mut scale = [len(c[0]), len(c[1]), len(c[2])];
    let cross = [
        c[1][1] * c[2][2] - c[1][2] * c[2][1],
        c[1][2] * c[2][0] - c[1][0] * c[2][2],
        c[1][0] * c[2][1] - c[1][1] * c[2][0],
    ];
    let det = c[0][0] * cross[0] + c[0][1] * cross[1] + c[0][2] * cross[2];
    if det < 0.0 {
        scale[0] = -scale[0];
    }
    let mut r = [[0.0; 3]; 3];
    for (row, r_row) in r.iter_mut().enumerate() {
        for (axis, cell) in r_row.iter_mut().enumerate() {
            *cell = c[axis][row] / scale[axis];
        }
    }
    let mut rotation = matrix_to_quaternion(&r);
    if rotation.iter().any(|v| v.is_nan()) {
        rotation = IDENTITY_ROTATION;
    }
    Decomposed {
        position: col(3),
        rotation,
        scale,
    }
}

fn scrub_negative_zero<const N: usize>(v: [f64; N]) -> [f64; N] {
    v.map(|x| if x == 0.0 { 0.0 } else { x })
}

pub fn gltf_src_is_excluded(src: &str) -> bool {
    let lower = src.to_lowercase();
    EXCLUDED_GLTF_SRCS.contains(&lower.as_str())
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ManifestPlacements {
    /// Descriptor order: first-seen gltf src, then entity insertion order.
    pub placements: Vec<Placement>,
    /// SDK7 primitives (`core::MeshRenderer` box/sphere/plane/cylinder with their
    /// `core::Material`), in entity insertion order. Production's descriptor never
    /// carried these — the Unity pipeline ignored them — so they are kept apart from
    /// `placements`, which stays comparable to the production ISS.
    pub primitives: Vec<PrimitivePlacement>,
    /// Entities carrying a MeshRenderer at all (placed or not).
    pub mesh_renderers: usize,
    /// MeshRenderer entities that produced no primitive: unset/unsupported mesh
    /// (a DELETE or empty payload) or the entity's own `visible == false`.
    pub skipped_mesh_renderer: usize,
    /// Primitive materials whose texture names a scene file the entity does not
    /// ship; those primitives keep their colour only.
    pub missing_textures: usize,
    /// Distinct gltf srcs with no content hash; every placement of such a
    /// src is dropped (`StaticSceneDescriptorBuilder.missingHashes`).
    pub unresolved_src: usize,
    /// The srcs behind `unresolved_src`, as the scene wrote them, in
    /// first-seen order — so a failure names what the deployment lacks.
    pub unresolved_srcs: Vec<String>,
    /// Placements dropped because the entity's own VisibilityComponent says
    /// `visible == false` (never inherited from a parent).
    pub invisible_skipped: usize,
    /// Placements dropped by `EXCLUDED_GLTF_SRCS`.
    pub excluded_src: usize,
}

/// `StaticSceneDescriptorBuilder.Build` over the LWW-folded components of
/// one scene, plus the primitives production never read. `gltf_srcs` holds
/// one winning src per entity in entity insertion order; `mesh_renderers`
/// one winning MeshRenderer per entity (`None` when it names no shape);
/// `materials` the winning Material per entity; `visibility` each entity's
/// own `visible` flag.
pub(crate) fn placements_from_components(
    transforms: HashMap<i64, Trs>,
    gltf_srcs: Vec<(i64, String)>,
    mesh_renderers: Vec<(i64, Option<PrimitiveSpec>)>,
    materials: HashMap<i64, MaterialFields>,
    visibility: HashMap<i64, bool>,
    content_by_file: &HashMap<String, String>,
) -> ManifestPlacements {
    let lowered: HashMap<String, &String> = content_by_file
        .iter()
        .map(|(k, v)| (k.to_lowercase(), v))
        .collect();
    let mut out = ManifestPlacements {
        mesh_renderers: mesh_renderers.len(),
        ..Default::default()
    };
    let mut groups: Vec<(String, Vec<i64>)> = Vec::new();
    let mut group_index: HashMap<String, usize> = HashMap::new();
    for (eid, src) in gltf_srcs {
        if src.is_empty() {
            continue;
        }
        if gltf_src_is_excluded(&src) {
            out.excluded_src += 1;
            continue;
        }
        match group_index.get(&src) {
            Some(&i) => groups[i].1.push(eid),
            None => {
                group_index.insert(src.clone(), groups.len());
                groups.push((src, vec![eid]));
            }
        }
    }
    for (src, entities) in groups {
        let Some(hash) = lowered.get(&src.to_lowercase()) else {
            out.unresolved_src += 1;
            out.unresolved_srcs.push(src);
            continue;
        };
        for eid in entities {
            if visibility.get(&eid) == Some(&false) {
                out.invisible_skipped += 1;
                continue;
            }
            let world = world_matrix(eid, &transforms, &mut HashSet::new());
            let d = decompose_unity(&world);
            out.placements.push(Placement {
                glb_hash: Some((*hash).clone()),
                glb_file: Some(src.clone()),
                position: scrub_negative_zero(d.position),
                rotation: scrub_negative_zero(d.rotation),
                scale: scrub_negative_zero(d.scale),
            });
        }
    }
    // Primitives: the explorer renders a MeshRenderer on its own entity whether or
    // not a GltfContainer sits there too, so every visible shape is placed. The
    // same transform chain and Unity decomposition as the GLBs apply.
    for (eid, spec) in mesh_renderers {
        let Some(spec) = spec else {
            out.skipped_mesh_renderer += 1;
            continue;
        };
        if visibility.get(&eid) == Some(&false) {
            out.skipped_mesh_renderer += 1;
            continue;
        }
        let (material, missing) = primitives::resolve_material(materials.get(&eid), &lowered);
        if missing {
            out.missing_textures += 1;
        }
        let world = world_matrix(eid, &transforms, &mut HashSet::new());
        let d = decompose_unity(&world);
        out.primitives.push(PrimitivePlacement {
            spec,
            material,
            position: scrub_negative_zero(d.position),
            rotation: scrub_negative_zero(d.rotation),
            scale: scrub_negative_zero(d.scale),
        });
    }
    out
}

/// `ManifestParser.Parse` over a `<sceneId>-lod-manifest.json`: rows are
/// folded per (entity, component) — by `timestamp` when rows carry one
/// (`>=` wins), otherwise the last row wins — and rows without an object
/// `data` (DELETEs) are ignored. Transform, GltfContainer and
/// VisibilityComponent feed the descriptor exactly as production reads them;
/// MeshRenderer and Material rows, which production dropped, feed the
/// primitives (a MeshRenderer row whose `data` is not an object still counts
/// the entity, as a skipped one). A VisibilityComponent row without a
/// `visible` field reads as `false`, exactly as `JsonUtility` defaults it.
pub fn parse_lod_manifest_full(
    bytes: &[u8],
    content_by_file: &HashMap<String, String>,
) -> Result<ManifestPlacements> {
    let v: serde_json::Value = serde_json::from_slice(bytes).context("lod manifest is not JSON")?;
    let rows = v
        .as_array()
        .ok_or_else(|| anyhow!("lod manifest is not a JSON array"))?;
    struct Cell<'a> {
        eid: i64,
        name: &'a str,
        timestamp: Option<f64>,
        data: &'a serde_json::Value,
    }
    let null = serde_json::Value::Null;
    let mut cells: Vec<Cell> = Vec::new();
    let mut index: HashMap<(i64, &str), usize> = HashMap::new();
    for row in rows {
        let Some(eid) = row.get("entityId").and_then(|x| x.as_i64()) else {
            continue;
        };
        let name = row
            .get("componentName")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        if !matches!(
            name,
            "core::Transform"
                | "core::GltfContainer"
                | "core::VisibilityComponent"
                | "core::MeshRenderer"
                | "core::Material"
        ) {
            continue;
        }
        let data = match row.get("data").filter(|d| d.is_object()) {
            Some(data) => data,
            // A MeshRenderer DELETE still marks the entity; every other DELETE is ignored.
            None if name == "core::MeshRenderer" => &null,
            None => continue,
        };
        let timestamp = row.get("timestamp").and_then(|t| t.as_f64());
        match index.get(&(eid, name)) {
            Some(&i) => {
                let cell = &mut cells[i];
                if let (Some(have), Some(new)) = (cell.timestamp, timestamp) {
                    if new < have {
                        continue;
                    }
                }
                cell.timestamp = timestamp;
                cell.data = data;
            }
            None => {
                index.insert((eid, name), cells.len());
                cells.push(Cell {
                    eid,
                    name,
                    timestamp,
                    data,
                });
            }
        }
    }
    let mut transforms: HashMap<i64, Trs> = HashMap::new();
    let mut gltf_srcs: Vec<(i64, String)> = Vec::new();
    let mut mesh_renderers: Vec<(i64, Option<PrimitiveSpec>)> = Vec::new();
    let mut materials: HashMap<i64, MaterialFields> = HashMap::new();
    let mut visibility: HashMap<i64, bool> = HashMap::new();
    for cell in &cells {
        let data = cell.data;
        match cell.name {
            "core::MeshRenderer" => {
                mesh_renderers.push((cell.eid, primitives::mesh_renderer_from_json(data)));
            }
            "core::Material" => {
                if let Some(m) = primitives::material_from_json(data) {
                    materials.insert(cell.eid, m);
                }
            }
            "core::Transform" => {
                transforms.insert(
                    cell.eid,
                    Trs {
                        position: vec3_or(data.get("position"), [0.0; 3]),
                        rotation: quat_or_identity(data.get("rotation")),
                        scale: vec3_or(data.get("scale"), [1.0; 3]),
                        parent: data.get("parent").and_then(|p| p.as_i64()).unwrap_or(0),
                    },
                );
            }
            "core::GltfContainer" => {
                if let Some(src) = data.get("src").and_then(|s| s.as_str()) {
                    if !src.is_empty() {
                        gltf_srcs.push((cell.eid, src.to_string()));
                    }
                }
            }
            "core::VisibilityComponent" => {
                visibility.insert(
                    cell.eid,
                    data.get("visible")
                        .and_then(|b| b.as_bool())
                        .unwrap_or(false),
                );
            }
            _ => {}
        }
    }
    Ok(placements_from_components(
        transforms,
        gltf_srcs,
        mesh_renderers,
        materials,
        visibility,
        content_by_file,
    ))
}

pub fn parse_lod_manifest(
    bytes: &[u8],
    content_by_file: &HashMap<String, String>,
) -> Result<Vec<Placement>> {
    Ok(parse_lod_manifest_full(bytes, content_by_file)?.placements)
}

/// `q` and `-q` are the same rotation.
pub fn same_rotation(a: [f64; 4], b: [f64; 4], tol: f64) -> bool {
    let within = |sign: f64| {
        a.iter()
            .zip(b.iter())
            .all(|(x, y)| (x - sign * y).abs() <= tol)
    };
    within(1.0) || within(-1.0)
}

pub fn same_trs(a: &Placement, b: &Placement, tol: f64) -> bool {
    let close = |x: &[f64], y: &[f64]| x.iter().zip(y.iter()).all(|(p, q)| (p - q).abs() <= tol);
    close(&a.position, &b.position)
        && close(&a.scale, &b.scale)
        && same_rotation(a.rotation, b.rotation, tol)
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct IssDiff {
    pub ours: usize,
    pub reference: usize,
    /// Reference placements with no counterpart of the same hash.
    pub missing: usize,
    /// Our placements with no counterpart of the same hash.
    pub extra: usize,
    /// Same-hash pairs left over once exact matches are removed.
    pub trs_mismatch: usize,
    #[serde(skip)]
    pub details: Vec<String>,
}

impl IssDiff {
    pub fn is_clean(&self) -> bool {
        self.missing + self.extra + self.trs_mismatch == 0
    }

    pub fn summary(&self) -> String {
        format!(
            "iss-diff: ours={} ref={} missing={} extra={} trs-mismatch={}",
            self.ours, self.reference, self.missing, self.extra, self.trs_mismatch
        )
    }
}

fn fmt_placement(p: &Placement) -> String {
    let r = |v: f64| format!("{:.4}", v);
    format!(
        "pos({},{},{}) rot({},{},{},{}) scale({},{},{})",
        r(p.position[0]),
        r(p.position[1]),
        r(p.position[2]),
        r(p.rotation[0]),
        r(p.rotation[1]),
        r(p.rotation[2]),
        r(p.rotation[3]),
        r(p.scale[0]),
        r(p.scale[1]),
        r(p.scale[2])
    )
}

/// Multiset comparison of two placement lists keyed by content hash: each of
/// ours consumes the first unused reference placement with the same hash and
/// a TRS within `tol` per component (rotation sign-insensitive); the same-hash
/// leftovers pair up as `trs_mismatch`, the rest count as `extra` (ours) or
/// `missing` (reference). Order never matters.
pub fn diff_iss(ours: &[Placement], reference: &[Placement], tol: f64) -> IssDiff {
    const MAX_DETAILS: usize = 24;
    let key = |p: &Placement| p.glb_hash.clone().unwrap_or_default();
    let mut used = vec![false; reference.len()];
    let mut ours_left: Vec<&Placement> = Vec::new();
    for o in ours {
        let hit = reference
            .iter()
            .enumerate()
            .position(|(i, r)| !used[i] && r.glb_hash == o.glb_hash && same_trs(o, r, tol));
        match hit {
            Some(i) => used[i] = true,
            None => ours_left.push(o),
        }
    }
    let mut ref_left: HashMap<String, Vec<&Placement>> = HashMap::new();
    for (i, r) in reference.iter().enumerate() {
        if !used[i] {
            ref_left.entry(key(r)).or_default().push(r);
        }
    }
    let mut diff = IssDiff {
        ours: ours.len(),
        reference: reference.len(),
        ..Default::default()
    };
    let mut push_detail = |line: String| {
        if diff.details.len() < MAX_DETAILS {
            diff.details.push(line);
        }
    };
    for o in ours_left {
        let hash = key(o);
        match ref_left.get_mut(&hash).and_then(|v| {
            if v.is_empty() {
                return None;
            }
            let dist = |r: &Placement| {
                o.position
                    .iter()
                    .zip(r.position.iter())
                    .map(|(a, b)| (a - b) * (a - b))
                    .sum::<f64>()
            };
            let nearest = (0..v.len())
                .min_by(|&a, &b| dist(v[a]).total_cmp(&dist(v[b])))
                .unwrap();
            Some(v.swap_remove(nearest))
        }) {
            Some(r) => {
                diff.trs_mismatch += 1;
                push_detail(format!(
                    "iss-diff trs-mismatch {} ours {} ref {}",
                    hash,
                    fmt_placement(o),
                    fmt_placement(r)
                ));
            }
            None => {
                diff.extra += 1;
                push_detail(format!("iss-diff extra {} ours {}", hash, fmt_placement(o)));
            }
        }
    }
    let mut missing: Vec<(String, &Placement)> = ref_left
        .into_iter()
        .flat_map(|(h, v)| v.into_iter().map(move |r| (h.clone(), r)))
        .collect();
    missing.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| cmp_f64s(&a.1.position, &b.1.position))
    });
    for (hash, r) in missing {
        diff.missing += 1;
        push_detail(format!(
            "iss-diff missing {} ref {}",
            hash,
            fmt_placement(r)
        ));
    }
    diff
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-6 * b.abs().max(1.0)
    }

    fn approx3(a: [f64; 3], b: [f64; 3]) -> bool {
        a.iter().zip(b.iter()).all(|(x, y)| approx(*x, *y))
    }

    const ISS_FIXTURE: &str = r#"{
    "version": 1,
    "sceneId": "bafkreifz6o7w75gy5t3ymlelhk4kuir2t7324vchat5kevy5vbkjmicvim",
    "assets": [
        {
            "hash": "bafkreiak47hgur7axdwsv53bu6vja5bapfgka3tx4ac6spf2rvspk33ipa",
            "position": {
                "x": 88.8968276977539,
                "y": 0.2825070321559906,
                "z": 13.414548873901368
            },
            "rotation": {
                "x": 0.0,
                "y": 0.7071065902709961,
                "z": 0.0,
                "w": 0.7071070671081543
            },
            "scale": {
                "x": 0.9699999690055847,
                "y": 0.9700000286102295,
                "z": 0.9699999690055847
            }
        },
        {
            "hash": "bafkreiak47hgur7axdwsv53bu6vja5bapfgka3tx4ac6spf2rvspk33ipa",
            "position": {
                "x": 93.97085571289063,
                "y": 0.2825070321559906,
                "z": 8.382261276245118
            },
            "rotation": {
                "x": 0.0,
                "y": -2.980231954552437e-7,
                "z": 0.0,
                "w": 1.0
            },
            "scale": {
                "x": 0.9700000286102295,
                "y": 0.9700000286102295,
                "z": 0.9700000286102295
            }
        },
        {
            "hash": "aaadefaults",
            "position": {
                "x": 1.0,
                "y": 2.0,
                "z": 3.0
            }
        }
    ]
}"#;

    #[test]
    fn iss_fixture_parses_in_file_order() {
        let got = parse_iss(ISS_FIXTURE.as_bytes()).unwrap();
        assert_eq!(got.len(), 3);
        let a = &got[0];
        assert_eq!(
            a.glb_hash.as_deref(),
            Some("bafkreiak47hgur7axdwsv53bu6vja5bapfgka3tx4ac6spf2rvspk33ipa")
        );
        assert_eq!(
            a.position,
            [88.8968276977539, 0.2825070321559906, 13.414548873901368]
        );
        assert_eq!(
            a.rotation,
            [0.0, 0.7071065902709961, 0.0, 0.7071070671081543]
        );
        assert_eq!(
            a.scale,
            [0.9699999690055847, 0.9700000286102295, 0.9699999690055847]
        );
        assert_eq!(a.glb_file, None);
        let b = &got[1];
        assert_eq!(
            b.position,
            [93.97085571289063, 0.2825070321559906, 8.382261276245118]
        );
        assert_eq!(b.rotation, [0.0, -2.980231954552437e-7, 0.0, 1.0]);
        let d = &got[2];
        assert_eq!(d.glb_hash.as_deref(), Some("aaadefaults"));
        assert_eq!(d.position, [1.0, 2.0, 3.0]);
        assert_eq!(d.rotation, [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(d.scale, [1.0, 1.0, 1.0]);
    }

    #[test]
    fn iss_descriptor_round_trips_bit_exact() {
        let want = parse_iss(ISS_FIXTURE.as_bytes()).unwrap();
        assert_eq!(want.len(), 3);
        let rebuilt: Vec<(String, &Placement)> = want
            .iter()
            .map(|p| (p.glb_hash.clone().unwrap(), p))
            .collect();
        let sid = "bafkreifz6o7w75gy5t3ymlelhk4kuir2t7324vchat5kevy5vbkjmicvim";
        let doc = iss_descriptor(sid, &rebuilt);
        assert_eq!(doc["version"], serde_json::json!(1));
        assert_eq!(doc["sceneId"], serde_json::json!(sid));
        assert_eq!(doc["assets"].as_array().unwrap().len(), 3);
        let bytes = serde_json::to_vec(&doc).unwrap();
        let got = parse_iss(&bytes).unwrap();
        assert_eq!(got, want);
        assert_eq!(got[2].glb_hash.as_deref(), Some("aaadefaults"));
        assert_eq!(got[2].rotation, [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(got[2].scale, [1.0, 1.0, 1.0]);
        assert_eq!(
            got[0].scale,
            [0.9699999690055847, 0.9700000286102295, 0.9699999690055847]
        );
        assert_eq!(
            got[0].rotation,
            [0.0, 0.7071065902709961, 0.0, 0.7071070671081543]
        );
        assert_eq!(got[1].rotation, [0.0, -2.980231954552437e-7, 0.0, 1.0]);
        let again = serde_json::to_vec(&iss_descriptor(sid, &rebuilt)).unwrap();
        assert_eq!(bytes, again);
        let pretty = serde_json::to_string_pretty(&doc).unwrap();
        assert_eq!(parse_iss(pretty.as_bytes()).unwrap(), want);
        assert!(pretty.contains("0.9699999690055847"));
        assert!(pretty.contains("-2.980231954552437e-7"));
    }

    const MANIFEST_FIXTURE: &str = r#"[
  {
    "entityId": 512,
    "componentId": 1,
    "componentName": "core::Transform",
    "data": {
      "position": {
        "x": null,
        "y": 0,
        "z": null
      },
      "rotation": {
        "x": 0,
        "y": 0.7071067690849304,
        "z": 0,
        "w": 0.7071067690849304
      },
      "scale": {
        "x": 16,
        "y": 1,
        "z": 16
      },
      "parent": 0
    }
  },
  {
    "entityId": 512,
    "componentId": 1041,
    "componentName": "core::GltfContainer",
    "data": {
      "src": "assets/road-driveway-double.glb",
      "visibleMeshesCollisionMask": 2,
      "invisibleMeshesCollisionMask": 0
    }
  }
]"#;

    #[test]
    fn lod_manifest_fixture_parses() {
        let mut content = HashMap::new();
        content.insert(
            "Assets/Road-Driveway-Double.GLB".to_string(),
            "bafkreiroadhash".to_string(),
        );
        let got = parse_lod_manifest_full(MANIFEST_FIXTURE.as_bytes(), &content).unwrap();
        assert_eq!(got.placements.len(), 1);
        assert_eq!(got.skipped_mesh_renderer, 0);
        assert_eq!(got.unresolved_src, 0);
        let p = &got.placements[0];
        assert_eq!(p.glb_hash.as_deref(), Some("bafkreiroadhash"));
        assert_eq!(
            p.glb_file.as_deref(),
            Some("assets/road-driveway-double.glb")
        );
        assert_eq!(p.position, [0.0, 0.0, 0.0]);
        assert!(same_rotation(
            p.rotation,
            [0.0, 0.7071067690849304, 0.0, 0.7071067690849304],
            1e-6
        ));
        assert!(p.rotation[3] > 0.0);
        assert!(approx3(p.scale, [16.0, 1.0, 16.0]), "{:?}", p.scale);
    }

    #[test]
    fn lod_manifest_parent_chain_composes() {
        let s2 = std::f64::consts::FRAC_1_SQRT_2;
        let fixture = serde_json::json!([
            {
                "entityId": 600,
                "componentName": "core::Transform",
                "data": {
                    "position": {"x": 10.0, "y": 0.0, "z": 0.0},
                    "rotation": {"x": 0.0, "y": s2, "z": 0.0, "w": s2},
                    "scale": {"x": 2.0, "y": 2.0, "z": 2.0},
                    "parent": 0
                }
            },
            {
                "entityId": 601,
                "componentName": "core::Transform",
                "data": {
                    "position": {"x": 1.0, "y": 0.0, "z": 0.0},
                    "rotation": {"x": 0.0, "y": 0.0, "z": 0.0, "w": 1.0},
                    "scale": {"x": 1.0, "y": 1.0, "z": 1.0},
                    "parent": 600
                }
            },
            {
                "entityId": 601,
                "componentName": "core::GltfContainer",
                "data": {"src": "models/child.glb"}
            }
        ]);
        let bytes = serde_json::to_vec(&fixture).unwrap();
        let mut content = HashMap::new();
        content.insert("models/child.glb".to_string(), "hchild".to_string());
        let got = parse_lod_manifest_full(&bytes, &content).unwrap();
        assert_eq!(got.placements.len(), 1);
        let p = &got.placements[0];
        assert!(approx3(p.position, [10.0, 0.0, -2.0]), "{:?}", p.position);
        assert!(approx(p.rotation[1], s2) && approx(p.rotation[3], s2));
        assert!(approx3(p.scale, [2.0, 2.0, 2.0]));
    }

    #[test]
    fn lod_manifest_skips_and_unresolved_counted() {
        let fixture = serde_json::json!([
            {
                "entityId": 700,
                "componentName": "core::MeshRenderer",
                "data": {"mesh": {"$case": "box", "box": {"uvs": []}}}
            },
            {
                "entityId": 701,
                "componentName": "core::GltfContainer",
                "data": {"src": "models/missing.glb"}
            },
            {
                "entityId": 702,
                "componentName": "core::MeshRenderer",
                "data": null
            }
        ]);
        let bytes = serde_json::to_vec(&fixture).unwrap();
        let content = HashMap::new();
        let got = parse_lod_manifest_full(&bytes, &content).unwrap();
        assert_eq!(got.mesh_renderers, 2);
        assert_eq!(got.skipped_mesh_renderer, 1);
        assert_eq!(got.unresolved_src, 1);
        assert!(got.placements.is_empty());
        assert_eq!(got.primitives.len(), 1);
        let p = &got.primitives[0];
        assert_eq!(
            p.spec,
            PrimitiveSpec::simple(primitives::PrimitiveShape::Box)
        );
        assert_eq!(p.material, primitives::PrimitiveMaterial::default());
        assert_eq!(p.position, [0.0; 3]);
        assert_eq!(p.rotation, IDENTITY_ROTATION);
        assert_eq!(p.scale, [1.0; 3]);
    }

    #[test]
    fn lod_manifest_primitives_take_transform_material_and_visibility() {
        let fixture = serde_json::json!([
            {
                "entityId": 600,
                "componentName": "core::Transform",
                "data": {
                    "position": {"x": 8.0, "y": 0.0, "z": 8.0},
                    "rotation": {"x": 0.0, "y": 0.0, "z": 0.0, "w": 1.0},
                    "scale": {"x": 2.0, "y": 2.0, "z": 2.0},
                    "parent": 0
                }
            },
            {
                "entityId": 601,
                "componentName": "core::Transform",
                "data": {
                    "position": {"x": 1.0, "y": 2.0, "z": 3.0},
                    "rotation": {"x": 0.0, "y": 0.0, "z": 0.0, "w": 1.0},
                    "scale": {"x": 1.0, "y": 1.0, "z": 1.0},
                    "parent": 600
                }
            },
            {
                "entityId": 601,
                "componentName": "core::MeshRenderer",
                "data": {"mesh": {"$case": "cylinder", "cylinder": {"radiusTop": 0, "radiusBottom": 1}}}
            },
            {
                "entityId": 601,
                "componentName": "core::Material",
                "data": {"material": {"$case": "pbr", "pbr": {
                    "texture": {"tex": {"$case": "texture", "texture": {"src": "Images/Wood.PNG", "wrapMode": 0, "filterMode": 0}}},
                    "albedoColor": {"r": 1, "g": 0.5, "b": 0, "a": 1},
                    "transparencyMode": 4
                }}}
            },
            {
                "entityId": 602,
                "componentName": "core::MeshRenderer",
                "data": {"mesh": {"$case": "sphere", "sphere": {}}}
            },
            {
                "entityId": 602,
                "componentName": "core::VisibilityComponent",
                "data": {"visible": false}
            },
            {
                "entityId": 603,
                "componentName": "core::MeshRenderer",
                "data": {"mesh": {"$case": "plane", "plane": {"uvs": []}}}
            },
            {
                "entityId": 603,
                "componentName": "core::Material",
                "data": {"material": {"$case": "pbr", "pbr": {
                    "texture": {"tex": {"$case": "texture", "texture": {"src": "images/nowhere.png", "wrapMode": 0}}},
                    "albedoColor": {"r": 1, "g": 1, "b": 1, "a": 0.5}
                }}}
            },
            {
                "entityId": 603,
                "componentName": "core::MeshRenderer",
                "data": {"mesh": {"$case": "box", "box": {"uvs": []}}}
            }
        ]);
        let mut content = HashMap::new();
        content.insert("images/wood.png".to_string(), "bafkreiwood".to_string());
        let got =
            parse_lod_manifest_full(&serde_json::to_vec(&fixture).unwrap(), &content).unwrap();
        assert_eq!(got.mesh_renderers, 3);
        assert_eq!(got.skipped_mesh_renderer, 1);
        assert_eq!(got.missing_textures, 1);
        assert_eq!(got.primitives.len(), 2);

        let cone = &got.primitives[0];
        assert_eq!(cone.spec.shape, primitives::PrimitiveShape::Cylinder);
        assert_eq!(cone.spec.radius_top, Some(0.0));
        assert_eq!(cone.spec.radius_bottom, Some(1.0));
        assert!(
            approx3(cone.position, [10.0, 4.0, 14.0]),
            "{:?}",
            cone.position
        );
        assert!(approx3(cone.scale, [2.0, 2.0, 2.0]));
        assert_eq!(cone.material.color, [1.0, 0.5, 0.0, 1.0]);
        assert_eq!(cone.material.class, super::super::model::AlphaClass::Opaque);
        assert_eq!(
            cone.material.texture,
            Some(primitives::PrimitiveTexture {
                source: primitives::TextureSource::Hash("bafkreiwood".to_string()),
                wrap: primitives::WrapMode::Repeat,
            })
        );

        // last MeshRenderer row wins; missing texture falls back to colour; alpha < 1 blends
        let cube = &got.primitives[1];
        assert_eq!(cube.spec.shape, primitives::PrimitiveShape::Box);
        assert_eq!(cube.material.class, super::super::model::AlphaClass::Blend);
        assert!(cube.material.texture.is_none());
        assert_eq!(cube.position, [0.0; 3]);
    }

    #[test]
    fn lod_manifest_last_row_wins_unless_timestamped() {
        let row = |src: &str, ts: Option<u32>| {
            let mut r = serde_json::json!({
                "entityId": 900,
                "componentName": "core::GltfContainer",
                "data": {"src": src}
            });
            if let Some(t) = ts {
                r["timestamp"] = serde_json::json!(t);
            }
            r
        };
        let mut content = HashMap::new();
        for f in ["a.glb", "b.glb", "c.glb"] {
            content.insert(f.to_string(), format!("h{f}"));
        }
        let plain =
            serde_json::to_vec(&serde_json::json!([row("a.glb", None), row("b.glb", None)]))
                .unwrap();
        let got = parse_lod_manifest_full(&plain, &content).unwrap();
        assert_eq!(got.placements.len(), 1);
        assert_eq!(got.placements[0].glb_hash.as_deref(), Some("hb.glb"));
        let timed = serde_json::to_vec(&serde_json::json!([
            row("a.glb", Some(5)),
            row("b.glb", Some(3)),
            row("c.glb", Some(5))
        ]))
        .unwrap();
        let got = parse_lod_manifest_full(&timed, &content).unwrap();
        assert_eq!(got.placements.len(), 1);
        assert_eq!(got.placements[0].glb_hash.as_deref(), Some("hc.glb"));
        let deleted = serde_json::to_vec(&serde_json::json!([
            row("a.glb", None),
            {"entityId": 900, "componentName": "core::GltfContainer", "data": null}
        ]))
        .unwrap();
        let got = parse_lod_manifest_full(&deleted, &content).unwrap();
        assert_eq!(got.placements[0].glb_hash.as_deref(), Some("ha.glb"));
    }

    #[test]
    fn deterministic_ordering() {
        let mk = |hash: &str, file: Option<&str>, x: f64| Placement {
            glb_hash: Some(hash.to_string()),
            glb_file: file.map(String::from),
            position: [x, 0.0, 0.0],
            ..Default::default()
        };
        let mut a = vec![
            mk("b", None, 1.0),
            mk("a", None, 2.0),
            mk("a", None, -1.0),
            mk("b", Some("f.glb"), 1.0),
        ];
        let mut b = a.clone();
        b.reverse();
        sort_placements(&mut a);
        sort_placements(&mut b);
        assert_eq!(a, b);
        assert_eq!(a[0].glb_hash.as_deref(), Some("a"));
        assert_eq!(a[0].position[0], -1.0);
        assert_eq!(a[1].position[0], 2.0);
        assert_eq!(a[2].glb_file, None);
        assert_eq!(a[3].glb_file.as_deref(), Some("f.glb"));
    }

    #[test]
    fn diff_iss_counts_missing_extra_and_mismatch() {
        let mk = |hash: &str, x: f64, rotation: [f64; 4]| Placement {
            glb_hash: Some(hash.to_string()),
            position: [x, 0.0, 0.0],
            rotation,
            ..Default::default()
        };
        let id = [0.0, 0.0, 0.0, 1.0];
        let ours = vec![
            mk("a", 1.0, id),
            mk("a", 2.0, [0.0, 0.8, 0.0, 0.6]),
            mk("b", 0.0, id),
        ];
        let reference = vec![
            mk("a", 2.0, [0.0, -0.8, 0.0, -0.6]),
            mk("a", 1.5, id),
            mk("c", 0.0, id),
        ];
        let d = diff_iss(&ours, &reference, 1e-3);
        assert_eq!((d.ours, d.reference), (3, 3));
        assert_eq!((d.missing, d.extra, d.trs_mismatch), (1, 1, 1));
        assert!(!d.is_clean());
        assert_eq!(
            d.summary(),
            "iss-diff: ours=3 ref=3 missing=1 extra=1 trs-mismatch=1"
        );
        assert!(d
            .details
            .iter()
            .any(|l| l.starts_with("iss-diff trs-mismatch a ")));
        assert!(d.details.iter().any(|l| l.starts_with("iss-diff extra b ")));
        assert!(d
            .details
            .iter()
            .any(|l| l.starts_with("iss-diff missing c ")));
        let mut shuffled = ours.clone();
        shuffled.reverse();
        let same = diff_iss(&shuffled, &ours, 1e-3);
        assert!(same.is_clean(), "{}", same.summary());
    }

    #[test]
    fn plaza_iss_full_guarded() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../docs/testing/lodgen-firststab-20260708/prod/bafkreifz6o7w75gy5t3ymlelhk4kuir2t7324vchat5kevy5vbkjmicvim_InitialSceneState.json"
        );
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let got = parse_iss(&bytes).unwrap();
        assert_eq!(got.len(), 639);
        assert!(got.iter().all(|p| p.glb_hash.is_some()));
        assert!(got.iter().all(|p| {
            p.position.iter().all(|v| v.is_finite())
                && p.rotation.iter().all(|v| v.is_finite())
                && p.scale.iter().all(|v| v.is_finite())
        }));
        let hashes: HashSet<&str> = got.iter().filter_map(|p| p.glb_hash.as_deref()).collect();
        assert!(hashes.len() > 1);
    }
}

/// Production descriptors regenerated from their manifests
/// (`scripts/lod-conformance-fixtures.py`).
#[cfg(test)]
mod conformance {
    use super::*;

    const TEAPARK_MANIFEST: &str = include_str!("testdata/conformance/teapark.lod-manifest.json");
    const TEAPARK_ISS: &str = include_str!("testdata/conformance/teapark.InitialSceneState.json");
    const TEAPARK_CONTENT: &str = include_str!("testdata/conformance/teapark.content.json");
    const AMAIXEN_MANIFEST: &str = include_str!("testdata/conformance/amaixen.lod-manifest.json");
    const AMAIXEN_ISS: &str = include_str!("testdata/conformance/amaixen.InitialSceneState.json");
    const AMAIXEN_CONTENT: &str = include_str!("testdata/conformance/amaixen.content.json");
    const LOUNGE_MANIFEST: &str = include_str!("testdata/conformance/lounge.lod-manifest.json");
    const LOUNGE_ISS: &str = include_str!("testdata/conformance/lounge.InitialSceneState.json");
    const LOUNGE_CONTENT: &str = include_str!("testdata/conformance/lounge.content.json");
    const CRASHWORLD_MANIFEST: &str =
        include_str!("testdata/conformance/crashworld-subset.lod-manifest.json");
    const CRASHWORLD_ISS: &str =
        include_str!("testdata/conformance/crashworld-subset.InitialSceneState.json");
    const CRASHWORLD_CONTENT: &str =
        include_str!("testdata/conformance/crashworld-subset.content.json");

    const TOL: f64 = 1e-3;

    fn content(json: &str) -> HashMap<String, String> {
        serde_json::from_str(json).unwrap()
    }

    fn check(manifest: &str, iss: &str, content_json: &str, want: usize) -> ManifestPlacements {
        let full = parse_lod_manifest_full(manifest.as_bytes(), &content(content_json)).unwrap();
        let reference = parse_iss(iss.as_bytes()).unwrap();
        assert_eq!(reference.len(), want);
        assert_eq!(full.placements.len(), want);
        let d = diff_iss(&full.placements, &reference, TOL);
        assert!(d.is_clean(), "{}\n{}", d.summary(), d.details.join("\n"));
        let ours: Vec<&str> = full
            .placements
            .iter()
            .map(|p| p.glb_hash.as_deref().unwrap())
            .collect();
        let theirs: Vec<&str> = reference
            .iter()
            .map(|p| p.glb_hash.as_deref().unwrap())
            .collect();
        assert_eq!(ours, theirs, "descriptor order");
        for (o, r) in full.placements.iter().zip(reference.iter()) {
            assert!(same_trs(o, r, TOL));
            assert!(
                o.rotation
                    .iter()
                    .zip(r.rotation.iter())
                    .all(|(a, b)| (a - b).abs() <= TOL),
                "sign convention differs: ours {:?} production {:?}",
                o.rotation,
                r.rotation
            );
        }
        full
    }

    #[test]
    fn teapark_iss_matches_production() {
        let full = check(TEAPARK_MANIFEST, TEAPARK_ISS, TEAPARK_CONTENT, 77);
        assert_eq!(full.invisible_skipped, 1);
        assert_eq!(full.unresolved_src, 1);
        assert_eq!(full.excluded_src, 0);
    }

    #[test]
    fn amaixen_iss_matches_production() {
        let full = check(AMAIXEN_MANIFEST, AMAIXEN_ISS, AMAIXEN_CONTENT, 26);
        assert_eq!(full.invisible_skipped, 15);
        assert_eq!(full.unresolved_src, 0);
        // 33 textured planes (publit.io URLs) + 4 translucent cones, two of them hidden.
        assert_eq!(full.mesh_renderers, 37);
        assert_eq!(full.skipped_mesh_renderer, 2);
        assert_eq!(full.primitives.len(), 35);
        assert_eq!(full.missing_textures, 0);
        let planes = full
            .primitives
            .iter()
            .filter(|p| p.spec.shape == primitives::PrimitiveShape::Plane)
            .count();
        assert_eq!(planes, 33);
        assert!(full
            .primitives
            .iter()
            .filter(|p| p.spec.shape == primitives::PrimitiveShape::Plane)
            .all(|p| matches!(
                p.material.texture.as_ref().map(|t| &t.source),
                Some(primitives::TextureSource::Url(u)) if u.starts_with("https://")
            )));
        let cones: Vec<_> = full
            .primitives
            .iter()
            .filter(|p| p.spec.shape == primitives::PrimitiveShape::Cylinder)
            .collect();
        assert_eq!(cones.len(), 2);
        assert!(cones.iter().all(|p| {
            p.material.class == super::super::model::AlphaClass::Blend
                && p.material.texture.is_none()
                && p.spec.radius_bottom == Some(0.0)
        }));
    }

    #[test]
    fn lounge_iss_matches_production() {
        let full = check(LOUNGE_MANIFEST, LOUNGE_ISS, LOUNGE_CONTENT, 52);
        assert_eq!(full.invisible_skipped, 0);
        assert_eq!(full.unresolved_src, 0);
        // 148 boxes + 8 planes, all visible; 6 scene-file textures (AHL_1, AHL_head,
        // ROWSIL_BLACK, kairos, kairos_amargo, rabbit), all present in the entity content.
        assert_eq!(full.mesh_renderers, 156);
        assert_eq!(full.skipped_mesh_renderer, 0);
        assert_eq!(full.primitives.len(), 156);
        assert_eq!(full.missing_textures, 0);
        let textured = full
            .primitives
            .iter()
            .filter(|p| p.material.texture.is_some())
            .count();
        assert_eq!(textured, 6);
        let near_minus_two = full
            .placements
            .iter()
            .filter(|p| (p.position[1] + 1.988).abs() < 0.002)
            .count();
        assert_eq!(near_minus_two, 4);
    }

    #[test]
    fn crashworld_subset_negative_scale_decomposes_like_unity() {
        let reference = parse_iss(CRASHWORLD_ISS.as_bytes()).unwrap();
        let full = check(
            CRASHWORLD_MANIFEST,
            CRASHWORLD_ISS,
            CRASHWORLD_CONTENT,
            reference.len(),
        );
        let negative: Vec<&Placement> = full
            .placements
            .iter()
            .filter(|p| p.scale[0] < 0.0)
            .collect();
        assert_eq!(negative.len(), 12);
        assert!(reference.iter().filter(|p| p.scale[0] < 0.0).count() == 12);
        for p in &negative {
            assert!(p.scale[1] > 0.0 && p.scale[2] > 0.0, "{:?}", p.scale);
            assert!(p.rotation.iter().all(|v| v.is_finite()));
            let n: f64 = p.rotation.iter().map(|v| v * v).sum();
            assert!((n - 1.0).abs() < 1e-6);
        }
        assert!(
            full.placements
                .iter()
                .filter(|p| p.rotation[3] < 0.0)
                .count()
                >= 10
        );
    }

    #[test]
    fn zero_scale_placements_are_kept() {
        let reference = parse_iss(CRASHWORLD_ISS.as_bytes()).unwrap();
        let zero = reference.iter().filter(|p| p.scale.contains(&0.0)).count();
        assert!(zero >= 10);
        let full =
            parse_lod_manifest_full(CRASHWORLD_MANIFEST.as_bytes(), &content(CRASHWORLD_CONTENT))
                .unwrap();
        let ours: Vec<&Placement> = full
            .placements
            .iter()
            .filter(|p| p.scale.contains(&0.0))
            .collect();
        assert_eq!(ours.len(), zero);
        assert!(ours.iter().all(|p| p.rotation == IDENTITY_ROTATION));
        let fixture = serde_json::json!([
            {
                "entityId": 10,
                "componentName": "core::Transform",
                "data": {
                    "position": {"x": 4.0, "y": 5.0, "z": 6.0},
                    "rotation": {"x": 0.0, "y": 0.38268343, "z": 0.0, "w": 0.92387953},
                    "scale": {"x": 1.0, "y": 0.0, "z": 1.0},
                    "parent": 0
                }
            },
            {"entityId": 10, "componentName": "core::GltfContainer", "data": {"src": "flat.glb"}}
        ]);
        let mut content = HashMap::new();
        content.insert("flat.glb".to_string(), "hflat".to_string());
        let got =
            parse_lod_manifest_full(&serde_json::to_vec(&fixture).unwrap(), &content).unwrap();
        assert_eq!(got.placements.len(), 1);
        let p = &got.placements[0];
        assert_eq!(p.position, [4.0, 5.0, 6.0]);
        assert_eq!(p.rotation, IDENTITY_ROTATION);
        assert!(
            (p.scale[0] - 1.0).abs() < 1e-6 && p.scale[1] == 0.0 && (p.scale[2] - 1.0).abs() < 1e-6,
            "{:?}",
            p.scale
        );
    }

    #[test]
    fn nan_rotation_becomes_identity() {
        let zero = [[0.0; 4]; 4];
        let d = decompose_unity(&zero);
        assert_eq!(d.rotation, IDENTITY_ROTATION);
        assert_eq!(d.scale, [0.0; 3]);
        let t = Trs {
            scale: [0.0, 0.0, 0.0],
            rotation: [0.5, 0.5, 0.5, 0.5],
            ..Default::default()
        };
        let d = decompose_unity(&mat_trs(&t));
        assert_eq!(d.rotation, IDENTITY_ROTATION);
        assert!(matrix_to_quaternion(&[[f64::NAN; 3]; 3])
            .iter()
            .all(|v| v.is_nan()));
    }

    #[test]
    fn live_events_excluded_case_insensitive() {
        let fixture = serde_json::json!([
            {"entityId": 1, "componentName": "core::GltfContainer",
             "data": {"src": "Assets/Models/OUT/models/LIVE_EVENTS.glb"}},
            {"entityId": 2, "componentName": "core::GltfContainer",
             "data": {"src": "assets/models/out/models/next_live_events.GLB"}},
            {"entityId": 3, "componentName": "core::GltfContainer",
             "data": {"src": "assets/models/out/models/board.glb"}},
            {"entityId": 4, "componentName": "core::GltfContainer",
             "data": {"src": "other/live_events.glb"}}
        ]);
        let mut content = HashMap::new();
        for f in [
            "assets/models/out/models/live_events.glb",
            "assets/models/out/models/next_live_events.glb",
            "assets/models/out/models/board.glb",
            "other/live_events.glb",
        ] {
            content.insert(f.to_string(), format!("h:{f}"));
        }
        let got =
            parse_lod_manifest_full(&serde_json::to_vec(&fixture).unwrap(), &content).unwrap();
        assert_eq!(got.excluded_src, 2);
        let files: Vec<&str> = got
            .placements
            .iter()
            .map(|p| p.glb_file.as_deref().unwrap())
            .collect();
        assert_eq!(
            files,
            [
                "assets/models/out/models/board.glb",
                "other/live_events.glb"
            ]
        );
        assert!(gltf_src_is_excluded(
            "ASSETS/MODELS/OUT/MODELS/LIVE_EVENTS.GLB"
        ));
        assert!(!gltf_src_is_excluded("live_events.glb"));
    }

    #[test]
    fn unresolved_src_drops_all_placements() {
        let fixture = serde_json::json!([
            {"entityId": 1, "componentName": "core::GltfContainer", "data": {"src": "models/gone.glb"}},
            {"entityId": 2, "componentName": "core::GltfContainer", "data": {"src": "models/gone.glb"}},
            {"entityId": 3, "componentName": "core::GltfContainer", "data": {"src": "models/here.glb"}},
            {"entityId": 4, "componentName": "core::GltfContainer", "data": {"src": "models/Gone.glb"}}
        ]);
        let mut content = HashMap::new();
        content.insert("models/here.glb".to_string(), "hhere".to_string());
        let got =
            parse_lod_manifest_full(&serde_json::to_vec(&fixture).unwrap(), &content).unwrap();
        assert_eq!(got.unresolved_src, 2);
        assert_eq!(
            got.unresolved_srcs,
            vec!["models/gone.glb".to_string(), "models/Gone.glb".to_string()]
        );
        assert_eq!(got.placements.len(), 1);
        assert_eq!(got.placements[0].glb_hash.as_deref(), Some("hhere"));
        assert!(got.placements.iter().all(|p| p.glb_hash.is_some()));
    }

    /// The downstream contract: a model the deployment lacks never reaches
    /// the ISS descriptor the Explorer consumes — neither as an asset entry
    /// nor as a dangling file name.
    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn unresolved_src_never_reaches_the_iss_descriptor() {
        let fixture = serde_json::json!([
            {"entityId": 1, "componentName": "core::GltfContainer", "data": {"src": "models/gone.glb"}},
            {"entityId": 2, "componentName": "core::GltfContainer", "data": {"src": "models/here.glb"}},
            {"entityId": 3, "componentName": "core::GltfContainer", "data": {"src": "sittingChair1"}}
        ]);
        let mut content = HashMap::new();
        content.insert("models/here.glb".to_string(), "hhere".to_string());
        let got =
            parse_lod_manifest_full(&serde_json::to_vec(&fixture).unwrap(), &content).unwrap();
        assert_eq!(got.unresolved_src, 2);

        let out = std::env::temp_dir().join(format!(
            "abgen-iss-unresolved-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let (path, written, skipped) =
            super::super::write_iss_descriptor(&out, "scene", &got.placements, &content).unwrap();
        assert_eq!((written, skipped), (1, 0));
        let text = std::fs::read_to_string(&path).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
        let assets = doc["assets"].as_array().unwrap();
        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0]["hash"], "hhere");
        assert!(!text.contains("gone.glb") && !text.contains("sittingChair"));
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn unity_quaternion_sign_convention() {
        let s2 = std::f64::consts::FRAC_1_SQRT_2;
        let rot = |q: [f64; 4]| {
            let t = Trs {
                rotation: q,
                ..Default::default()
            };
            decompose_unity(&mat_trs(&t)).rotation
        };
        let close =
            |a: [f64; 4], b: [f64; 4]| a.iter().zip(b.iter()).all(|(x, y)| (x - y).abs() < 1e-6);
        // A quarter turn about y written with w < 0 comes back with w > 0
        // (positive trace branch): the same rotation, the other sign.
        assert!(close(rot([0.0, s2, 0.0, -s2]), [0.0, -s2, 0.0, s2]));
        assert!(close(rot([0.0, s2, 0.0, s2]), [0.0, s2, 0.0, s2]));
        // A half turn about y has trace -1: the y component is made positive.
        assert!(close(rot([0.0, -1.0, 0.0, 0.0]), [0.0, 1.0, 0.0, 0.0]));
        assert!(close(rot([0.0, 1.0, 0.0, 0.0]), [0.0, 1.0, 0.0, 0.0]));
        // Tea Park entity 551 (rock03.glb): production flips the whole quaternion.
        assert!(close(
            rot([0.0, 0.13052618503570557, 0.0, -0.9914448857307434]),
            [0.0, -0.13052618503570557, 0.0, 0.9914448857307434]
        ));
        // Genesis Plaza minuteHand.glb (entity 615 under 614): production keeps
        // w < 0 because the largest-diagonal branch fixes the sign of y, not w.
        let mut transforms = HashMap::new();
        transforms.insert(
            614,
            Trs {
                position: [27.933000564575195, 5.724999904632568, 11.932999610900879],
                rotation: [0.0, -0.3826834261417389, 0.0, 0.9238795042037964],
                scale: [0.8999999761581421; 3],
                parent: 0,
            },
        );
        transforms.insert(
            615,
            Trs {
                position: [0.03799999877810478, -0.014999999664723873, 0.0],
                rotation: [
                    -0.43045932054519653,
                    -0.5609855055809021,
                    0.43045932054519653,
                    0.5609855055809021,
                ],
                scale: [1.0; 3],
                parent: 614,
            },
        );
        let d = decompose_unity(&world_matrix(615, &transforms, &mut HashSet::new()));
        let want = [
            0.5624222159385681,
            0.7329629063606262,
            -0.23296292126178741,
            -0.30360323190689087,
        ];
        assert!(
            d.rotation
                .iter()
                .zip(want.iter())
                .all(|(a, b)| (a - b).abs() < 1e-5),
            "{:?}",
            d.rotation
        );
        assert!(d.rotation[3] < 0.0);
        let pos = [27.957183837890625, 5.7114996910095215, 11.957182884216309];
        assert!(d
            .position
            .iter()
            .zip(pos.iter())
            .all(|(a, b)| (a - b).abs() < 1e-5));
        assert!(d.scale.iter().all(|s| (s - 0.9).abs() < 1e-6));
        // Odd reflections land on scale.x with a proper rotation (crashworld WaterTank_02).
        let t = Trs {
            rotation: [0.0, 0.0, 1.0, 0.0],
            scale: [-1.0, -1.0, -1.0],
            ..Default::default()
        };
        let d = decompose_unity(&mat_trs(&t));
        assert!(close(d.rotation, [0.0, 1.0, 0.0, 0.0]), "{:?}", d.rotation);
        assert!(close(
            [d.scale[0], d.scale[1], d.scale[2], 0.0],
            [-1.0, 1.0, 1.0, 0.0]
        ));
    }
}

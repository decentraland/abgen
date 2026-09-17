//! Baked `SkinnedMeshRenderer.m_AABB` for animated skins.
//!
//! Unity only recomputes a skinned renderer's local bounds from `m_BonesAABB`
//! once, when `m_DirtyAABB` is set, using whatever pose the skeleton is in at
//! that moment. For a serialized bundle that is the bind pose at load time.
//! Rigs whose animations move the mesh far away from the bind pose (GoldSrc
//! view models, for one) then render outside a box that never follows them
//! and get frustum-culled. Unity's own model importer avoids this by baking a
//! box that covers every clip; this module does the same: it samples every
//! glTF animation, unions the per-bone boxes in root-bone space across all
//! sampled poses plus the rest pose, and returns that as the renderer's
//! `m_AABB` so the bundle can ship `m_DirtyAABB = false`.
//!
//! All matrices are built in glTF space and converted to Unity space by
//! conjugating with `X = diag(-1, 1, 1)`, which is exactly what the transform
//! and animation writers do (`conv_translation` / `conv_rotation`).

use serde_json::Value as J;
use std::collections::HashMap;

type Mat4 = [[f64; 4]; 4];

const ID: Mat4 = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
];

/// Samples per second of animation. Rotations interpolate along arcs, so the
/// extremes are not always on keyframes; 30 Hz plus every key time is plenty.
const SAMPLE_HZ: f64 = 30.0;
/// Upper bound on samples per clip, so a pathological clip cannot stall a build.
const MAX_SAMPLES_PER_CLIP: usize = 2000;

/// A per-bone AABB in Unity-space bone coordinates, as stored in `m_BonesAABB`.
#[derive(Clone, Copy, Debug)]
pub struct BoneBox {
    pub min: [f64; 3],
    pub max: [f64; 3],
}

impl BoneBox {
    fn valid(&self) -> bool {
        (0..3).all(|a| {
            self.min[a].is_finite() && self.max[a].is_finite() && self.min[a] <= self.max[a]
        })
    }
}

/// Everything needed to bake one renderer's bounds.
pub struct SkinBoundsInput<'a> {
    pub gltf: &'a J,
    pub buffers: &'a [Vec<u8>],
    /// glTF node indices of the skin joints, in `m_Bones` order.
    pub joints: &'a [usize],
    /// glTF node index of the Transform used as `m_RootBone`.
    pub root: usize,
    /// One entry per joint (same order); `None` or an invalid box means the
    /// bone influences nothing and is skipped, like Unity does.
    pub bone_boxes: &'a [Option<BoneBox>],
}

/// Returns `(center, extent)` of the baked local bounds in Unity space,
/// relative to the root bone, or `None` when nothing usable was found.
pub fn bake_skinned_aabb(input: &SkinBoundsInput<'_>) -> Option<([f64; 3], [f64; 3])> {
    let rig = Rig::from_gltf(input.gltf)?;
    if input.root >= rig.locals.len() {
        return None;
    }
    let clips = parse_clips(input.gltf, input.buffers);

    let mut acc = Union::empty();
    // Rest pose first: this is what Unity would have computed on its own.
    accumulate_pose(&rig, &HashMap::new(), input, &mut acc);

    for clip in &clips {
        for t in clip.sample_times() {
            let overrides = clip.pose_at(t);
            accumulate_pose(&rig, &overrides, input, &mut acc);
        }
    }
    acc.center_extent()
}

/// Diagnostic companion to [`bake_skinned_aabb`]: the same box per source
/// (`"<rest>"` first, then one entry per clip, named after the glTF clip).
pub fn bake_skinned_aabb_per_clip(
    input: &SkinBoundsInput<'_>,
) -> Vec<(String, Option<([f64; 3], [f64; 3])>)> {
    let Some(rig) = Rig::from_gltf(input.gltf) else {
        return Vec::new();
    };
    if input.root >= rig.locals.len() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut rest = Union::empty();
    accumulate_pose(&rig, &HashMap::new(), input, &mut rest);
    out.push(("<rest>".to_string(), rest.center_extent()));
    let names: Vec<String> = input.gltf["animations"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|c| c["name"].as_str().unwrap_or("").to_string())
                .collect()
        })
        .unwrap_or_default();
    for clip in parse_clips(input.gltf, input.buffers).iter() {
        let mut acc = Union::empty();
        for t in clip.sample_times() {
            accumulate_pose(&rig, &clip.pose_at(t), input, &mut acc);
        }
        out.push((
            names.get(clip.index).cloned().unwrap_or_default(),
            acc.center_extent(),
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Poses

#[derive(Clone, Copy, Debug)]
struct Trs {
    t: [f64; 3],
    r: [f64; 4],
    s: [f64; 3],
}

struct Rig {
    /// Rest local transform of every node (`matrix` nodes are pre-baked).
    locals: Vec<LocalXf>,
    parent: Vec<Option<usize>>,
}

#[derive(Clone, Copy)]
enum LocalXf {
    Trs(Trs),
    Matrix(Mat4),
}

impl Rig {
    fn from_gltf(gltf: &J) -> Option<Rig> {
        let nodes = gltf["nodes"].as_array()?;
        let mut locals = Vec::with_capacity(nodes.len());
        let mut parent = vec![None; nodes.len()];
        for (i, n) in nodes.iter().enumerate() {
            if let Some(m) = n["matrix"].as_array().filter(|m| m.len() == 16) {
                let cols: Vec<f64> = m.iter().map(|v| v.as_f64().unwrap_or(0.0)).collect();
                let mut mm = ID;
                for c in 0..4 {
                    for r in 0..4 {
                        mm[r][c] = cols[c * 4 + r];
                    }
                }
                locals.push(LocalXf::Matrix(mm));
            } else {
                locals.push(LocalXf::Trs(Trs {
                    t: vec3_or(&n["translation"], [0.0, 0.0, 0.0]),
                    r: vec4_or(&n["rotation"], [0.0, 0.0, 0.0, 1.0]),
                    s: vec3_or(&n["scale"], [1.0, 1.0, 1.0]),
                }));
            }
            if let Some(ch) = n["children"].as_array() {
                for c in ch {
                    if let Some(ci) = c.as_u64().map(|x| x as usize) {
                        if ci < nodes.len() && parent[ci].is_none() && ci != i {
                            parent[ci] = Some(i);
                        }
                    }
                }
            }
        }
        Some(Rig { locals, parent })
    }

    fn local(&self, i: usize, overrides: &HashMap<usize, Trs>) -> Mat4 {
        if let Some(o) = overrides.get(&i) {
            return trs_to_mat(o);
        }
        match self.locals[i] {
            LocalXf::Trs(t) => trs_to_mat(&t),
            LocalXf::Matrix(m) => m,
        }
    }

    /// World matrix via the parent chain, memoised per pose.
    fn world(
        &self,
        i: usize,
        overrides: &HashMap<usize, Trs>,
        memo: &mut HashMap<usize, Mat4>,
    ) -> Mat4 {
        if let Some(m) = memo.get(&i) {
            return *m;
        }
        // Walk up iteratively; guard against malformed cycles.
        let mut chain = Vec::new();
        let mut cur = Some(i);
        let mut steps = 0;
        while let Some(c) = cur {
            if memo.contains_key(&c) || steps > 4096 {
                break;
            }
            chain.push(c);
            cur = self.parent[c];
            steps += 1;
        }
        // `cur` is `None` at a real root, but also `Some` when the walk stopped
        // on the depth guard, where the node need not be memoised yet: a
        // malformed glTF can make the parent chain cyclic. Fall back to
        // identity rather than indexing a missing entry.
        let mut m = cur.and_then(|c| memo.get(&c).copied()).unwrap_or(ID);
        for &c in chain.iter().rev() {
            m = mul(&m, &self.local(c, overrides));
            memo.insert(c, m);
        }
        m
    }
}

fn accumulate_pose(
    rig: &Rig,
    overrides: &HashMap<usize, Trs>,
    input: &SkinBoundsInput<'_>,
    acc: &mut Union,
) {
    let mut memo = HashMap::new();
    let root_world = rig.world(input.root, overrides, &mut memo);
    let Some(root_inv) = inverse(&root_world) else {
        return;
    };
    for (k, &j) in input.joints.iter().enumerate() {
        let Some(Some(bb)) = input.bone_boxes.get(k) else {
            continue;
        };
        if !bb.valid() || j >= rig.locals.len() {
            continue;
        }
        let bone_world = rig.world(j, overrides, &mut memo);
        let rel = mul(&root_inv, &bone_world);
        let rel_u = conj_x(&rel);
        acc.add_box(&rel_u, bb);
    }
}

// ---------------------------------------------------------------------------
// Animation clips

#[derive(Clone, Copy, PartialEq)]
enum Interp {
    Step,
    Linear,
    Cubic,
}

#[derive(Clone, Copy, PartialEq)]
enum Path {
    Translation,
    Rotation,
    Scale,
}

struct Channel {
    node: usize,
    path: Path,
    interp: Interp,
    times: Vec<f64>,
    /// For `Cubic`: 3 entries per key (in-tangent, value, out-tangent).
    values: Vec<Vec<f64>>,
}

struct Clip {
    /// Index into `gltf.animations`.
    index: usize,
    channels: Vec<Channel>,
    rest: HashMap<usize, Trs>,
}

impl Clip {
    fn sample_times(&self) -> Vec<f64> {
        let mut end = 0.0f64;
        let mut keys: Vec<f64> = Vec::new();
        for ch in &self.channels {
            for &t in &ch.times {
                if t.is_finite() {
                    end = end.max(t);
                    keys.push(t);
                }
            }
        }
        let n_uniform = ((end * SAMPLE_HZ).ceil() as usize + 1).min(MAX_SAMPLES_PER_CLIP);
        let mut out: Vec<f64> = (0..n_uniform).map(|i| i as f64 / SAMPLE_HZ).collect();
        if keys.len() <= MAX_SAMPLES_PER_CLIP {
            out.extend(keys);
        }
        out.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        out.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
        out
    }

    fn pose_at(&self, t: f64) -> HashMap<usize, Trs> {
        let mut pose: HashMap<usize, Trs> = HashMap::new();
        for ch in &self.channels {
            let Some(v) = ch.sample(t) else { continue };
            let entry = pose.entry(ch.node).or_insert_with(|| {
                self.rest.get(&ch.node).copied().unwrap_or(Trs {
                    t: [0.0; 3],
                    r: [0.0, 0.0, 0.0, 1.0],
                    s: [1.0; 3],
                })
            });
            match ch.path {
                Path::Translation => entry.t = [v[0], v[1], v[2]],
                Path::Scale => entry.s = [v[0], v[1], v[2]],
                Path::Rotation => entry.r = normalize4([v[0], v[1], v[2], v[3]]),
            }
        }
        pose
    }
}

impl Channel {
    fn width(&self) -> usize {
        if self.path == Path::Rotation {
            4
        } else {
            3
        }
    }

    fn key_value(&self, k: usize) -> Option<&[f64]> {
        let idx = if self.interp == Interp::Cubic {
            k * 3 + 1
        } else {
            k
        };
        self.values.get(idx).map(|v| v.as_slice())
    }

    fn sample(&self, t: f64) -> Option<Vec<f64>> {
        let n = self.times.len();
        if n == 0 {
            return None;
        }
        let w = self.width();
        let take = |v: &[f64]| -> Vec<f64> {
            let mut o = vec![0.0; w];
            let n = w.min(v.len());
            o[..n].copy_from_slice(&v[..n]);
            o
        };
        if t <= self.times[0] {
            return self.key_value(0).map(take);
        }
        if t >= self.times[n - 1] {
            return self.key_value(n - 1).map(take);
        }
        // First key strictly after t.
        let hi = self.times.partition_point(|&k| k <= t);
        let lo = hi.saturating_sub(1);
        let (t0, t1) = (self.times[lo], self.times[hi]);
        let a = take(self.key_value(lo)?);
        let b = take(self.key_value(hi)?);
        let td = t1 - t0;
        if td <= 0.0 {
            return Some(b);
        }
        let s = ((t - t0) / td).clamp(0.0, 1.0);
        match self.interp {
            Interp::Step => Some(a),
            Interp::Linear => {
                if self.path == Path::Rotation {
                    let mut bq = [b[0], b[1], b[2], b[3]];
                    let dot: f64 = (0..4).map(|i| a[i] * bq[i]).sum();
                    if dot < 0.0 {
                        for x in bq.iter_mut() {
                            *x = -*x;
                        }
                    }
                    let q: Vec<f64> = (0..4).map(|i| a[i] + (bq[i] - a[i]) * s).collect();
                    Some(normalize4([q[0], q[1], q[2], q[3]]).to_vec())
                } else {
                    Some((0..w).map(|i| a[i] + (b[i] - a[i]) * s).collect())
                }
            }
            Interp::Cubic => {
                let out_a = take(self.values.get(lo * 3 + 2)?);
                let in_b = take(self.values.get(hi * 3)?);
                let s2 = s * s;
                let s3 = s2 * s;
                let h00 = 2.0 * s3 - 3.0 * s2 + 1.0;
                let h10 = s3 - 2.0 * s2 + s;
                let h01 = -2.0 * s3 + 3.0 * s2;
                let h11 = s3 - s2;
                Some(
                    (0..w)
                        .map(|i| h00 * a[i] + h10 * td * out_a[i] + h01 * b[i] + h11 * td * in_b[i])
                        .collect(),
                )
            }
        }
    }
}

fn parse_clips(gltf: &J, buffers: &[Vec<u8>]) -> Vec<Clip> {
    let Some(anims) = gltf["animations"].as_array() else {
        return Vec::new();
    };
    let rig_nodes = gltf["nodes"].as_array().map(|a| a.len()).unwrap_or(0);
    let rest_of = |i: usize| -> Trs {
        let n = &gltf["nodes"][i];
        Trs {
            t: vec3_or(&n["translation"], [0.0, 0.0, 0.0]),
            r: vec4_or(&n["rotation"], [0.0, 0.0, 0.0, 1.0]),
            s: vec3_or(&n["scale"], [1.0, 1.0, 1.0]),
        }
    };
    let mut clips = Vec::new();
    for (ai, anim) in anims.iter().enumerate() {
        let channels = anim["channels"].as_array().cloned().unwrap_or_default();
        let samplers = anim["samplers"].as_array().cloned().unwrap_or_default();
        let mut out = Vec::new();
        let mut rest = HashMap::new();
        for ch in &channels {
            let Some(si) = ch["sampler"].as_u64().map(|x| x as usize) else {
                continue;
            };
            let Some(sampler) = samplers.get(si) else {
                continue;
            };
            let Some(node) = ch["target"]["node"].as_u64().map(|x| x as usize) else {
                continue;
            };
            if node >= rig_nodes {
                continue;
            }
            let path = match ch["target"]["path"].as_str() {
                Some("translation") => Path::Translation,
                Some("rotation") => Path::Rotation,
                Some("scale") => Path::Scale,
                _ => continue,
            };
            let interp = match sampler["interpolation"].as_str() {
                Some("STEP") => Interp::Step,
                Some("CUBICSPLINE") => Interp::Cubic,
                _ => Interp::Linear,
            };
            let (Some(input), Some(output)) = (
                sampler["input"].as_u64().map(|x| x as usize),
                sampler["output"].as_u64().map(|x| x as usize),
            ) else {
                continue;
            };
            if gltf["accessors"].get(input).is_none() || gltf["accessors"].get(output).is_none() {
                continue;
            }
            let times: Vec<f64> =
                crate::animation::glb::read_accessor_with_buffers(gltf, buffers, input)
                    .into_iter()
                    .map(|t| t.first().copied().unwrap_or(0.0))
                    .collect();
            let values = crate::animation::glb::read_accessor_with_buffers(gltf, buffers, output);
            let expected = if interp == Interp::Cubic {
                times.len() * 3
            } else {
                times.len()
            };
            if times.is_empty() || values.len() < expected {
                continue;
            }
            rest.entry(node).or_insert_with(|| rest_of(node));
            out.push(Channel {
                node,
                path,
                interp,
                times,
                values,
            });
        }
        if !out.is_empty() {
            clips.push(Clip {
                index: ai,
                channels: out,
                rest,
            });
        }
    }
    clips
}

// ---------------------------------------------------------------------------
// Box union

struct Union {
    min: [f64; 3],
    max: [f64; 3],
    any: bool,
}

impl Union {
    fn empty() -> Self {
        Union {
            min: [f64::INFINITY; 3],
            max: [f64::NEG_INFINITY; 3],
            any: false,
        }
    }

    fn add_box(&mut self, m: &Mat4, bb: &BoneBox) {
        for corner in 0..8 {
            let p = [
                if corner & 1 == 0 {
                    bb.min[0]
                } else {
                    bb.max[0]
                },
                if corner & 2 == 0 {
                    bb.min[1]
                } else {
                    bb.max[1]
                },
                if corner & 4 == 0 {
                    bb.min[2]
                } else {
                    bb.max[2]
                },
            ];
            let q = xform(m, p);
            if !q.iter().all(|v| v.is_finite()) {
                continue;
            }
            for a in 0..3 {
                self.min[a] = self.min[a].min(q[a]);
                self.max[a] = self.max[a].max(q[a]);
            }
            self.any = true;
        }
    }

    fn center_extent(&self) -> Option<([f64; 3], [f64; 3])> {
        if !self.any {
            return None;
        }
        let c = [
            (self.min[0] + self.max[0]) * 0.5,
            (self.min[1] + self.max[1]) * 0.5,
            (self.min[2] + self.max[2]) * 0.5,
        ];
        let e = [
            (self.max[0] - self.min[0]) * 0.5,
            (self.max[1] - self.min[1]) * 0.5,
            (self.max[2] - self.min[2]) * 0.5,
        ];
        Some((c, e))
    }
}

// ---------------------------------------------------------------------------
// Small matrix toolkit

fn vec3_or(v: &J, d: [f64; 3]) -> [f64; 3] {
    match v.as_array() {
        Some(a) if a.len() >= 3 => [
            a[0].as_f64().unwrap_or(d[0]),
            a[1].as_f64().unwrap_or(d[1]),
            a[2].as_f64().unwrap_or(d[2]),
        ],
        _ => d,
    }
}

fn vec4_or(v: &J, d: [f64; 4]) -> [f64; 4] {
    match v.as_array() {
        Some(a) if a.len() >= 4 => [
            a[0].as_f64().unwrap_or(d[0]),
            a[1].as_f64().unwrap_or(d[1]),
            a[2].as_f64().unwrap_or(d[2]),
            a[3].as_f64().unwrap_or(d[3]),
        ],
        _ => d,
    }
}

fn normalize4(q: [f64; 4]) -> [f64; 4] {
    let n = (q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3]).sqrt();
    if n > 1e-12 && n.is_finite() {
        [q[0] / n, q[1] / n, q[2] / n, q[3] / n]
    } else {
        [0.0, 0.0, 0.0, 1.0]
    }
}

fn trs_to_mat(t: &Trs) -> Mat4 {
    let [x, y, z, w] = normalize4(t.r);
    let rot = [
        [
            1.0 - 2.0 * (y * y + z * z),
            2.0 * (x * y - z * w),
            2.0 * (x * z + y * w),
        ],
        [
            2.0 * (x * y + z * w),
            1.0 - 2.0 * (x * x + z * z),
            2.0 * (y * z - x * w),
        ],
        [
            2.0 * (x * z - y * w),
            2.0 * (y * z + x * w),
            1.0 - 2.0 * (x * x + y * y),
        ],
    ];
    let mut m = ID;
    for r in 0..3 {
        for c in 0..3 {
            m[r][c] = rot[r][c] * t.s[c];
        }
        m[r][3] = t.t[r];
    }
    m
}

fn mul(a: &Mat4, b: &Mat4) -> Mat4 {
    let mut o = [[0.0; 4]; 4];
    for r in 0..4 {
        for c in 0..4 {
            o[r][c] = (0..4).map(|k| a[r][k] * b[k][c]).sum();
        }
    }
    o
}

fn xform(m: &Mat4, p: [f64; 3]) -> [f64; 3] {
    let mut o = [0.0; 3];
    for r in 0..3 {
        o[r] = m[r][0] * p[0] + m[r][1] * p[1] + m[r][2] * p[2] + m[r][3];
    }
    o
}

/// `X * m * X` with `X = diag(-1, 1, 1)`: flips the sign of every entry with
/// exactly one x-index (row 0 or column 0, not both).
fn conj_x(m: &Mat4) -> Mat4 {
    let mut o = *m;
    for i in 1..4 {
        o[0][i] = -o[0][i];
        o[i][0] = -o[i][0];
    }
    o
}

/// Gauss-Jordan inverse; `None` for singular input (a zero-scaled root bone).
fn inverse(m: &Mat4) -> Option<Mat4> {
    let mut a = *m;
    let mut inv = ID;
    for col in 0..4 {
        let mut piv = col;
        for r in col + 1..4 {
            if a[r][col].abs() > a[piv][col].abs() {
                piv = r;
            }
        }
        if a[piv][col].abs() < 1e-12 {
            return None;
        }
        a.swap(col, piv);
        inv.swap(col, piv);
        let d = a[col][col];
        for c in 0..4 {
            a[col][c] /= d;
            inv[col][c] /= d;
        }
        for r in 0..4 {
            if r == col {
                continue;
            }
            let f = a[r][col];
            if f == 0.0 {
                continue;
            }
            for c in 0..4 {
                a[r][c] -= f * a[col][c];
                inv[r][c] -= f * inv[col][c];
            }
        }
    }
    Some(inv)
}

/// Reads `m_BonesAABB` entries back out of a built Mesh tree.
pub fn bone_boxes_from_mesh(mesh: &crate::value::Value) -> Vec<Option<BoneBox>> {
    let Some(arr) = mesh.get("m_BonesAABB").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let v3 = |v: Option<&crate::value::Value>| -> Option<[f64; 3]> {
        let v = v?;
        Some([
            v.get("x")?.as_f64()?,
            v.get("y")?.as_f64()?,
            v.get("z")?.as_f64()?,
        ])
    };
    arr.iter()
        .map(|e| {
            let bb = BoneBox {
                min: v3(e.get("m_Min"))?,
                max: v3(e.get("m_Max"))?,
            };
            bb.valid().then_some(bb)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::FRAC_1_SQRT_2;

    #[test]
    fn conj_x_matches_explicit_product() {
        let m = [
            [1.0, 2.0, 3.0, 4.0],
            [5.0, 6.0, 7.0, 8.0],
            [9.0, 10.0, 11.0, 12.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
        let x = [
            [-1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
        let want = mul(&mul(&x, &m), &x);
        assert_eq!(conj_x(&m), want);
    }

    #[test]
    fn inverse_roundtrips_mirrored_rotation() {
        let t = Trs {
            t: [1.0, -2.0, 0.5],
            r: normalize4([FRAC_1_SQRT_2, 0.0, -FRAC_1_SQRT_2, 0.0]),
            s: [-1.0, -1.0, -1.0],
        };
        let m = trs_to_mat(&t);
        let inv = inverse(&m).unwrap();
        let p = mul(&m, &inv);
        for r in 0..4 {
            for c in 0..4 {
                assert!((p[r][c] - ID[r][c]).abs() < 1e-9, "{p:?}");
            }
        }
    }

    #[test]
    fn linear_rotation_sampling_takes_short_arc() {
        let ch = Channel {
            node: 0,
            path: Path::Rotation,
            interp: Interp::Linear,
            times: vec![0.0, 1.0],
            values: vec![vec![0.0, 0.0, 0.0, 1.0], vec![0.0, 0.0, 0.0, -1.0]],
        };
        let q = ch.sample(0.5).unwrap();
        // Same rotation on both keys; the sign flip must not collapse to zero.
        assert!((q[3].abs() - 1.0).abs() < 1e-9, "{q:?}");
    }

    /// A rig whose rest pose sits far from every animated pose: one root, one
    /// bone translated 10 units along +x for the whole clip.
    fn synthetic_rig() -> (J, Vec<Vec<u8>>) {
        let mut bin: Vec<u8> = Vec::new();
        for t in [0.0f32, 1.0] {
            bin.extend_from_slice(&t.to_le_bytes());
        }
        for v in [10.0f32, 0.0, 0.0, 10.0, 0.0, 0.0] {
            bin.extend_from_slice(&v.to_le_bytes());
        }
        let gltf = serde_json::json!({
            "nodes": [
                {"name": "root", "children": [1]},
                {"name": "bone"}
            ],
            "bufferViews": [
                {"buffer": 0, "byteOffset": 0, "byteLength": 8},
                {"buffer": 0, "byteOffset": 8, "byteLength": 24}
            ],
            "accessors": [
                {"bufferView": 0, "componentType": 5126, "type": "SCALAR", "count": 2},
                {"bufferView": 1, "componentType": 5126, "type": "VEC3", "count": 2}
            ],
            "animations": [{
                "name": "slide",
                "channels": [{"sampler": 0, "target": {"node": 1, "path": "translation"}}],
                "samplers": [{"input": 0, "output": 1, "interpolation": "LINEAR"}]
            }]
        });
        (gltf, vec![bin])
    }

    const UNIT_BOX: BoneBox = BoneBox {
        min: [-1.0, -1.0, -1.0],
        max: [1.0, 1.0, 1.0],
    };

    #[test]
    fn baked_box_spans_rest_pose_and_animated_pose() {
        let (gltf, buffers) = synthetic_rig();
        let boxes = [Some(UNIT_BOX)];
        let input = SkinBoundsInput {
            gltf: &gltf,
            buffers: &buffers,
            joints: &[1],
            root: 0,
            bone_boxes: &boxes,
        };
        let (c, e) = bake_skinned_aabb(&input).expect("baked bounds");
        // Rest: the unit box at the origin. Animated: the same box at x = -10
        // (+10 in glTF, mirrored into Unity space). Union spans x [-11, 1].
        let want_c = [-5.0, 0.0, 0.0];
        let want_e = [6.0, 1.0, 1.0];
        for a in 0..3 {
            assert!((c[a] - want_c[a]).abs() < 1e-9, "center {c:?}");
            assert!((e[a] - want_e[a]).abs() < 1e-9, "extent {e:?}");
        }
    }

    #[test]
    fn rest_pose_alone_is_kept_when_there_are_no_clips() {
        let (mut gltf, buffers) = synthetic_rig();
        gltf.as_object_mut().unwrap().remove("animations");
        let boxes = [Some(UNIT_BOX)];
        let input = SkinBoundsInput {
            gltf: &gltf,
            buffers: &buffers,
            joints: &[1],
            root: 0,
            bone_boxes: &boxes,
        };
        let (c, e) = bake_skinned_aabb(&input).expect("baked bounds");
        for a in 0..3 {
            assert!(c[a].abs() < 1e-9, "center {c:?}");
            assert!((e[a] - 1.0).abs() < 1e-9, "extent {e:?}");
        }
    }

    #[test]
    fn bones_that_influence_nothing_are_skipped() {
        let (gltf, buffers) = synthetic_rig();
        let boxes = [None];
        let input = SkinBoundsInput {
            gltf: &gltf,
            buffers: &buffers,
            joints: &[1],
            root: 0,
            bone_boxes: &boxes,
        };
        assert!(bake_skinned_aabb(&input).is_none());
    }

    #[test]
    fn cyclic_parent_chain_does_not_panic() {
        // node 0 and node 1 are each other's child: `parent` keeps the first
        // link it sees, so the walk up hits the depth guard rather than a root.
        let gltf = serde_json::json!({
            "nodes": [
                {"name": "a", "children": [1]},
                {"name": "b", "children": [0]}
            ]
        });
        let boxes = [Some(UNIT_BOX)];
        let input = SkinBoundsInput {
            gltf: &gltf,
            buffers: &[],
            joints: &[1],
            root: 0,
            bone_boxes: &boxes,
        };
        let _ = bake_skinned_aabb(&input);
    }

    #[test]
    fn cubic_hits_key_values() {
        let ch = Channel {
            node: 0,
            path: Path::Translation,
            interp: Interp::Cubic,
            times: vec![0.0, 2.0],
            values: vec![
                vec![0.0; 3],
                vec![1.0, 2.0, 3.0],
                vec![0.0; 3],
                vec![0.0; 3],
                vec![5.0, 6.0, 7.0],
                vec![0.0; 3],
            ],
        };
        assert_eq!(ch.sample(0.0).unwrap(), vec![1.0, 2.0, 3.0]);
        assert_eq!(ch.sample(2.0).unwrap(), vec![5.0, 6.0, 7.0]);
        let mid = ch.sample(1.0).unwrap();
        assert!((mid[0] - 3.0).abs() < 1e-9, "{mid:?}");
    }
}

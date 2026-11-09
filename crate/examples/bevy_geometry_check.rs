//! Geometry check of `glb-meshopt` variants against their originals.
//!
//! ```text
//! cargo run --release --example bevy-geometry-check -- <bevy-variants-stats.json> <contents-dir> <live-files-dir>
//! ```
//!
//! For every `glb-meshopt` row of the stats file (`entities[].model_files[]`), decodes the
//! variant's meshopt streams and compares each primitive with the original: indices identical,
//! positions within 1 mm (0.001 glTF units), normals and tangents within 1 degree (tangent
//! handedness equal), UVs within 1/16384, every other attribute within 1e-6; "indices identical"
//! means the same triangles in the same order with the same winding (the meshopt index codec may
//! rotate the vertices within a triangle; the summary counts those). Prints a JSON
//! summary with the worst ten rows and exits non-zero on any failure.

use abgen::bevyvariants::meshopt::{load_merged, read_accessor_f32, read_indices, same_triangles};
use gltf_json::validation::Checked;
use gltf_json::{mesh, Root};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const POS_TOL: f64 = 0.001;
const ANGLE_TOL_DEG: f64 = 1.0;
const UV_TOL: f64 = 1.0 / 16384.0;
const OTHER_TOL: f64 = 1e-6;

#[derive(Clone, Debug, serde::Serialize)]
struct Row {
    file: String,
    mesh: usize,
    primitive: usize,
    attribute: String,
    /// Max error over the primitive (units of the attribute; degrees for normals/tangents).
    error: f64,
    tolerance: f64,
    /// `error / tolerance`; above 1 is a failure.
    ratio: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

fn attr_name(sem: &Checked<mesh::Semantic>) -> String {
    match sem {
        Checked::Valid(s) => format!("{s:?}"),
        Checked::Invalid => "invalid".into(),
    }
}

fn angle_deg(a: &[f32], b: &[f32]) -> f64 {
    let n = |v: &[f32]| {
        let l = (v[0] as f64).hypot(v[1] as f64).hypot(v[2] as f64);
        if l > 0.0 {
            [v[0] as f64 / l, v[1] as f64 / l, v[2] as f64 / l]
        } else {
            [0.0, 0.0, 0.0]
        }
    };
    let (a, b) = (n(a), n(b));
    let dot = (a[0] * b[0] + a[1] * b[1] + a[2] * b[2]).clamp(-1.0, 1.0);
    dot.acos().to_degrees()
}

fn max_abs(a: &[Vec<f32>], b: &[Vec<f32>], comps: usize) -> f64 {
    let mut m = 0f64;
    for (ra, rb) in a.iter().zip(b) {
        for c in 0..comps.min(ra.len()).min(rb.len()) {
            m = m.max((ra[c] as f64 - rb[c] as f64).abs());
        }
    }
    m
}

fn load(path: &Path) -> anyhow::Result<(Root, Vec<u8>)> {
    let bytes = std::fs::read(path)?;
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let resolve = move |uri: &str| -> Option<Vec<u8>> {
        let rel: String = uri
            .split('/')
            .map(urlencoding_decode)
            .collect::<Vec<_>>()
            .join("/");
        std::fs::read(dir.join(rel)).ok()
    };
    load_merged(&bytes, Some(&resolve))
}

fn urlencoding_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn check_model(file: &str, orig: &(Root, Vec<u8>), var: &(Root, Vec<u8>), rows: &mut Vec<Row>) {
    let (o, ob) = orig;
    let (v, vb) = var;
    let mut row = |mesh: usize,
                   primitive: usize,
                   attribute: &str,
                   error: f64,
                   tolerance: f64,
                   note: Option<String>| {
        rows.push(Row {
            file: file.to_string(),
            mesh,
            primitive,
            attribute: attribute.to_string(),
            error,
            tolerance,
            ratio: if tolerance > 0.0 {
                error / tolerance
            } else {
                f64::INFINITY
            },
            note,
        });
    };
    if o.meshes.len() != v.meshes.len() {
        row(
            0,
            0,
            "meshes",
            f64::INFINITY,
            1.0,
            Some("mesh count differs".into()),
        );
        return;
    }
    for (mi, (om, vm)) in o.meshes.iter().zip(&v.meshes).enumerate() {
        if om.primitives.len() != vm.primitives.len() {
            row(
                mi,
                0,
                "primitives",
                f64::INFINITY,
                1.0,
                Some("primitive count differs".into()),
            );
            continue;
        }
        for (pi, (op, vp)) in om.primitives.iter().zip(&vm.primitives).enumerate() {
            match (op.indices, vp.indices) {
                (None, None) => {}
                (Some(oi), Some(vi)) => {
                    let a = read_indices(o, ob, oi.value());
                    let b = read_indices(v, vb, vi.value());
                    match (a, b) {
                        // Identical triangles in order and winding; the meshopt index codec
                        // may rotate the vertices within a triangle, which is noted.
                        (Some(a), Some(b)) => match same_triangles(&a, &b) {
                            Ok(rotated) => row(
                                mi,
                                pi,
                                "indices",
                                0.0,
                                0.5,
                                (rotated > 0).then(|| {
                                    format!("{rotated} of {} triangles rotated", a.len() / 3)
                                }),
                            ),
                            Err(e) => row(mi, pi, "indices", 1.0, 0.5, Some(e)),
                        },
                        _ => row(
                            mi,
                            pi,
                            "indices",
                            f64::INFINITY,
                            1.0,
                            Some("unreadable".into()),
                        ),
                    }
                }
                _ => row(
                    mi,
                    pi,
                    "indices",
                    f64::INFINITY,
                    1.0,
                    Some("one side has no indices".into()),
                ),
            }
            for (sem, oa) in &op.attributes {
                let name = attr_name(sem);
                let Some(va) = vp.attributes.get(sem) else {
                    row(
                        mi,
                        pi,
                        &name,
                        f64::INFINITY,
                        1.0,
                        Some("missing in variant".into()),
                    );
                    continue;
                };
                let (Some(a), Some(b)) = (
                    read_accessor_f32(o, ob, oa.value()),
                    read_accessor_f32(v, vb, va.value()),
                ) else {
                    row(mi, pi, &name, f64::INFINITY, 1.0, Some("unreadable".into()));
                    continue;
                };
                if a.len() != b.len() {
                    row(
                        mi,
                        pi,
                        &name,
                        f64::INFINITY,
                        1.0,
                        Some(format!("{} vs {} elements", a.len(), b.len())),
                    );
                    continue;
                }
                match sem {
                    Checked::Valid(mesh::Semantic::Positions) => {
                        row(mi, pi, &name, max_abs(&a, &b, 3), POS_TOL, None)
                    }
                    Checked::Valid(mesh::Semantic::Normals) => {
                        let m = a
                            .iter()
                            .zip(&b)
                            .map(|(x, y)| angle_deg(x, y))
                            .fold(0.0, f64::max);
                        row(mi, pi, &name, m, ANGLE_TOL_DEG, None)
                    }
                    Checked::Valid(mesh::Semantic::Tangents) => {
                        let m = a
                            .iter()
                            .zip(&b)
                            .map(|(x, y)| angle_deg(x, y))
                            .fold(0.0, f64::max);
                        let flips = a
                            .iter()
                            .zip(&b)
                            .filter(|(x, y)| {
                                x.get(3).map(|w| w.signum()) != y.get(3).map(|w| w.signum())
                            })
                            .count();
                        let note = (flips > 0).then(|| format!("{flips} handedness flips"));
                        row(
                            mi,
                            pi,
                            &name,
                            if flips > 0 { f64::INFINITY } else { m },
                            ANGLE_TOL_DEG,
                            note,
                        )
                    }
                    Checked::Valid(mesh::Semantic::TexCoords(_)) => {
                        row(mi, pi, &name, max_abs(&a, &b, 2), UV_TOL, None)
                    }
                    _ => row(mi, pi, &name, max_abs(&a, &b, 16), OTHER_TOL, None),
                }
            }
            for sem in vp.attributes.keys() {
                if !op.attributes.contains_key(sem) {
                    row(
                        mi,
                        pi,
                        &attr_name(sem),
                        f64::INFINITY,
                        1.0,
                        Some("added in variant".into()),
                    );
                }
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 3 {
        eprintln!(
            "usage: bevy-geometry-check <bevy-variants-stats.json> <contents-dir> <live-files-dir>"
        );
        std::process::exit(2);
    }
    let stats: Value =
        serde_json::from_slice(&std::fs::read(&args[0]).expect("read stats")).expect("parse stats");
    let contents = PathBuf::from(&args[1]);
    let live = PathBuf::from(&args[2]);
    let mut rows: Vec<Row> = Vec::new();
    let mut models = 0usize;
    let mut unreadable: Vec<String> = Vec::new();
    let entities = stats
        .get("entities")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_else(|| vec![stats.clone()]);
    for ent in &entities {
        for m in ent
            .get("model_files")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if m.get("format").and_then(Value::as_str) != Some("glb-meshopt") {
                continue;
            }
            let (Some(file), Some(cid)) = (
                m.get("file").and_then(Value::as_str),
                m.get("variant_cid").and_then(Value::as_str),
            ) else {
                continue;
            };
            models += 1;
            let orig = load(&live.join(file));
            let var = load(&contents.join(cid));
            match (orig, var) {
                (Ok(o), Ok(v)) => check_model(file, &o, &v, &mut rows),
                (Err(e), _) => unreadable.push(format!("{file}: original: {e:#}")),
                (_, Err(e)) => unreadable.push(format!("{file}: variant {cid}: {e:#}")),
            }
        }
    }
    let mut per_attr: BTreeMap<String, (f64, f64, usize)> = BTreeMap::new();
    for r in &rows {
        let key = r
            .attribute
            .split('(')
            .next()
            .unwrap_or(&r.attribute)
            .to_string();
        let e = per_attr.entry(key).or_insert((0.0, r.tolerance, 0));
        e.0 = e.0.max(r.error);
        e.2 += 1;
    }
    let failures: Vec<&Row> = rows.iter().filter(|r| r.ratio > 1.0).collect();
    let mut worst: Vec<&Row> = rows.iter().collect();
    worst.sort_by(|a, b| {
        b.ratio
            .partial_cmp(&a.ratio)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    worst.truncate(10);
    let primitives = rows
        .iter()
        .map(|r| (r.file.as_str(), r.mesh, r.primitive))
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let ok = failures.is_empty() && unreadable.is_empty();
    let out = json!({
        "ok": ok,
        "models": models,
        "primitives": primitives,
        "checks": rows.len(),
        "failures": failures.len(),
        "unreadable": unreadable,
        "tolerances": {"positions": POS_TOL, "normals_deg": ANGLE_TOL_DEG, "tangents_deg": ANGLE_TOL_DEG, "texcoords": UV_TOL, "other": OTHER_TOL, "indices": "same triangles, order and winding (vertex rotation within a triangle allowed)"},
        "rotated_triangle_primitives": rows.iter().filter(|r| r.attribute == "indices" && r.note.as_deref().is_some_and(|n| n.contains("rotated"))).count(),
        "max_error_by_attribute": per_attr.iter().map(|(k, (e, t, n))| (k.clone(), json!({"max_error": e, "tolerance": t, "primitives": n}))).collect::<serde_json::Map<_, _>>(),
        "worst": worst,
        "failed": failures,
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
    if !ok {
        std::process::exit(1);
    }
}

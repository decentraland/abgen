//! Handedness conformance of the LOD assembly frame against the production
//! `lods-unity/lods/<sceneId>_1.glb` references (Tea Park, Mount AMAIXEN).
//!
//! The `*.refglb.json` fixtures pin the reference GLB's world AABB together with
//! the node translation/scale and accessor min/max it was dequantized from
//! (`scripts/lod-refglb-fixture.py`); the `conformance/*.InitialSceneState.json` fixtures are
//! the production descriptors verbatim. A descriptor placement expressed in glTF
//! space is `T(-tx, ty, tz) * R(qx, -qy, -qz, qw) * S(s)`,
//! so every mirrored translation has to land inside the reference AABB while the
//! un-mirrored one does not.
//!
//! ```sh
//! cargo test --release --test lod_handedness
//! cargo test --release --test lod_handedness -- --ignored   # assembles from the catalyst
//! ```

use abgen::lodgen::crop::crop_rect_rh;
use abgen::lodgen::placements::{parse_iss, Placement};
use serde_json::Value;

const TEAPARK_ISS: &str =
    include_str!("../src/lodgen/testdata/conformance/teapark.InitialSceneState.json");
const TEAPARK_REF: &str = include_str!("../src/lodgen/testdata/handedness/teapark.refglb.json");
const AMAIXEN_ISS: &str =
    include_str!("../src/lodgen/testdata/conformance/amaixen.InitialSceneState.json");
const AMAIXEN_REF: &str = include_str!("../src/lodgen/testdata/handedness/amaixen.refglb.json");

/// Margin, in metres on x and z, around the reference AABB that the mirrored
/// descriptor translations must fall inside: a placement pivot can sit a little
/// outside the geometry the crop kept.
const XZ_MARGIN: f64 = 8.0;
/// Tolerance for the recorded AABB extremes quoted in this file.
const AABB_TOL: f64 = 0.05;

struct RefGlb {
    tag: String,
    scene_id: String,
    base: (i32, i32),
    parcels: Vec<(i32, i32)>,
    rect_x: [f64; 2],
    rect_z: [f64; 2],
    aabb_min: [f64; 3],
    aabb_max: [f64; 3],
    iss_assets: usize,
    doc: Value,
}

fn f3(v: &Value) -> [f64; 3] {
    let a = v.as_array().expect("3-vector");
    [
        a[0].as_f64().unwrap(),
        a[1].as_f64().unwrap(),
        a[2].as_f64().unwrap(),
    ]
}

fn f2(v: &Value) -> [f64; 2] {
    let a = v.as_array().expect("2-vector");
    [a[0].as_f64().unwrap(), a[1].as_f64().unwrap()]
}

fn parcel(v: &Value) -> (i32, i32) {
    let a = v.as_array().expect("parcel pair");
    (a[0].as_i64().unwrap() as i32, a[1].as_i64().unwrap() as i32)
}

fn load_ref(text: &str) -> RefGlb {
    let doc: Value = serde_json::from_str(text).expect("refglb.json parses");
    RefGlb {
        tag: doc["tag"].as_str().unwrap().to_string(),
        scene_id: doc["sceneId"].as_str().unwrap().to_string(),
        base: parcel(&doc["base"]),
        parcels: doc["parcels"]
            .as_array()
            .unwrap()
            .iter()
            .map(parcel)
            .collect(),
        rect_x: f2(&doc["parcelRectRh"]["x"]),
        rect_z: f2(&doc["parcelRectRh"]["z"]),
        aabb_min: f3(&doc["aabb"]["min"]),
        aabb_max: f3(&doc["aabb"]["max"]),
        iss_assets: doc["iss"]["assets"].as_u64().unwrap() as usize,
        doc,
    }
}

/// Re-derives the AABB from the recorded node translation/scale and accessor
/// min/max: `translation + scale * q` per axis, which is the whole dequantization
/// when no node in the chain rotates.
fn recomputed_aabb(r: &RefGlb) -> ([f64; 3], [f64; 3]) {
    assert!(
        !r.doc["chainHasRotation"].as_bool().unwrap(),
        "{}: node chain rotates; the per-axis formula does not apply",
        r.tag
    );
    let nodes = r.doc["nodes"].as_array().unwrap();
    let mut mn = [f64::INFINITY; 3];
    let mut mx = [f64::NEG_INFINITY; 3];
    for prim in r.doc["primitives"].as_array().unwrap() {
        let node_idx = prim["node"].as_u64().unwrap();
        let node = nodes
            .iter()
            .find(|n| n["index"].as_u64() == Some(node_idx))
            .expect("primitive's node recorded");
        assert!(
            node["ancestorsIdentity"].as_bool().unwrap(),
            "{}: ancestors carry a transform",
            r.tag
        );
        let t = f3(&node["translation"]);
        let s = f3(&node["scale"]);
        let lo = f3(&prim["min"]);
        let hi = f3(&prim["max"]);
        for i in 0..3 {
            let (a, b) = (t[i] + s[i] * lo[i], t[i] + s[i] * hi[i]);
            mn[i] = mn[i].min(a.min(b));
            mx[i] = mx[i].max(a.max(b));
        }
        assert_eq!(
            prim["componentType"].as_u64(),
            Some(5123),
            "{}: KHR_mesh_quantization u16 positions",
            r.tag
        );
    }
    (mn, mx)
}

/// Translation of a descriptor placement in glTF space: `(-tx, ty, tz)`.
fn gltf_translation(p: &Placement) -> [f64; 3] {
    [-p.position[0], p.position[1], p.position[2]]
}

fn inside_xz_expanded(r: &RefGlb, t: [f64; 3]) -> bool {
    t[0] >= r.aabb_min[0] - XZ_MARGIN
        && t[0] <= r.aabb_max[0] + XZ_MARGIN
        && t[2] >= r.aabb_min[2] - XZ_MARGIN
        && t[2] <= r.aabb_max[2] + XZ_MARGIN
        && t[1] >= r.aabb_min[1]
        && t[1] <= r.aabb_max[1]
}

fn check_scene(iss_text: &str, ref_text: &str) -> (RefGlb, Vec<Placement>) {
    let r = load_ref(ref_text);
    let placements = parse_iss(iss_text.as_bytes()).expect("ISS parses");
    assert_eq!(placements.len(), r.iss_assets, "{}: ISS asset count", r.tag);
    assert!(
        placements.iter().all(|p| p.scale.iter().all(|s| *s != 0.0)),
        "{}: no zero-scale placements in this ISS",
        r.tag
    );

    let (mn, mx) = recomputed_aabb(&r);
    for i in 0..3 {
        assert!(
            (mn[i] - r.aabb_min[i]).abs() < 1e-9,
            "{}: aabb min[{i}] {} vs recorded {}",
            r.tag,
            mn[i],
            r.aabb_min[i]
        );
        assert!(
            (mx[i] - r.aabb_max[i]).abs() < 1e-9,
            "{}: aabb max[{i}] {} vs recorded {}",
            r.tag,
            mx[i],
            r.aabb_max[i]
        );
    }

    // The crop rect is the parcel rect mirrored on X; the reference lies inside it.
    let rect = crop_rect_rh(r.base, &r.parcels);
    assert_eq!(
        rect,
        [r.rect_x[0], r.rect_x[1], r.rect_z[0], r.rect_z[1]],
        "{}: parcel rect",
        r.tag
    );
    assert!(
        r.aabb_min[0] >= rect[0] - AABB_TOL && r.aabb_max[0] <= rect[1] + AABB_TOL,
        "{}: bbox x {:?}..{:?} outside rect {rect:?}",
        r.tag,
        r.aabb_min,
        r.aabb_max
    );
    assert!(
        r.aabb_min[2] >= rect[2] - AABB_TOL && r.aabb_max[2] <= rect[3] + AABB_TOL,
        "{}: bbox z {:?}..{:?} outside rect {rect:?}",
        r.tag,
        r.aabb_min,
        r.aabb_max
    );

    // Every mirrored translation is inside the reference bbox (+8 m on x/z) ...
    let outside: Vec<String> = placements
        .iter()
        .enumerate()
        .filter(|(_, p)| !inside_xz_expanded(&r, gltf_translation(p)))
        .map(|(i, p)| format!("{i}: {:?} -> {:?}", p.position, gltf_translation(p)))
        .collect();
    assert!(
        outside.is_empty(),
        "{}: placements outside the reference bbox {:?}..{:?}:\n{}",
        r.tag,
        r.aabb_min,
        r.aabb_max,
        outside.join("\n")
    );
    // ... and the un-mirrored frame does not fit, so the check discriminates handedness.
    assert!(
        placements
            .iter()
            .any(|p| !inside_xz_expanded(&r, p.position)),
        "{}: the un-mirrored translations also fit; the bbox check would not catch an X flip",
        r.tag
    );
    (r, placements)
}

#[test]
fn teapark_reference_bbox_contains_iss_placements() {
    let (r, placements) = check_scene(TEAPARK_ISS, TEAPARK_REF);
    assert_eq!(
        r.scene_id,
        "bafkreifed6j4zxjdv72sxyupsz3kj4mf6hogxaccvwcdmfvribogg3waxa"
    );
    assert_eq!(placements.len(), 77);
    // The reference fills the parcel rect exactly: x [-80, 0], z [-32, 112].
    assert!(
        (r.aabb_min[0] - -80.0).abs() < AABB_TOL,
        "bbox min x {}",
        r.aabb_min[0]
    );
    assert!(
        (r.aabb_max[0] - 0.0).abs() < AABB_TOL,
        "bbox max x {}",
        r.aabb_max[0]
    );
    assert!(
        (r.aabb_min[2] - -32.0).abs() < AABB_TOL,
        "bbox min z {}",
        r.aabb_min[2]
    );
    assert!(
        (r.aabb_max[2] - 112.0).abs() < AABB_TOL,
        "bbox max z {}",
        r.aabb_max[2]
    );
}

#[test]
fn amaixen_reference_bbox_contains_iss_placements() {
    let (r, placements) = check_scene(AMAIXEN_ISS, AMAIXEN_REF);
    assert_eq!(
        r.scene_id,
        "bafkreiceqm43l33evsc43jtotf2fs27efizwxn76cdnd3ypd6mcsdnpf6a"
    );
    assert_eq!(placements.len(), 26);
    // The reference sits inside its 5x5 rect x [-80, 0], z [0, 80].
    let want_min = [-78.99, -0.85, 0.34];
    let want_max = [-2.30, 88.69, 77.52];
    for i in 0..3 {
        assert!(
            (r.aabb_min[i] - want_min[i]).abs() < AABB_TOL,
            "bbox min[{i}] {}",
            r.aabb_min[i]
        );
        assert!(
            (r.aabb_max[i] - want_max[i]).abs() < AABB_TOL,
            "bbox max[{i}] {}",
            r.aabb_max[i]
        );
    }
}

/// Assembles both scenes from the catalyst with their production ISS, crops to the
/// parcels' bounding rect (mirrored on X) and compares the world AABB with the
/// reference GLB: per-axis extremes
/// within 1.0 m and IoU >= 0.95. Needs network; `ABGEN_CATALYST` overrides the
/// content server, `ABGEN_LOD_CACHE` a directory for fetched GLBs.
#[test]
#[ignore = "network: assembles Tea Park and AMAIXEN from the catalyst"]
fn reference_glb_aabb_matches_assembly() {
    use abgen::catalyst::CatalystClient;
    use abgen::lodgen::assemble::assemble;
    use abgen::lodgen::crop::crop;
    use abgen::lodgen::model::MatLane;
    use abgen::lodgen::scene_geometry;

    let catalyst = std::env::var("ABGEN_CATALYST")
        .unwrap_or_else(|_| "https://peer.decentraland.org/content".to_string());
    let cache = std::env::var_os("ABGEN_LOD_CACHE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("abgen-lod-handedness-cache"));
    std::fs::create_dir_all(&cache).unwrap();
    let client = CatalystClient::new(&catalyst);

    for (iss_text, ref_text) in [(TEAPARK_ISS, TEAPARK_REF), (AMAIXEN_ISS, AMAIXEN_REF)] {
        let r = load_ref(ref_text);
        let scene = client
            .fetch_entity(&r.scene_id)
            .unwrap_or_else(|e| panic!("{}: fetch entity {}: {e:#}", r.tag, r.scene_id));
        let placements = parse_iss(iss_text.as_bytes()).unwrap();
        let mut model = assemble(
            &client,
            &scene,
            &placements,
            1,
            Some(&cache),
            MatLane::default(),
        )
        .unwrap_or_else(|e| panic!("{}: assemble: {e:#}", r.tag));
        let (base, parcels) = scene_geometry(&scene).unwrap();
        assert_eq!(base, r.base, "{}: base parcel", r.tag);
        // The production GLB spans the parcels' bounding rect (Tea Park owns 28 of the
        // 45 parcels in its 5x9 rect and still fills x [-80, 0], z [-32, 112]), so the
        // comparison crops to that single rect, not to the per-cell rects `generate` uses.
        let report = crop(&mut model, &[crop_rect_rh(base, &parcels)]);
        let (mn, mx) = model.bounds();
        let (mn, mx) = (mn.map(|v| v as f64), mx.map(|v| v as f64));
        eprintln!(
            "{}: crop {} | assembled bbox {mn:?}..{mx:?} | reference {:?}..{:?}",
            r.tag,
            report.summary(),
            r.aabb_min,
            r.aabb_max
        );
        for i in 0..3 {
            assert!(
                (mn[i] - r.aabb_min[i]).abs() <= 1.0,
                "{}: min[{i}] {} vs reference {}",
                r.tag,
                mn[i],
                r.aabb_min[i]
            );
            assert!(
                (mx[i] - r.aabb_max[i]).abs() <= 1.0,
                "{}: max[{i}] {} vs reference {}",
                r.tag,
                mx[i],
                r.aabb_max[i]
            );
        }
        let vol = |a: [f64; 3], b: [f64; 3]| {
            (b[0] - a[0]).max(0.0) * (b[1] - a[1]).max(0.0) * (b[2] - a[2]).max(0.0)
        };
        let imin = [
            mn[0].max(r.aabb_min[0]),
            mn[1].max(r.aabb_min[1]),
            mn[2].max(r.aabb_min[2]),
        ];
        let imax = [
            mx[0].min(r.aabb_max[0]),
            mx[1].min(r.aabb_max[1]),
            mx[2].min(r.aabb_max[2]),
        ];
        let inter = vol(imin, imax);
        let union = vol(mn, mx) + vol(r.aabb_min, r.aabb_max) - inter;
        let iou = inter / union;
        eprintln!("{}: bbox IoU {iou:.4}", r.tag);
        assert!(iou >= 0.95, "{}: bbox IoU {iou:.4} < 0.95", r.tag);
    }
}

use super::*;
use crate::lodgen::emit::{emit_empty_glb, emit_glb};
use crate::lodgen::model::{AlphaClass, LodImage, LodMaterial, LodModel, LodPrimitive};

#[test]
fn level_path_formatting() {
    assert_eq!(staged_glb_name("BafkReiX", 1), "bafkreix_1.glb");
    assert_eq!(
        staged_glb_name("qmccggwqvb7v3b3vqxajzcjimmzhzrrvmk3ulkt6qxsesd", 1),
        "qmccggwqvb7v3b3vqxajzcjimmzhzrrvmk3ulkt6qxsesd_1.glb"
    );
    assert_eq!(
        expected_rel_path("BafkReiX", 1, "windows"),
        "LOD/1/bafkreix_1_windows"
    );
    assert_eq!(expected_rel_path("scene", 0, "mac"), "LOD/0/scene_0_mac");
}

#[test]
fn choose_lane_level_0_is_always_passthrough() {
    for tri_cap in [None, Some(100u64), Some(1_000_000u64)] {
        for tri_cap_auto in [false, true] {
            for source_tris in [0usize, 400, 500, 501, 5_000_000] {
                for ratio in [0.1, 1.0] {
                    assert_eq!(
                        choose_lane(0, tri_cap, tri_cap_auto, ratio, source_tris, 500),
                        SimplifyLane::Passthrough,
                        "tri_cap={tri_cap:?} auto={tri_cap_auto} tris={source_tris} ratio={ratio}"
                    );
                }
            }
        }
    }
}

#[test]
fn choose_lane_level_1_matrix() {
    assert_eq!(
        choose_lane(1, None, false, 0.1, 400, 500),
        SimplifyLane::Passthrough
    );
    assert_eq!(
        choose_lane(1, None, false, 0.1, 500, 500),
        SimplifyLane::Passthrough
    );
    assert_eq!(
        choose_lane(1, None, false, 0.1, 501, 500),
        SimplifyLane::Capped {
            ratio: 0.1,
            cap: 500
        }
    );
    assert_eq!(
        choose_lane(1, None, false, 0.25, 5_000_000, 1500),
        SimplifyLane::Capped {
            ratio: 0.25,
            cap: 1500
        }
    );
    assert_eq!(
        choose_lane(1, Some(250), false, 0.25, 400, 500),
        SimplifyLane::Capped {
            ratio: 0.25,
            cap: 250
        }
    );
    assert_eq!(
        choose_lane(1, Some(250), false, 0.1, 5_000_000, 500),
        SimplifyLane::Capped {
            ratio: 0.1,
            cap: 250
        }
    );
    assert_eq!(
        choose_lane(1, None, true, 0.1, 5_000_000, 1500),
        SimplifyLane::Capped {
            ratio: 0.1,
            cap: 1500
        }
    );
    assert_eq!(
        choose_lane(1, None, true, 0.1, 400, 500),
        SimplifyLane::Passthrough
    );
    assert_eq!(
        choose_lane(1, None, true, 0.1, 500, 500),
        SimplifyLane::Passthrough
    );
    assert_eq!(
        choose_lane(1, None, true, 0.1, 501, 500),
        SimplifyLane::Capped {
            ratio: 0.1,
            cap: 500
        }
    );
    assert_eq!(
        choose_lane(1, Some(500), false, 0.1, 400, 9999),
        SimplifyLane::Passthrough
    );
    assert_eq!(
        choose_lane(1, Some(9), true, 0.1, 400, 1500),
        SimplifyLane::Passthrough
    );
    assert_eq!(
        choose_lane(1, Some(9), true, 0.1, 1501, 1500),
        SimplifyLane::Capped {
            ratio: 0.1,
            cap: 1500
        }
    );
}

#[test]
fn default_params_cap_tris_to_auto_budget() {
    let p = GenerateParams::default();
    assert!(p.tri_cap_auto);
    assert_eq!(p.tri_cap, None);
    assert_eq!(p.levels, vec![0, 1]);
}

#[test]
fn simplifier_backend_parse_and_names() {
    use super::simplify::SimplifierBackend;
    assert_eq!(
        SimplifierBackend::parse("meshopt").unwrap(),
        SimplifierBackend::Meshopt
    );
    assert_eq!(
        SimplifierBackend::parse(" GLTFPACK ").unwrap(),
        SimplifierBackend::Gltfpack
    );
    let msg = format!("{:#}", SimplifierBackend::parse("pixyz").unwrap_err());
    assert!(msg.contains("meshopt|gltfpack"), "{msg}");
    assert_eq!(SimplifierBackend::Meshopt.name(), "meshopt");
    assert_eq!(SimplifierBackend::Gltfpack.name(), "gltfpack");
}

#[test]
fn normalize_levels_dedupes_and_refuses() {
    assert_eq!(normalize_levels(&[0, 1]).unwrap(), vec![0, 1]);
    assert_eq!(normalize_levels(&[1, 0, 1, 0]).unwrap(), vec![1, 0]);
    assert_eq!(normalize_levels(&[1]).unwrap(), vec![1]);
    let msg = format!("{:#}", normalize_levels(&[]).unwrap_err());
    assert!(msg.contains("at least one"), "{msg}");
    let msg = format!("{:#}", normalize_levels(&[0, 1, 2]).unwrap_err());
    assert!(msg.contains("level 2"), "{msg}");
    let msg = format!("{:#}", normalize_levels(&[7]).unwrap_err());
    assert!(msg.contains("level 7"), "{msg}");
}

#[test]
fn effective_tri_cap_table() {
    assert_eq!(effective_tri_cap(0, Some(100), true, 500), None);
    assert_eq!(effective_tri_cap(0, None, true, 500), None);
    assert_eq!(effective_tri_cap(1, None, true, 500), Some(500));
    assert_eq!(effective_tri_cap(1, Some(100), true, 500), Some(500));
    assert_eq!(effective_tri_cap(1, Some(100), false, 500), Some(100));
    assert_eq!(effective_tri_cap(1, None, false, 500), None);
}

#[test]
fn crop_union_and_orphan_stats_feed_the_gate() {
    let base = (0, 0);
    let parcels = vec![(0, 0), (1, 0), (0, 1)];
    let rects = crop::crop_rects_rh(base, &parcels);
    assert_eq!(rects.len(), 2);
    let mut model = LodModel {
        root_name: "gate-stats".to_string(),
        primitives: vec![LodPrimitive {
            positions: vec![
                [-2.0, 0.0, 2.0],
                [-4.0, 0.0, 2.0],
                [-2.0, 1.0, 4.0],
                [-99.0, 0.0, 99.0],
            ],
            normals: vec![[0.0, 1.0, 0.0]; 4],
            uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2],
            material: 0,
            ..Default::default()
        }],
        materials: Vec::new(),
        images: Vec::new(),
        log: Vec::new(),
    };
    let report = crop::crop(&mut model, &rects);
    assert_eq!(report.rects, 2);
    assert_eq!(report.tris_out, 1);
    assert_eq!(report.verts_dropped, 1);
    let stats = crop::union_stats(&model, &rects, 1e-3);
    assert_eq!(stats.rects, 2);
    assert_eq!(stats.buffer_verts, 3);
    assert_eq!(stats.referenced_verts, 3);
    assert_eq!(stats.outside, 0);
    assert_eq!(stats.outside_fraction(), 0.0);
}

#[test]
fn tri_cap_gate_check_pass_fail_waiver() {
    let pass = gate::tri_cap_check(500, 500, false);
    assert!(pass.ok);
    assert_eq!(pass.label, "tri-cap");
    assert!(
        pass.detail.contains("500 tris <= cap 500"),
        "{}",
        pass.detail
    );

    let fail = gate::tri_cap_check(500, 501, false);
    assert!(!fail.ok);
    assert_eq!(gate_failures(&[pass, fail]), 1);

    let waived = gate::tri_cap_check(500, 3519, true);
    assert!(waived.ok);
    assert!(waived.detail.contains("WAIVED"), "{}", waived.detail);
    assert!(
        waived.detail.contains("--allow-unsimplified"),
        "{}",
        waived.detail
    );
}

#[test]
fn generate_refuses_level_2() {
    let params = GenerateParams {
        scene: "0,0".to_string(),
        levels: vec![2],
        ..Default::default()
    };
    let err = generate(&params).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("level 2"), "{msg}");

    let params = GenerateParams {
        scene: "0,0".to_string(),
        levels: Vec::new(),
        ..Default::default()
    };
    let msg = format!("{:#}", generate(&params).unwrap_err());
    assert!(msg.contains("at least one"), "{msg}");
}

#[test]
fn generate_refuses_webgl_platform() {
    let params = GenerateParams {
        scene: "0,0".to_string(),
        platform: "webgl".to_string(),
        ..Default::default()
    };
    let msg = format!("{:#}", generate(&params).unwrap_err());
    assert!(msg.contains("webgl"), "{msg}");

    let params = GenerateParams {
        scene: "0,0".to_string(),
        platforms: vec!["windows".to_string(), "webgl".to_string()],
        ..Default::default()
    };
    let msg = format!("{:#}", generate(&params).unwrap_err());
    assert!(msg.contains("webgl"), "{msg}");

    let params = GenerateParams {
        scene: "0,0".to_string(),
        platforms: vec!["amiga".to_string()],
        ..Default::default()
    };
    let msg = format!("{:#}", generate(&params).unwrap_err());
    assert!(msg.contains("amiga"), "{msg}");
}

fn tiny_png() -> Vec<u8> {
    let mut img = image::RgbaImage::new(4, 4);
    for (i, p) in img.pixels_mut().enumerate() {
        *p = image::Rgba([(i * 16) as u8, 128, 200, 255]);
    }
    let mut cur = std::io::Cursor::new(Vec::new());
    img.write_to(&mut cur, image::ImageFormat::Png).unwrap();
    cur.into_inner()
}

fn synthetic_glb() -> Vec<u8> {
    emit_glb(&LodModel {
        root_name: "synthetic".to_string(),
        primitives: vec![LodPrimitive {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: vec![[0.0, 0.0, 1.0]; 3],
            uvs: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
            indices: vec![0, 1, 2],
            material: 0,
            ..Default::default()
        }],
        materials: vec![LodMaterial {
            name: "TextureBakeResult-mat".to_string(),
            class: AlphaClass::Opaque,
            base_color: [1.0, 1.0, 1.0, 1.0],
            cutoff: 0.5,
            image: Some(0),
            double_sided: false,
        }],
        images: vec![LodImage {
            bytes: tiny_png(),
            mime: "image/png".to_string(),
        }],
        log: Vec::new(),
    })
    .unwrap()
}

#[test]
fn empty_scene_bundle_passes_empty_gate_and_fails_content_gate() {
    std::env::set_var(
        "ABGEN_ROOT",
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap(),
    );
    let dir = std::env::temp_dir().join(format!("abgen-lod-emptygate-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sid = "bafkreiemptyscene";
    let src = dir.join(format!("{sid}_1.glb"));
    std::fs::write(&src, emit_empty_glb(&format!("{sid}_1")).unwrap()).unwrap();

    let client = CatalystClient::new("http://127.0.0.1:9");
    let opts = lods::LodOptions {
        platform: "windows".to_string(),
        lod: Some(lods::LodGenMeta {
            parcels: vec![(55, -76)],
            base: (55, -76),
            timestamp: None,
            vertical_override: None,
        }),
        ..Default::default()
    };
    let out = dir.join("out");
    let conv = lods::convert_lods(
        &client,
        &[src.to_string_lossy().into_owned()],
        out.to_str().unwrap(),
        &opts,
    )
    .unwrap();
    assert_eq!(conv.results.len(), 1);
    let bundle_path = out.join(sid).join(&conv.results[0].rel_path);
    let data = std::fs::read(&bundle_path).unwrap();

    let checks = self_gate_bundle_with(&data, sid, 1, "windows", false, None).unwrap();
    for c in &checks {
        assert!(c.ok, "unexpected FAIL {}: {}", c.label, c.detail);
    }
    // Nonzero base (55,-76): the bundled root must still sit at the origin.
    assert!(
        checks.iter().any(|c| c.label == "root-position" && c.ok),
        "root-position gate missing"
    );
    let as_content = self_gate_bundle_with(&data, sid, 1, "windows", true, None).unwrap();
    let failed: Vec<&str> = as_content
        .iter()
        .filter(|c| !c.ok)
        .map(|c| c.label.as_str())
        .collect();
    assert!(failed.contains(&"material-count"), "{failed:?}");
    assert!(failed.contains(&"texture-count"), "{failed:?}");
    assert!(failed.contains(&"metadata-deps"), "{failed:?}");

    let (iss_path, iss_assets, iss_skipped) =
        write_iss_descriptor(&out, sid, &[], &HashMap::new()).unwrap();
    assert_eq!((iss_assets, iss_skipped), (0, 0));
    assert_eq!(
        iss_path,
        out.join(sid)
            .join(format!("{sid}{}", placements::ISS_SUFFIX))
    );
    let bytes = std::fs::read(&iss_path).unwrap();
    let doc: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(doc["assets"], serde_json::json!([]));
    assert_eq!(doc["sceneId"], serde_json::json!(sid));
    assert_eq!(doc["version"], serde_json::json!(1));
    assert!(placements::parse_iss(&bytes).unwrap().is_empty());
    let mut br = iss_path.as_os_str().to_owned();
    br.push(".br");
    assert!(PathBuf::from(br).is_file());

    let _ = std::fs::remove_dir_all(&dir);
}

fn first_target_platform(data: &[u8]) -> i32 {
    let bundle = Bundle::load_bytes(data).unwrap();
    for file in &bundle.files {
        if let FileContent::Serialized(sf) = &file.content {
            return sf.target_platform;
        }
    }
    panic!("no serialized file in bundle");
}

#[test]
fn multi_platform_bundles_union_manifest_and_target_platform_gate() {
    std::env::set_var(
        "ABGEN_ROOT",
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap(),
    );
    let dir = std::env::temp_dir().join(format!("abgen-lod-multiplat-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sid = "bafkreimultiplatsynthetic";
    let src = dir.join(format!("{sid}_1.glb"));
    std::fs::write(&src, synthetic_glb()).unwrap();

    let client = CatalystClient::new("http://127.0.0.1:9");
    let opts = lods::LodOptions {
        platform: "windows".to_string(),
        lod: Some(lods::LodGenMeta {
            parcels: vec![(8, -83)],
            base: (8, -83),
            timestamp: None,
            vertical_override: None,
        }),
        ..Default::default()
    };
    let platforms: Vec<String> = ["windows", "mac", "linux"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let out = dir.join("out");
    let conv = lods::convert_lods_platforms(
        &client,
        &[src.to_string_lossy().into_owned()],
        out.to_str().unwrap(),
        &opts,
        &platforms,
    )
    .unwrap();
    assert!(conv.skipped.is_empty(), "{:?}", conv.skipped);
    assert_eq!(conv.results.len(), 3);

    let mut datas: HashMap<&str, Vec<u8>> = HashMap::new();
    for (plat, want_tp) in [("windows", 19), ("mac", 2), ("linux", 24)] {
        let rel = expected_rel_path(sid, 1, plat);
        assert!(
            conv.results.iter().any(|r| r.rel_path == rel),
            "{rel} missing from results"
        );
        let path = out.join(sid).join(&rel);
        let data = std::fs::read(&path).unwrap();
        assert_eq!(first_target_platform(&data), want_tp, "{plat}");
        let mut br = path.as_os_str().to_owned();
        br.push(".br");
        assert!(PathBuf::from(br).is_file(), "{plat} .br sidecar missing");
        let checks = self_gate_bundle(&data, sid, 1, plat).unwrap();
        for c in &checks {
            assert!(c.ok, "{plat} unexpected FAIL {}: {}", c.label, c.detail);
        }
        let tp = checks
            .iter()
            .find(|c| c.label == "target-platform")
            .unwrap();
        assert!(
            tp.detail.contains(&format!("Some({want_tp})")),
            "{plat}: {}",
            tp.detail
        );
        let dep = checks
            .iter()
            .find(|c| c.label == "assetbundle-dep")
            .unwrap();
        let want_cab =
            crate::cabname::cab_name(&crate::shader::texarray_bundle_name(plat)).to_lowercase();
        assert!(dep.detail.contains(&want_cab), "{plat}: {}", dep.detail);
        if plat == "mac" {
            assert!(
                dep.detail.contains("cab-2f95afafeab990fc349e5ab530941444"),
                "{}",
                dep.detail
            );
        }
        datas.insert(plat, data);
    }
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.join(sid).join("LOD.manifest.json")).unwrap())
            .unwrap();
    let files: Vec<String> = manifest["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        files,
        vec![
            format!("LOD/1/{sid}_1_linux"),
            format!("LOD/1/{sid}_1_mac"),
            format!("LOD/1/{sid}_1_windows"),
        ]
    );
    assert_eq!(manifest["levels"], serde_json::json!([1]));
    assert_eq!(manifest["sceneId"], serde_json::json!(sid));
    assert_eq!(manifest["exitCode"], serde_json::json!(0));

    let out_single = dir.join("out-single");
    let conv_single = lods::convert_lods(
        &client,
        &[src.to_string_lossy().into_owned()],
        out_single.to_str().unwrap(),
        &opts,
    )
    .unwrap();
    assert_eq!(conv_single.results.len(), 1);
    let single_bundle = std::fs::read(
        out_single
            .join(sid)
            .join(expected_rel_path(sid, 1, "windows")),
    )
    .unwrap();
    assert_eq!(
        &single_bundle, &datas["windows"],
        "windows bundle bytes differ between convert_lods and convert_lods_platforms"
    );
    let single_manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(out_single.join(sid).join("LOD.manifest.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        single_manifest["files"],
        serde_json::json!([format!("LOD/1/{sid}_1_windows")])
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn multi_level_sources_build_both_levels_from_one_bake() {
    std::env::set_var(
        "ABGEN_ROOT",
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap(),
    );
    let dir =
        std::env::temp_dir().join(format!("abgen-lod-multilevel-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sid = "bafkreimultilevelsynthetic";
    let glb = synthetic_glb();
    let mut sources = Vec::new();
    for level in [0u32, 1] {
        let src = dir.join(staged_glb_name(sid, level));
        std::fs::write(&src, &glb).unwrap();
        sources.push(src.to_string_lossy().into_owned());
    }

    let client = CatalystClient::new("http://127.0.0.1:9");
    let opts = lods::LodOptions {
        platform: "windows".to_string(),
        lod: Some(lods::LodGenMeta {
            parcels: vec![(8, -83)],
            base: (8, -83),
            timestamp: None,
            vertical_override: None,
        }),
        ..Default::default()
    };
    let out = dir.join("out");
    let conv = lods::convert_lods_platforms(
        &client,
        &sources,
        out.to_str().unwrap(),
        &opts,
        &["windows".to_string()],
    )
    .unwrap();
    assert!(conv.skipped.is_empty(), "{:?}", conv.skipped);
    assert_eq!(conv.results.len(), 2);
    assert_eq!(conv.scene_id, sid);
    for level in [0u32, 1] {
        let rel = expected_rel_path(sid, level, "windows");
        assert!(
            conv.results
                .iter()
                .any(|r| r.rel_path == rel && r.level == level),
            "{rel} missing"
        );
        let path = out.join(sid).join(&rel);
        let data = std::fs::read(&path).unwrap();
        let checks = self_gate_bundle(&data, sid, level, "windows").unwrap();
        for c in &checks {
            assert!(c.ok, "L{level} unexpected FAIL {}: {}", c.label, c.detail);
        }
        let mut br = path.as_os_str().to_owned();
        br.push(".br");
        assert!(PathBuf::from(br).is_file(), "L{level} .br sidecar missing");
    }
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.join(sid).join("LOD.manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["levels"], serde_json::json!([0, 1]));
    assert_eq!(
        manifest["files"],
        serde_json::json!([
            format!("LOD/0/{sid}_0_windows"),
            format!("LOD/1/{sid}_1_windows"),
        ])
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn self_gate_passes_on_synthetic_lod_bundle_and_catches_mismatches() {
    std::env::set_var(
        "ABGEN_ROOT",
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap(),
    );
    let dir = std::env::temp_dir().join(format!("abgen-lod-selfgate-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sid = "bafkreiselfgatesynthetic";
    let src = dir.join(format!("{sid}_1.glb"));
    std::fs::write(&src, synthetic_glb()).unwrap();

    let client = CatalystClient::new("http://127.0.0.1:9");
    let opts = lods::LodOptions {
        platform: "windows".to_string(),
        lod: Some(lods::LodGenMeta {
            parcels: vec![(8, -83)],
            base: (8, -83),
            timestamp: None,
            vertical_override: None,
        }),
        ..Default::default()
    };
    let out = dir.join("out");
    let conv = lods::convert_lods(
        &client,
        &[src.to_string_lossy().into_owned()],
        out.to_str().unwrap(),
        &opts,
    )
    .unwrap();
    assert_eq!(conv.results.len(), 1);
    assert_eq!(
        conv.results[0].rel_path,
        expected_rel_path(sid, 1, "windows")
    );
    let bundle_path = out.join(sid).join(&conv.results[0].rel_path);
    let data = std::fs::read(&bundle_path).unwrap();

    let checks = self_gate_bundle(&data, sid, 1, "windows").unwrap();
    assert!(checks.len() >= 9, "{}", checks.len());
    for c in &checks {
        assert!(c.ok, "unexpected FAIL {}: {}", c.label, c.detail);
    }
    assert_eq!(gate_failures(&checks), 0);

    let wrong_id = self_gate_bundle(&data, "bafkreiwrongid", 1, "windows").unwrap();
    let failed: Vec<&str> = wrong_id
        .iter()
        .filter(|c| !c.ok)
        .map(|c| c.label.as_str())
        .collect();
    assert!(failed.contains(&"root-name"), "{failed:?}");
    assert!(failed.contains(&"metadata-main-asset"), "{failed:?}");

    let wrong_platform = self_gate_bundle(&data, sid, 1, "mac").unwrap();
    let failed: Vec<&str> = wrong_platform
        .iter()
        .filter(|c| !c.ok)
        .map(|c| c.label.as_str())
        .collect();
    assert!(failed.contains(&"assetbundle-dep"), "{failed:?}");
    assert!(failed.contains(&"metadata-deps"), "{failed:?}");

    let wrong_level = self_gate_bundle(&data, sid, 0, "windows").unwrap();
    assert!(gate_failures(&wrong_level) > 0);

    let on_budget = self_gate_bundle_with(&data, sid, 1, "windows", true, Some(4)).unwrap();
    assert_eq!(gate_failures(&on_budget), 0);
    assert!(on_budget
        .iter()
        .any(|c| c.label == "texture-uniform-size" && c.ok));

    let off_budget = self_gate_bundle_with(&data, sid, 1, "windows", true, Some(256)).unwrap();
    let failed: Vec<&str> = off_budget
        .iter()
        .filter(|c| !c.ok)
        .map(|c| c.label.as_str())
        .collect();
    assert!(!failed.is_empty());
    assert!(
        failed.iter().all(|l| l.starts_with("texture[")),
        "{failed:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn orphan_material_is_dropped_from_lod_bundle() {
    std::env::set_var(
        "ABGEN_ROOT",
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap(),
    );
    let dir = std::env::temp_dir().join(format!("abgen-lod-orphanmat-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sid = "bafkreiorphanmat";
    let src = dir.join(format!("{sid}_1.glb"));
    let glb = emit_glb(&LodModel {
        root_name: "orphan".to_string(),
        primitives: vec![LodPrimitive {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: vec![[0.0, 0.0, 1.0]; 3],
            uvs: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
            indices: vec![0, 1, 2],
            material: 0,
            ..Default::default()
        }],
        materials: vec![
            LodMaterial {
                name: "TextureBakeResult-mat".to_string(),
                class: AlphaClass::Opaque,
                base_color: [1.0, 1.0, 1.0, 1.0],
                cutoff: 0.5,
                image: Some(0),
                double_sided: false,
            },
            LodMaterial {
                name: "TextureBakeResult-mat-transparent".to_string(),
                class: AlphaClass::Blend,
                base_color: [1.0, 1.0, 1.0, 1.0],
                cutoff: 0.5,
                image: Some(0),
                double_sided: false,
            },
        ],
        images: vec![LodImage {
            bytes: tiny_png(),
            mime: "image/png".to_string(),
        }],
        log: Vec::new(),
    })
    .unwrap();
    std::fs::write(&src, glb).unwrap();

    let client = CatalystClient::new("http://127.0.0.1:9");
    let opts = lods::LodOptions {
        platform: "windows".to_string(),
        lod: Some(lods::LodGenMeta {
            parcels: vec![(1, 2)],
            base: (1, 2),
            timestamp: None,
            vertical_override: None,
        }),
        ..Default::default()
    };
    let out = dir.join("out");
    let conv = lods::convert_lods(
        &client,
        &[src.to_string_lossy().into_owned()],
        out.to_str().unwrap(),
        &opts,
    )
    .unwrap();
    let data = std::fs::read(out.join(sid).join(&conv.results[0].rel_path)).unwrap();
    let checks = self_gate_bundle(&data, sid, 1, "windows").unwrap();
    for c in &checks {
        assert!(c.ok, "unexpected FAIL {}: {}", c.label, c.detail);
    }
    let shader_checks = checks
        .iter()
        .filter(|c| c.label.starts_with("shader-pptr["))
        .count();
    assert_eq!(shader_checks, 1, "orphan material shipped");
    assert!(
        checks
            .iter()
            .any(|c| c.label == "shader-pptr[TextureBakeResult-mat]"),
        "referenced material missing"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn metadata_script(data: &[u8]) -> String {
    let bundle = Bundle::load_bytes(data).unwrap();
    for file in &bundle.files {
        let FileContent::Serialized(sf) = &file.content else {
            continue;
        };
        for obj in &sf.objects {
            if obj.class_id != 49 {
                continue;
            }
            let v = sf.read_typetree(obj).unwrap();
            if v.get("m_Name").and_then(|x| x.as_str()) == Some("metadata") {
                let s = v.get("m_Script").unwrap();
                return s.as_str().map(String::from).unwrap_or_else(|| {
                    String::from_utf8_lossy(s.as_bytes().unwrap()).into_owned()
                });
            }
        }
    }
    panic!("no metadata TextAsset in bundle");
}

#[test]
fn metadata_ticks_stable_across_rebuilds_and_schema_modulo_ticks() {
    std::env::set_var(
        "ABGEN_ROOT",
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap(),
    );
    let dir = std::env::temp_dir().join(format!("abgen-lod-ticks-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sid = "bafkreitickssynthetic";
    let src = dir.join(format!("{sid}_1.glb"));
    std::fs::write(&src, synthetic_glb()).unwrap();

    let client = CatalystClient::new("http://127.0.0.1:9");
    let ticks = lods::entity_ticks(1_694_177_669_000);
    assert_eq!(ticks, 638_297_744_690_000_000);
    let opts = lods::LodOptions {
        platform: "windows".to_string(),
        lod: Some(lods::LodGenMeta {
            parcels: vec![(8, -83)],
            base: (8, -83),
            timestamp: Some(ticks),
            vertical_override: None,
        }),
        ..Default::default()
    };
    let mut bundles: Vec<Vec<u8>> = Vec::new();
    for run in ["a", "b"] {
        let out = dir.join(run);
        let conv = lods::convert_lods(
            &client,
            &[src.to_string_lossy().into_owned()],
            out.to_str().unwrap(),
            &opts,
        )
        .unwrap();
        bundles.push(std::fs::read(out.join(sid).join(&conv.results[0].rel_path)).unwrap());
    }
    assert_eq!(bundles[0], bundles[1], "rebuild not byte-stable");

    let want = format!(
        "{{\"timestamp\":{ticks},\"version\":\"1.0\",\"dependencies\":[{}],\"mainAsset\":\"{sid}_1.prefab\"}}",
        serde_json::to_string(&crate::shader::texarray_bundle_name("windows")).unwrap()
    );
    assert_eq!(metadata_script(&bundles[0]), want);

    let _ = std::fs::remove_dir_all(&dir);
}

fn build_lod_bundle_from(glb: Vec<u8>, sid: &str, tag: &str) -> Vec<u8> {
    std::env::set_var(
        "ABGEN_ROOT",
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap(),
    );
    let dir = std::env::temp_dir().join(format!("abgen-lod-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join(format!("{sid}_1.glb"));
    std::fs::write(&src, glb).unwrap();
    let client = CatalystClient::new("http://127.0.0.1:9");
    let opts = lods::LodOptions {
        platform: "windows".to_string(),
        lod: Some(lods::LodGenMeta {
            parcels: vec![(0, 0)],
            base: (0, 0),
            timestamp: None,
            vertical_override: None,
        }),
        ..Default::default()
    };
    let out = dir.join("out");
    let conv = lods::convert_lods(
        &client,
        &[src.to_string_lossy().into_owned()],
        out.to_str().unwrap(),
        &opts,
    )
    .unwrap();
    let data = std::fs::read(out.join(sid).join(&conv.results[0].rel_path)).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    data
}

fn mesh_encoding_facts(data: &[u8]) -> Vec<(Vec<(i64, i64, i64, i64)>, i64, i64, Vec<u8>)> {
    let bundle = Bundle::load_bytes(data).unwrap();
    let mut ress: std::collections::HashMap<String, Vec<u8>> = std::collections::HashMap::new();
    for file in &bundle.files {
        if let FileContent::Raw(raw) = &file.content {
            ress.insert(file.name.clone(), raw.clone());
        }
    }
    let mut out = Vec::new();
    for file in &bundle.files {
        let FileContent::Serialized(sf) = &file.content else {
            continue;
        };
        for obj in &sf.objects {
            if obj.class_id != 43 {
                continue;
            }
            let v = sf.read_typetree(obj).unwrap();
            let vd = v.get("m_VertexData").unwrap();
            let channels = vd
                .get("m_Channels")
                .and_then(|c| c.as_array())
                .unwrap()
                .iter()
                .map(|c| {
                    let g = |k: &str| c.get(k).and_then(|x| x.as_i64()).unwrap();
                    (g("stream"), g("offset"), g("format"), g("dimension"))
                })
                .collect();
            let idxfmt = v.get("m_IndexFormat").and_then(|x| x.as_i64()).unwrap();
            let vcount = vd.get("m_VertexCount").and_then(|x| x.as_i64()).unwrap();
            let inline = vd.get("m_DataSize").and_then(|x| x.as_bytes()).unwrap();
            let bytes = if inline.is_empty() {
                let sd = v.get("m_StreamData").unwrap();
                let g = |k: &str| sd.get(k).and_then(|x| x.as_i64()).unwrap() as usize;
                let path = sd.get("path").and_then(|x| x.as_str()).unwrap();
                let node = path.rsplit('/').next().unwrap();
                ress[node][g("offset")..g("offset") + g("size")].to_vec()
            } else {
                inline.to_vec()
            };
            out.push((channels, idxfmt, vcount, bytes));
        }
    }
    out
}

#[test]
fn lod_vertex_declaration_matches_prod_and_payload_is_interleaved() {
    let sid = "bafkreimeshencoding";
    let data = build_lod_bundle_from(synthetic_glb(), sid, "meshenc");
    let facts = mesh_encoding_facts(&data);
    assert_eq!(facts.len(), 1);
    let (channels, idxfmt, vcount, bytes) = &facts[0];
    assert_eq!(*channels, crate::mesh_layout::lod_channel_table());
    assert_eq!(*idxfmt, 0, "u16 indices when max index < 65536");
    assert_eq!(
        bytes.len() as i64,
        vcount * crate::mesh_layout::LOD_INTERLEAVED_STRIDE as i64
    );
    assert!(
        facts[0]
            .3
            .chunks(32)
            .any(|row| row[20..28].iter().any(|&b| b != 0)),
        "tangent bytes all zero"
    );
    let checks = self_gate_bundle(&data, sid, 1, "windows").unwrap();
    assert!(
        checks.iter().any(|c| c.label.starts_with("mesh-encoding[")),
        "mesh-encoding gate missing"
    );
    for c in &checks {
        assert!(c.ok, "unexpected FAIL {}: {}", c.label, c.detail);
    }
}

#[test]
fn lod_bundle_splits_vertex_payload_into_cab_ress() {
    let sid = "bafkreiresssplit";
    let data = build_lod_bundle_from(synthetic_glb(), sid, "resssplit");
    let bundle = Bundle::load_bytes(&data).unwrap();
    let names: Vec<&str> = bundle.files.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names.len(), 2, "container census: {names:?}");
    assert_eq!(names[1], format!("{}.resS", names[0]), "census: {names:?}");
    let cab = names[0].to_string();
    let ress = &bundle.files[1];
    assert_eq!(ress.flags, crate::ress::RESS_NODE_FLAGS);
    let FileContent::Raw(ress_bytes) = &ress.content else {
        panic!(".resS node must be raw");
    };
    let FileContent::Serialized(sf) = &bundle.files[0].content else {
        panic!("CAB node must be serialized");
    };
    let mut meshes = 0;
    for obj in &sf.objects {
        if obj.class_id != 43 {
            continue;
        }
        meshes += 1;
        let v = sf.read_typetree(obj).unwrap();
        let vd = v.get("m_VertexData").unwrap();
        let inline = vd.get("m_DataSize").and_then(|x| x.as_bytes()).unwrap();
        assert!(inline.is_empty(), "vertexData must be inline=0");
        let vcount = vd.get("m_VertexCount").and_then(|x| x.as_i64()).unwrap();
        let sd = v.get("m_StreamData").unwrap();
        let g = |k: &str| sd.get(k).and_then(|x| x.as_i64()).unwrap();
        assert_eq!(
            sd.get("path").and_then(|x| x.as_str()).unwrap(),
            format!("archive:/{cab}/{cab}.resS"),
            "archive streamData ref"
        );
        assert_eq!(
            g("size"),
            vcount * crate::mesh_layout::LOD_INTERLEAVED_STRIDE as i64
        );
        let end = (g("offset") + g("size")) as usize;
        assert!(end <= ress_bytes.len(), "stream span exceeds .resS node");
    }
    assert!(meshes > 0, "fixture bundle has no meshes");
    assert!(
        ress_bytes.len().is_multiple_of(16),
        ".resS length {} not 16-aligned",
        ress_bytes.len()
    );
    let mut texs = 0;
    for obj in &sf.objects {
        if obj.class_id != 28 {
            continue;
        }
        texs += 1;
        let v = sf.read_typetree(obj).unwrap();
        let img = v.get("image data").and_then(|x| x.as_bytes()).unwrap();
        assert!(!img.is_empty(), "texture streamed out of CAB");
        let size = v
            .get("m_StreamData")
            .and_then(|s| s.get("size"))
            .and_then(|x| x.as_i64())
            .unwrap();
        assert_eq!(size, 0);
    }
    assert!(texs > 0, "fixture bundle has no textures");
    let checks = self_gate_bundle(&data, sid, 1, "windows").unwrap();
    assert!(
        checks.iter().any(|c| c.label == "archive-nodes" && c.ok),
        "archive-nodes gate missing"
    );
    for c in &checks {
        assert!(c.ok, "unexpected FAIL {}: {}", c.label, c.detail);
    }
}

#[test]
fn lod_index_width_follows_max_index_boundary() {
    let n = 65537usize;
    let glb = emit_glb(&LodModel {
        root_name: "wide".to_string(),
        primitives: vec![LodPrimitive {
            positions: (0..n)
                .map(|i| [(i % 256) as f32 * 0.01, (i / 256) as f32 * 0.01, 0.0])
                .collect(),
            normals: vec![[0.0, 0.0, 1.0]; n],
            uvs: vec![[0.0, 0.0]; n],
            indices: vec![0, 1, n as u32 - 1],
            material: 0,
            ..Default::default()
        }],
        materials: vec![LodMaterial {
            name: "TextureBakeResult-m".to_string(),
            class: AlphaClass::Opaque,
            base_color: [1.0, 1.0, 1.0, 1.0],
            cutoff: 0.5,
            image: None,
            double_sided: false,
        }],
        images: Vec::new(),
        log: Vec::new(),
    })
    .unwrap();
    let data = build_lod_bundle_from(glb, "bafkreiwideindex", "wideidx");
    let facts = mesh_encoding_facts(&data);
    assert_eq!(facts.len(), 1);
    let (_, idxfmt, vcount, _) = &facts[0];
    assert_eq!(*vcount, 65537);
    assert_eq!(*idxfmt, 1, "u32 indices once max index >= 65536");
}

fn rollup_fixture_glb() -> Vec<u8> {
    let glb = synthetic_glb();
    let jlen = u32::from_le_bytes(glb[12..16].try_into().unwrap()) as usize;
    let mut json: serde_json::Value = serde_json::from_slice(&glb[20..20 + jlen]).unwrap();
    let bstart = 20 + jlen;
    let blen = u32::from_le_bytes(glb[bstart..bstart + 4].try_into().unwrap()) as usize;
    let bin = glb[bstart + 8..bstart + 8 + blen].to_vec();
    json["nodes"] = serde_json::json!([
        {"children": [1, 2], "name": "prop"},
        {"mesh": 0, "name": "box"},
        {"mesh": 0, "name": "box_collider"}
    ]);
    json["scenes"] = serde_json::json!([{"nodes": [0]}]);
    let mut jb = serde_json::to_vec(&json).unwrap();
    while !jb.len().is_multiple_of(4) {
        jb.push(b' ');
    }
    let mut bb = bin;
    while !bb.len().is_multiple_of(4) {
        bb.push(0);
    }
    let total = 12 + 8 + jb.len() + 8 + bb.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(jb.len() as u32).to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(&jb);
    out.extend_from_slice(&(bb.len() as u32).to_le_bytes());
    out.extend_from_slice(&[0x42, 0x49, 0x4E, 0x00]);
    out.extend_from_slice(&bb);
    out
}

#[test]
fn lod0_rollup_single_root_collider_parity_and_original_textures() {
    std::env::set_var(
        "ABGEN_ROOT",
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap(),
    );
    let dir = std::env::temp_dir().join(format!("abgen-lod0-rollup-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let cache = dir.join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("hbox"), rollup_fixture_glb()).unwrap();
    let sid = "bafkreirollupzero";
    let ent = crate::catalyst::Scene {
        entity_id: sid.to_string(),
        entity_type: "scene".to_string(),
        pointers: Vec::new(),
        content: vec![crate::catalyst::ContentEntry {
            file: "box.glb".to_string(),
            hash: "hbox".to_string(),
        }],
        metadata: serde_json::json!({}),
        timestamp: None,
    };
    let client = CatalystClient::new("http://127.0.0.1:9");
    let mk = |pos: [f64; 3]| placements::Placement {
        glb_hash: Some("hbox".to_string()),
        position: pos,
        ..Default::default()
    };
    let out = rollup::rollup(
        &client,
        &ent,
        &[mk([3.0, 4.0, 5.0]), mk([-8.0, 0.0, 2.0])],
        0,
        Some(&cache),
    )
    .unwrap();
    let src = dir.join(format!("{sid}_0.glb"));
    std::fs::write(&src, &out.glb).unwrap();

    let opts = lods::LodOptions {
        platform: "windows".to_string(),
        lod: Some(lods::LodGenMeta {
            parcels: vec![(1, 2)],
            base: (1, 2),
            timestamp: Some(1_694_177_669_000),
            vertical_override: None,
        }),
        ..Default::default()
    };
    let outdir = dir.join("out");
    let conv = lods::convert_lods(
        &client,
        &[src.to_string_lossy().into_owned()],
        outdir.to_str().unwrap(),
        &opts,
    )
    .unwrap();
    let data = std::fs::read(outdir.join(sid).join(&conv.results[0].rel_path)).unwrap();

    let bundle = Bundle::load_bytes(&data).unwrap();
    let mut go_names: HashMap<i64, String> = HashMap::new();
    let mut roots: Vec<(i64, [f64; 3])> = Vec::new();
    let mut positions: Vec<[f64; 3]> = Vec::new();
    let mut filters: HashMap<i64, i64> = HashMap::new();
    let mut colliders: Vec<(i64, i64)> = Vec::new();
    let mut renderer_gos: Vec<i64> = Vec::new();
    let mut materials: Vec<(String, i64, Vec<(String, [f64; 4])>)> = Vec::new();
    let mut textures: Vec<(String, i64, i64, i64)> = Vec::new();
    let mut mesh_count = 0;
    for file in &bundle.files {
        let FileContent::Serialized(sf) = &file.content else {
            continue;
        };
        for obj in &sf.objects {
            let v = sf.read_typetree(obj).unwrap();
            let go = |v: &crate::value::Value| {
                v.get("m_GameObject")
                    .and_then(|p| p.get("m_PathID"))
                    .and_then(|x| x.as_i64())
                    .unwrap_or(0)
            };
            match obj.class_id {
                1 => {
                    go_names.insert(
                        obj.path_id,
                        v.get("m_Name")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .into(),
                    );
                }
                4 => {
                    let pos = v.get("m_LocalPosition");
                    let axis =
                        |k: &str| pos.and_then(|p| p.get(k)).and_then(|x| x.as_f64()).unwrap();
                    let p = [axis("x"), axis("y"), axis("z")];
                    positions.push(p);
                    let father = v
                        .get("m_Father")
                        .and_then(|f| f.get("m_PathID"))
                        .and_then(|x| x.as_i64())
                        .unwrap_or(0);
                    if father == 0 {
                        roots.push((go(&v), p));
                    }
                }
                21 => {
                    let name = v
                        .get("m_Name")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let pid = v
                        .get("m_Shader")
                        .and_then(|p| p.get("m_PathID"))
                        .and_then(|x| x.as_i64())
                        .unwrap_or(0);
                    let mut colors = Vec::new();
                    if let Some(arr) = v
                        .get("m_SavedProperties")
                        .and_then(|sp| sp.get("m_Colors"))
                        .and_then(|c| c.as_array())
                    {
                        for pair in arr {
                            let Some(items) = pair.as_array() else {
                                continue;
                            };
                            let key = items[0].as_str().unwrap_or("").to_string();
                            let col = items.get(1);
                            let f = |k: &str| {
                                col.and_then(|c| c.get(k))
                                    .and_then(|x| x.as_f64())
                                    .unwrap_or(f64::NAN)
                            };
                            colors.push((key, [f("r"), f("g"), f("b"), f("a")]));
                        }
                    }
                    materials.push((name, pid, colors));
                }
                23 => renderer_gos.push(go(&v)),
                33 => {
                    let mesh = v
                        .get("m_Mesh")
                        .and_then(|p| p.get("m_PathID"))
                        .and_then(|x| x.as_i64())
                        .unwrap_or(0);
                    filters.insert(go(&v), mesh);
                }
                43 => mesh_count += 1,
                64 => {
                    let mesh = v
                        .get("m_Mesh")
                        .and_then(|p| p.get("m_PathID"))
                        .and_then(|x| x.as_i64())
                        .unwrap_or(0);
                    colliders.push((go(&v), mesh));
                }
                28 => {
                    let g = |k: &str| v.get(k).and_then(|x| x.as_i64()).unwrap_or(-1);
                    textures.push((
                        v.get("m_Name")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .into(),
                        g("m_TextureFormat"),
                        g("m_Width"),
                        g("m_Height"),
                    ));
                }
                _ => {}
            }
        }
    }

    assert_eq!(roots.len(), 1, "exactly one root GameObject");
    assert_eq!(go_names[&roots[0].0], format!("{sid}_0"));
    assert_eq!(roots[0].1, [0.0, 0.0, 0.0], "root at origin");
    for want in [[3.0, 4.0, 5.0], [-8.0, 0.0, 2.0]] {
        assert!(
            positions
                .iter()
                .any(|p| p.iter().zip(want).all(|(a, b)| (a - b).abs() < 1e-5)),
            "no transform at {want:?}: {positions:?}"
        );
    }
    assert_eq!(colliders.len(), 2, "one MeshCollider per instance");
    for (go, mesh) in &colliders {
        assert_eq!(go_names[go], "box_collider");
        assert_eq!(filters[go], *mesh, "collider mesh == filter mesh");
        assert!(*mesh != 0);
        assert!(!renderer_gos.contains(go), "collider GO has a renderer");
    }
    assert_eq!(renderer_gos.len(), 2, "one MeshRenderer per instance");
    assert_eq!(mesh_count, 1, "instances share the mesh");
    assert_eq!(materials.len(), 1);
    let (_, shader_pid, colors) = &materials[0];
    assert_eq!(*shader_pid, crate::shader::SHADER_PATH_ID);
    let color = |k: &str| colors.iter().find(|(n, _)| n == k).unwrap().1;
    assert_eq!(
        color("_VerticalClipping"),
        [-2147483648.0, 2147483648.0, 0.0, 0.0]
    );
    for (got, want) in color("_PlaneClipping")
        .iter()
        .zip([15.95, 32.05, 31.95, 48.05])
    {
        assert!((got - want).abs() < 1e-4, "plane clipping {got} vs {want}");
    }
    assert_eq!(textures.len(), 1);
    let (_, fmt, w, h) = &textures[0];
    assert_eq!((*w, *h), (4, 4), "original texture dims, no atlas canvas");
    assert!(*fmt == 10 || *fmt == 12, "DXT pair, got {fmt}");

    let checks = self_gate_bundle_with(&data, sid, 0, "windows", true, None).unwrap();
    assert!(
        checks
            .iter()
            .any(|c| c.label == "collider-mesh-refs" && c.ok),
        "collider-mesh-refs gate missing"
    );
    for c in &checks {
        assert!(c.ok, "unexpected FAIL {}: {}", c.label, c.detail);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

fn skinned_fixture_glb() -> Vec<u8> {
    // skinned node + static node sharing mesh 0, two joint nodes
    let glb = synthetic_glb();
    let jlen = u32::from_le_bytes(glb[12..16].try_into().unwrap()) as usize;
    let mut json: serde_json::Value = serde_json::from_slice(&glb[20..20 + jlen]).unwrap();
    let bstart = 20 + jlen;
    let blen = u32::from_le_bytes(glb[bstart..bstart + 4].try_into().unwrap()) as usize;
    let mut bin = glb[bstart + 8..bstart + 8 + blen].to_vec();
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
    let w_off = bin.len();
    for w in [[0.7f32, 0.3, 0.0, 0.0]; 3] {
        for c in w {
            bin.extend_from_slice(&c.to_le_bytes());
        }
    }
    let j_off = bin.len();
    bin.extend_from_slice(&[1u8, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0]);
    let views = json["bufferViews"].as_array().unwrap().len();
    json["bufferViews"].as_array_mut().unwrap().extend([
        serde_json::json!({"buffer": 0, "byteOffset": w_off, "byteLength": 48}),
        serde_json::json!({"buffer": 0, "byteOffset": j_off, "byteLength": 12}),
    ]);
    let accs = json["accessors"].as_array().unwrap().len();
    json["accessors"].as_array_mut().unwrap().extend([
        serde_json::json!({"bufferView": views, "componentType": 5126, "count": 3, "type": "VEC4"}),
        serde_json::json!({"bufferView": views + 1, "componentType": 5121, "count": 3, "type": "VEC4"}),
    ]);
    json["buffers"][0]["byteLength"] = serde_json::json!(bin.len());
    json["meshes"][0]["primitives"][0]["attributes"]["WEIGHTS_0"] = serde_json::json!(accs);
    json["meshes"][0]["primitives"][0]["attributes"]["JOINTS_0"] = serde_json::json!(accs + 1);
    json["skins"] = serde_json::json!([{"joints": [3, 4]}]);
    json["nodes"] = serde_json::json!([
        {"children": [1, 2, 3, 4], "name": "prop"},
        {"mesh": 0, "skin": 0, "name": "skinbox"},
        {"mesh": 0, "name": "staticbox"},
        {"name": "jointA"},
        {"name": "jointB"}
    ]);
    json["scenes"] = serde_json::json!([{"nodes": [0]}]);
    let mut jb = serde_json::to_vec(&json).unwrap();
    while !jb.len().is_multiple_of(4) {
        jb.push(b' ');
    }
    let total = 12 + 8 + jb.len() + 8 + bin.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(jb.len() as u32).to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(&jb);
    out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    out.extend_from_slice(&[0x42, 0x49, 0x4E, 0x00]);
    out.extend_from_slice(&bin);
    out
}

#[test]
fn lod0_skinned_mesh_keeps_blend_channels_and_stays_inline() {
    std::env::set_var(
        "ABGEN_ROOT",
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap(),
    );
    let dir = std::env::temp_dir().join(format!("abgen-lod0-skinned-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let cache = dir.join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("hskin"), skinned_fixture_glb()).unwrap();
    let sid = "bafkreiskinzero";
    let ent = crate::catalyst::Scene {
        entity_id: sid.to_string(),
        entity_type: "scene".to_string(),
        pointers: Vec::new(),
        content: vec![crate::catalyst::ContentEntry {
            file: "skin.glb".to_string(),
            hash: "hskin".to_string(),
        }],
        metadata: serde_json::json!({}),
        timestamp: None,
    };
    let client = CatalystClient::new("http://127.0.0.1:9");
    let out = rollup::rollup(
        &client,
        &ent,
        &[placements::Placement {
            glb_hash: Some("hskin".to_string()),
            position: [3.0, 0.0, 5.0],
            ..Default::default()
        }],
        0,
        Some(&cache),
    )
    .unwrap();
    let src = dir.join(format!("{sid}_0.glb"));
    std::fs::write(&src, &out.glb).unwrap();

    let opts = lods::LodOptions {
        platform: "windows".to_string(),
        lod: Some(lods::LodGenMeta {
            parcels: vec![(1, 2)],
            base: (1, 2),
            timestamp: Some(1_694_177_669_000),
            vertical_override: None,
        }),
        ..Default::default()
    };
    let outdir = dir.join("out");
    let conv = lods::convert_lods(
        &client,
        &[src.to_string_lossy().into_owned()],
        outdir.to_str().unwrap(),
        &opts,
    )
    .unwrap();
    let data = std::fs::read(outdir.join(sid).join(&conv.results[0].rel_path)).unwrap();

    let bundle = Bundle::load_bytes(&data).unwrap();
    let mut smr_mesh_pids: Vec<i64> = Vec::new();
    struct MeshFacts {
        bindposes: usize,
        channels: Vec<(usize, i64, i64, i64, i64)>,
        inline: usize,
        stream: i64,
    }
    let mut meshes: HashMap<i64, MeshFacts> = HashMap::new();
    for file in &bundle.files {
        let FileContent::Serialized(sf) = &file.content else {
            continue;
        };
        for obj in &sf.objects {
            let v = sf.read_typetree(obj).unwrap();
            if obj.class_id == 137 {
                smr_mesh_pids.push(
                    v.get("m_Mesh")
                        .and_then(|p| p.get("m_PathID"))
                        .and_then(|x| x.as_i64())
                        .unwrap(),
                );
            } else if obj.class_id == 43 {
                let vd = v.get("m_VertexData").unwrap();
                let channels = vd
                    .get("m_Channels")
                    .and_then(|c| c.as_array())
                    .unwrap()
                    .iter()
                    .enumerate()
                    .filter_map(|(i, c)| {
                        let f = |k: &str| c.get(k).and_then(|x| x.as_i64()).unwrap();
                        (f("dimension") != 0)
                            .then(|| (i, f("stream"), f("offset"), f("format"), f("dimension")))
                    })
                    .collect();
                meshes.insert(
                    obj.path_id,
                    MeshFacts {
                        bindposes: v
                            .get("m_BindPose")
                            .and_then(|b| b.as_array())
                            .map_or(0, |a| a.len()),
                        channels,
                        inline: vd
                            .get("m_DataSize")
                            .and_then(|x| x.as_bytes())
                            .map_or(0, |b| b.len()),
                        stream: v
                            .get("m_StreamData")
                            .and_then(|s| s.get("size"))
                            .and_then(|x| x.as_i64())
                            .unwrap_or(0),
                    },
                );
            }
        }
    }
    assert_eq!(smr_mesh_pids.len(), 1, "one SkinnedMeshRenderer");
    let skinned = &meshes[&smr_mesh_pids[0]];
    assert_eq!(skinned.bindposes, 2);
    assert!(
        skinned.channels.contains(&(12, 2, 0, 0, 4))
            && skinned.channels.contains(&(13, 2, 16, 10, 4)),
        "blend channels: {:?}",
        skinned.channels
    );
    assert!(
        skinned.inline > 0 && skinned.stream == 0,
        "skinned stays inline"
    );
    let statics: Vec<&MeshFacts> = meshes.values().filter(|m| m.bindposes == 0).collect();
    assert!(!statics.is_empty(), "static mesh present");
    for m in statics {
        assert!(
            m.inline == 0 && m.stream > 0,
            "static meshes stream to .resS"
        );
    }

    let checks = self_gate_bundle_with(&data, sid, 0, "windows", true, None).unwrap();
    for c in &checks {
        assert!(c.ok, "unexpected FAIL {}: {}", c.label, c.detail);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

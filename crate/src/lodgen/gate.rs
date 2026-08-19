use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};

use crate::lods;
use crate::unity::bundle_file::{Bundle, FileContent};

#[derive(Clone, Debug)]
pub struct GateCheck {
    pub label: String,
    pub ok: bool,
    pub detail: String,
}

pub fn gate_failures(checks: &[GateCheck]) -> usize {
    checks.iter().filter(|c| !c.ok).count()
}

pub(super) fn push_check(
    checks: &mut Vec<GateCheck>,
    label: impl Into<String>,
    ok: bool,
    detail: String,
) {
    checks.push(GateCheck {
        label: label.into(),
        ok,
        detail,
    });
}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(super) fn tri_cap_check(cap: u64, tris_after: usize, unsimplified: bool) -> GateCheck {
    let detail = if unsimplified {
        format!("{tris_after} tris vs cap {cap}: WAIVED (--allow-unsimplified verbatim copy)")
    } else {
        format!("{tris_after} tris <= cap {cap}")
    };
    GateCheck {
        label: "tri-cap".to_string(),
        ok: unsimplified || tris_after as u64 <= cap,
        detail,
    }
}

fn lod_target_platform(platform: &str) -> Option<i32> {
    match platform {
        "windows" => Some(19),
        "mac" => Some(2),
        "linux" => Some(24),
        _ => None,
    }
}

pub fn self_gate_bundle(
    data: &[u8],
    scene_id: &str,
    level: u32,
    platform: &str,
) -> Result<Vec<GateCheck>> {
    self_gate_bundle_with(data, scene_id, level, platform, true, None)
}

pub fn self_gate_bundle_with(
    data: &[u8],
    scene_id: &str,
    level: u32,
    platform: &str,
    expect_content: bool,
    atlas_budget: Option<u32>,
) -> Result<Vec<GateCheck>> {
    let bundle = Bundle::load_bytes(data).context("parse built bundle")?;
    let sid = scene_id.to_lowercase();
    let mut go_names: HashMap<i64, String> = HashMap::new();
    let mut root_gos: Vec<i64> = Vec::new();
    let mut root_positions: Vec<[f64; 3]> = Vec::new();
    let mut materials: Vec<(String, i64, i64, i64)> = Vec::new();
    let mut renderer_mat_pids: HashSet<i64> = HashSet::new();
    let mut textures: Vec<(String, i64, i64, i64, i64)> = Vec::new();
    let mut mat_metal: Vec<(String, i64, Vec<String>, f64, f64, f64)> = Vec::new();
    let mut tex_colorspace: HashMap<i64, i64> = HashMap::new();
    let mut mesh_extent = [0.0f64; 3];
    let mut mesh_count = 0usize;
    let mut deps: Vec<String> = Vec::new();
    let mut metadata: Option<serde_json::Value> = None;
    let mut target_platform: Option<i32> = None;
    for file in &bundle.files {
        let FileContent::Serialized(sf) = &file.content else {
            continue;
        };
        if target_platform.is_none() {
            target_platform = Some(sf.target_platform);
        }
        for obj in &sf.objects {
            let v = sf
                .read_typetree(obj)
                .with_context(|| format!("typetree pid {}", obj.path_id))?;
            let name = v
                .get("m_Name")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            match obj.class_id {
                1 => {
                    go_names.insert(obj.path_id, name);
                }
                4 => {
                    let father = v
                        .get("m_Father")
                        .and_then(|p| p.get("m_PathID"))
                        .and_then(|x| x.as_i64())
                        .unwrap_or(0);
                    if father == 0 {
                        let go = v
                            .get("m_GameObject")
                            .and_then(|p| p.get("m_PathID"))
                            .and_then(|x| x.as_i64())
                            .unwrap_or(0);
                        root_gos.push(go);
                        let pos = v.get("m_LocalPosition");
                        let axis = |k: &str| {
                            pos.and_then(|p| p.get(k))
                                .and_then(|x| x.as_f64())
                                .unwrap_or(f64::NAN)
                        };
                        root_positions.push([axis("x"), axis("y"), axis("z")]);
                    }
                }
                21 => {
                    let fid = v
                        .get("m_Shader")
                        .and_then(|p| p.get("m_FileID"))
                        .and_then(|x| x.as_i64())
                        .unwrap_or(-1);
                    let pid = v
                        .get("m_Shader")
                        .and_then(|p| p.get("m_PathID"))
                        .and_then(|x| x.as_i64())
                        .unwrap_or(-1);
                    let keywords: Vec<String> = v
                        .get("m_ValidKeywords")
                        .and_then(|k| k.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|s| s.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                    let mgm = v
                        .get("m_SavedProperties")
                        .and_then(|p| p.get("m_TexEnvs"))
                        .and_then(|t| t.as_array())
                        .and_then(|arr| {
                            arr.iter().find_map(|e| {
                                let pair = e.as_array()?;
                                if pair.first()?.as_str()? == "_MetallicGlossMap" {
                                    pair.get(1)?.get("m_Texture")?.get("m_PathID")?.as_i64()
                                } else {
                                    None
                                }
                            })
                        })
                        .unwrap_or(0);
                    let flt = |n: &str| {
                        v.get("m_SavedProperties")
                            .and_then(|p| p.get("m_Floats"))
                            .and_then(|f| f.as_array())
                            .and_then(|arr| {
                                arr.iter().find_map(|e| {
                                    let pair = e.as_array()?;
                                    if pair.first()?.as_str()? == n {
                                        pair.get(1)?.as_f64()
                                    } else {
                                        None
                                    }
                                })
                            })
                            .unwrap_or(f64::NAN)
                    };
                    mat_metal.push((
                        name.clone(),
                        mgm,
                        keywords,
                        flt("_Metallic"),
                        flt("_Smoothness"),
                        flt("_SmoothnessTextureChannel"),
                    ));
                    materials.push((name, fid, pid, obj.path_id));
                }
                23 | 137 => {
                    if let Some(mats) = v.get("m_Materials").and_then(|m| m.as_array()) {
                        for m in mats {
                            if let Some(p) = m.get("m_PathID").and_then(|x| x.as_i64()) {
                                renderer_mat_pids.insert(p);
                            }
                        }
                    }
                }
                43 => {
                    mesh_count += 1;
                    if let Some(ext) = v.get("m_LocalAABB").and_then(|b| b.get("m_Extent")) {
                        for (i, axis) in ["x", "y", "z"].iter().enumerate() {
                            let e = ext.get(axis).and_then(|x| x.as_f64()).unwrap_or(0.0);
                            mesh_extent[i] = mesh_extent[i].max(e.abs());
                        }
                    }
                }
                28 => {
                    let get = |k: &str| v.get(k).and_then(|x| x.as_i64()).unwrap_or(-1);
                    tex_colorspace.insert(obj.path_id, get("m_ColorSpace"));
                    textures.push((
                        name,
                        get("m_TextureFormat"),
                        get("m_Width"),
                        get("m_Height"),
                        get("m_MipCount"),
                    ));
                }
                49 if name == "metadata" => {
                    let script = v
                        .get("m_Script")
                        .map(|s| {
                            s.as_str().map(String::from).unwrap_or_else(|| {
                                s.as_bytes()
                                    .map(|b| String::from_utf8_lossy(b).into_owned())
                                    .unwrap_or_default()
                            })
                        })
                        .unwrap_or_default();
                    metadata = serde_json::from_str(&script).ok();
                }
                142 => {
                    if let Some(d) = v.get("m_Dependencies").and_then(|d| d.as_array()) {
                        for x in d {
                            if let Some(s) = x.as_str() {
                                deps.push(s.to_string());
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    let mut checks: Vec<GateCheck> = Vec::new();
    push_check(
        &mut checks,
        "root-count",
        root_gos.len() == 1,
        format!("{} parentless root(s)", root_gos.len()),
    );
    let want_root = format!("{sid}_{level}");
    let got_root = root_gos
        .first()
        .and_then(|go| go_names.get(go))
        .cloned()
        .unwrap_or_default();
    push_check(
        &mut checks,
        "root-name",
        got_root == want_root,
        format!("got {got_root:?} want {want_root:?}"),
    );
    push_check(
        &mut checks,
        "root-position",
        root_positions
            .iter()
            .all(|p| p[1] == 0.0 && p[0] % 16.0 == 0.0 && p[2] % 16.0 == 0.0),
        format!("{root_positions:?} want base*16 (parcel-grid multiple, y=0)"),
    );
    let want_tp = lod_target_platform(platform);
    push_check(
        &mut checks,
        "target-platform",
        target_platform.is_some() && target_platform == want_tp,
        format!("got {target_platform:?} want {want_tp:?} ({platform})"),
    );
    push_check(
        &mut checks,
        "material-count",
        materials.is_empty() != expect_content,
        format!(
            "{} material(s), expect_content={expect_content}",
            materials.len()
        ),
    );
    for (name, fid, pid, _) in &materials {
        push_check(
            &mut checks,
            format!("shader-pptr[{name}]"),
            (*fid, *pid) == (1, crate::shader::TEXARRAY_SHADER_PATH_ID),
            format!("({fid}, {pid})"),
        );
    }
    let orphan_mats: Vec<&String> = materials
        .iter()
        .filter(|(_, _, _, pid)| !renderer_mat_pids.contains(pid))
        .map(|(name, _, _, _)| name)
        .collect();
    push_check(
        &mut checks,
        "material-liveness",
        orphan_mats.is_empty(),
        format!(
            "{} material(s) with no referencing renderer: {orphan_mats:?}",
            orphan_mats.len()
        ),
    );
    let want_deps: Vec<String> = if expect_content {
        vec![
            crate::cabname::cab_name(&crate::shader::texarray_bundle_name(platform)).to_lowercase(),
        ]
    } else {
        Vec::new()
    };
    push_check(
        &mut checks,
        "assetbundle-dep",
        deps.iter().map(|d| d.to_lowercase()).collect::<Vec<_>>() == want_deps,
        format!("got {deps:?} want {want_deps:?}"),
    );
    let extent_span = mesh_extent.iter().cloned().fold(0.0f64, f64::max);
    push_check(
        &mut checks,
        "mesh-extent",
        (mesh_count > 0 && extent_span > 0.0) == expect_content,
        format!("{mesh_count} mesh(es), max extent {extent_span}, expect_content={expect_content}"),
    );
    push_check(
        &mut checks,
        "texture-count",
        textures.is_empty() != expect_content,
        format!(
            "{} texture(s), expect_content={expect_content}",
            textures.len()
        ),
    );
    for (name, fmt, w, h, mips) in &textures {
        let square_pot = *w > 0 && w == h && (*w as u64).is_power_of_two() && *w <= 512;
        let full_mips = square_pot && *mips == (*w as u64).trailing_zeros() as i64 + 1;
        let on_budget = atlas_budget.is_none_or(|b| *w == b as i64);
        push_check(
            &mut checks,
            format!("texture[{name}]"),
            *fmt == 25 && square_pot && full_mips && on_budget,
            format!("fmt={fmt} {w}x{h} mips={mips} budget={atlas_budget:?}"),
        );
    }
    let tex_sizes: std::collections::BTreeSet<(i64, i64)> =
        textures.iter().map(|(_, _, w, h, _)| (*w, *h)).collect();
    push_check(
        &mut checks,
        "texture-uniform-size",
        tex_sizes.len() <= 1,
        format!("{} distinct size(s): {tex_sizes:?}", tex_sizes.len()),
    );
    for (name, mgm, keywords, met, smooth, stc) in &mat_metal {
        if *mgm == 0 {
            continue;
        }
        let has_kw = keywords.iter().any(|k| k == "_METALLICSPECGLOSSMAP");
        push_check(
            &mut checks,
            format!("metal-gloss-map[{name}]"),
            has_kw && *met == 1.0 && *smooth == 1.0 && *stc == 0.0,
            format!(
                "keyword={has_kw} _Metallic={met} _Smoothness={smooth} \
                 _SmoothnessTextureChannel={stc}"
            ),
        );
        let cs = tex_colorspace.get(mgm).copied();
        push_check(
            &mut checks,
            format!("metal-gloss-map-linear[{name}]"),
            cs == Some(0),
            format!("m_ColorSpace={cs:?} want Some(0)"),
        );
    }
    let stray: Vec<&String> = mat_metal
        .iter()
        .filter(|(_, mgm, kw, _, _, _)| {
            *mgm == 0 && kw.iter().any(|k| k == "_METALLICSPECGLOSSMAP")
        })
        .map(|(n, _, _, _, _, _)| n)
        .collect();
    push_check(
        &mut checks,
        "metal-gloss-keyword-hygiene",
        stray.is_empty(),
        format!(
            "{} material(s) with _METALLICSPECGLOSSMAP but no bound map: {stray:?}",
            stray.len()
        ),
    );
    match &metadata {
        Some(m) => {
            push_check(
                &mut checks,
                "metadata-version",
                m.get("version").and_then(|v| v.as_str()) == Some("1.0"),
                format!("{:?}", m.get("version")),
            );
            let want_main = lods::lod_main_asset(&sid, level);
            push_check(
                &mut checks,
                "metadata-main-asset",
                m.get("mainAsset").and_then(|v| v.as_str()) == Some(want_main.as_str()),
                format!("got {:?} want {want_main:?}", m.get("mainAsset")),
            );
            let want_deps = if expect_content {
                serde_json::json!([crate::shader::texarray_bundle_name(platform)])
            } else {
                serde_json::json!([])
            };
            push_check(
                &mut checks,
                "metadata-deps",
                m.get("dependencies") == Some(&want_deps),
                format!("got {:?} want {want_deps}", m.get("dependencies")),
            );
        }
        None => {
            push_check(
                &mut checks,
                "metadata-present",
                false,
                "no metadata TextAsset".to_string(),
            );
        }
    }
    Ok(checks)
}

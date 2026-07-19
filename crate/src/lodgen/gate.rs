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
    let mut renderer_gos: HashSet<i64> = HashSet::new();
    let mut filter_mesh_by_go: HashMap<i64, i64> = HashMap::new();
    let mut colliders: Vec<(i64, i64)> = Vec::new();
    let mut textures: Vec<(String, i64, i64, i64, i64)> = Vec::new();
    let mut mesh_extent = [0.0f64; 3];
    let mut mesh_count = 0usize;
    struct MeshEnc {
        name: String,
        channels: Vec<(i64, i64, i64, i64)>,
        idxfmt: i64,
        vcount: i64,
        inline_len: i64,
        stream_size: i64,
        stream_path: String,
        has_bindposes: bool,
    }
    let mut mesh_encodings: Vec<MeshEnc> = Vec::new();
    let node_names: Vec<String> = bundle.files.iter().map(|f| f.name.clone()).collect();
    let mut cab_node: Option<String> = None;
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
        if cab_node.is_none() {
            cab_node = Some(file.name.clone());
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
                    if let Some(go) = v
                        .get("m_GameObject")
                        .and_then(|p| p.get("m_PathID"))
                        .and_then(|x| x.as_i64())
                    {
                        renderer_gos.insert(go);
                    }
                }
                33 => {
                    let go = v
                        .get("m_GameObject")
                        .and_then(|p| p.get("m_PathID"))
                        .and_then(|x| x.as_i64())
                        .unwrap_or(0);
                    let mesh = v
                        .get("m_Mesh")
                        .and_then(|p| p.get("m_PathID"))
                        .and_then(|x| x.as_i64())
                        .unwrap_or(0);
                    filter_mesh_by_go.insert(go, mesh);
                }
                64 => {
                    let go = v
                        .get("m_GameObject")
                        .and_then(|p| p.get("m_PathID"))
                        .and_then(|x| x.as_i64())
                        .unwrap_or(0);
                    let mesh = v
                        .get("m_Mesh")
                        .and_then(|p| p.get("m_PathID"))
                        .and_then(|x| x.as_i64())
                        .unwrap_or(0);
                    colliders.push((go, mesh));
                }
                43 => {
                    mesh_count += 1;
                    if let Some(ext) = v.get("m_LocalAABB").and_then(|b| b.get("m_Extent")) {
                        for (i, axis) in ["x", "y", "z"].iter().enumerate() {
                            let e = ext.get(axis).and_then(|x| x.as_f64()).unwrap_or(0.0);
                            mesh_extent[i] = mesh_extent[i].max(e.abs());
                        }
                    }
                    let vd = v.get("m_VertexData");
                    let channels: Vec<(i64, i64, i64, i64)> = vd
                        .and_then(|d| d.get("m_Channels"))
                        .and_then(|c| c.as_array())
                        .map(|a| {
                            a.iter()
                                .map(|c| {
                                    let f =
                                        |k: &str| c.get(k).and_then(|x| x.as_i64()).unwrap_or(-1);
                                    (f("stream"), f("offset"), f("format"), f("dimension"))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let vcount = vd
                        .and_then(|d| d.get("m_VertexCount"))
                        .and_then(|x| x.as_i64())
                        .unwrap_or(-1);
                    let inline_len = match vd.and_then(|d| d.get("m_DataSize")) {
                        Some(crate::value::Value::Bytes(b)) => b.len() as i64,
                        Some(crate::value::Value::Array(a)) => a.len() as i64,
                        _ => -1,
                    };
                    let idxfmt = v
                        .get("m_IndexFormat")
                        .and_then(|x| x.as_i64())
                        .unwrap_or(-1);
                    let sd = v.get("m_StreamData");
                    let stream_size = sd
                        .and_then(|s| s.get("size"))
                        .and_then(|x| x.as_i64())
                        .unwrap_or(0);
                    let stream_path = sd
                        .and_then(|s| s.get("path"))
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let has_bindposes = v
                        .get("m_BindPose")
                        .and_then(|b| b.as_array())
                        .is_some_and(|a| !a.is_empty());
                    mesh_encodings.push(MeshEnc {
                        name,
                        channels,
                        idxfmt,
                        vcount,
                        inline_len,
                        stream_size,
                        stream_path,
                        has_bindposes,
                    });
                }
                28 => {
                    let get = |k: &str| v.get(k).and_then(|x| x.as_i64()).unwrap_or(-1);
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
    // The client places the root at base*16; a baked offset would double it.
    push_check(
        &mut checks,
        "root-position",
        root_positions.iter().all(|p| *p == [0.0, 0.0, 0.0]),
        format!("{root_positions:?} want origin"),
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
    let want_shader_pid = if level == 0 {
        crate::shader::SHADER_PATH_ID
    } else {
        crate::shader::TEXARRAY_SHADER_PATH_ID
    };
    for (name, fid, pid, _) in &materials {
        push_check(
            &mut checks,
            format!("shader-pptr[{name}]"),
            (*fid, *pid) == (1, want_shader_pid),
            format!("({fid}, {pid}) want (1, {want_shader_pid})"),
        );
    }
    // an unreferenced material drags its texture into the preload table:
    // dead download + resident memory on every client
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
    let want_deps: Vec<String> = if !expect_content {
        Vec::new()
    } else if level == 0 {
        vec![crate::cabname::shader_bundle_cab(platform).to_lowercase()]
    } else {
        vec![
            crate::cabname::cab_name(&crate::shader::texarray_bundle_name(platform)).to_lowercase(),
        ]
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
    let want_channels = crate::mesh_layout::lod_channel_table();
    let cab = cab_node.unwrap_or_default();
    let want_stream_path = format!("archive:/{cab}/{cab}.resS");
    for m in &mesh_encodings {
        let name = &m.name;
        let idx_ok = (m.idxfmt == 0) == (m.vcount <= 65536);
        let detail = format!(
            "channels={:?} idxfmt={} verts={} streamed={} inline={} path={:?}",
            m.channels, m.idxfmt, m.vcount, m.stream_size, m.inline_len, m.stream_path
        );
        if m.has_bindposes {
            let pos_ok = m.channels.first() == Some(&(0, 0, 0, 3));
            let blend_ok = matches!(
                (m.channels.get(12), m.channels.get(13)),
                (Some(&(ws, 0, 0, 4)), Some(&(js, 16, 10, 4))) if ws == js && ws >= 1
            );
            let inline = m.stream_size == 0 && m.stream_path.is_empty() && m.inline_len > 0;
            push_check(
                &mut checks,
                format!("mesh-encoding[{name}]"),
                pos_ok && blend_ok && idx_ok && inline,
                detail,
            );
            continue;
        }
        let interleaved = m.channels == want_channels
            && m.stream_size == m.vcount * crate::mesh_layout::LOD_INTERLEAVED_STRIDE as i64;
        let streamed = m.inline_len == 0 && m.stream_path == want_stream_path;
        push_check(
            &mut checks,
            format!("mesh-encoding[{name}]"),
            interleaved && idx_ok && streamed,
            detail,
        );
    }
    let want_nodes: Vec<String> = if mesh_encodings.iter().any(|m| m.stream_size > 0) {
        vec![cab.clone(), format!("{cab}.resS")]
    } else {
        vec![cab.clone()]
    };
    push_check(
        &mut checks,
        "archive-nodes",
        node_names == want_nodes,
        format!("got {node_names:?} want {want_nodes:?}"),
    );
    let tex_count_ok = if level == 0 {
        expect_content || textures.is_empty()
    } else {
        textures.is_empty() != expect_content
    };
    push_check(
        &mut checks,
        "texture-count",
        tex_count_ok,
        format!(
            "{} texture(s), expect_content={expect_content}",
            textures.len()
        ),
    );
    for (name, fmt, w, h, mips) in &textures {
        if level == 0 {
            let fmt_ok = matches!(*fmt, 10 | 12 | 25);
            let dims_ok = *w > 0 && *h > 0 && *w <= 2048 && *h <= 2048;
            push_check(
                &mut checks,
                format!("texture[{name}]"),
                fmt_ok && dims_ok && *mips >= 1,
                format!("fmt={fmt} {w}x{h} mips={mips}"),
            );
            continue;
        }
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
    // production ships one uniform atlas size per bundle (shared texture-array slots)
    if level >= 1 {
        let tex_sizes: std::collections::BTreeSet<(i64, i64)> =
            textures.iter().map(|(_, _, w, h, _)| (*w, *h)).collect();
        push_check(
            &mut checks,
            "texture-uniform-size",
            tex_sizes.len() <= 1,
            format!("{} distinct size(s): {tex_sizes:?}", tex_sizes.len()),
        );
    }
    if !colliders.is_empty() {
        let bad_ref = colliders
            .iter()
            .filter(|(go, mesh)| *mesh == 0 || filter_mesh_by_go.get(go) != Some(mesh))
            .count();
        let rendered = colliders
            .iter()
            .filter(|(go, _)| renderer_gos.contains(go))
            .count();
        push_check(
            &mut checks,
            "collider-mesh-refs",
            bad_ref == 0 && rendered == 0,
            format!(
                "{} collider(s), {bad_ref} without matching MeshFilter mesh, {rendered} with a renderer",
                colliders.len()
            ),
        );
    }
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
            let want_deps = if !expect_content {
                serde_json::json!([])
            } else if level == 0 {
                serde_json::json!([format!("dcl/scene_ignore_{platform}")])
            } else {
                serde_json::json!([crate::shader::texarray_bundle_name(platform)])
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

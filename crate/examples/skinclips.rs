//! Per-clip breakdown of the baked SkinnedMeshRenderer bounds.
//!
//! usage: skinclips <source.glb> <built.bundle> [skin-index]
//!
//! For every skinned Mesh in the bundle, prints the root-bone-space box the
//! rest pose and each glTF animation clip contribute, so an oversized baked
//! `m_AABB` can be traced to the clip that drives it.

use abgen::skinbounds::{
    bake_skinned_aabb, bake_skinned_aabb_per_clip, bone_boxes_from_mesh, SkinBoundsInput,
};
use abgen::unity::bundle_file::{Bundle, FileContent};

const CLASS_MESH: i32 = 43;

/// Minimal GLB container split: JSON chunk + first BIN chunk.
fn parse_glb(bytes: &[u8]) -> (serde_json::Value, Vec<u8>) {
    let u32_at = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap()) as usize;
    assert_eq!(u32_at(0), 0x4654_6C67, "not a glb");
    let mut off = 12;
    let mut json = None;
    let mut bin = Vec::new();
    while off + 8 <= bytes.len() {
        let (len, ty) = (u32_at(off), u32_at(off + 4));
        let chunk = &bytes[off + 8..off + 8 + len];
        match ty {
            0x4E4F_534A => json = Some(serde_json::from_slice(chunk).expect("glb json")),
            0x004E_4942 if bin.is_empty() => bin = chunk.to_vec(),
            _ => {}
        }
        off += 8 + len;
    }
    (json.expect("glb has no json chunk"), bin)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: skinclips <source.glb> <built.bundle> [skin-index]");
        std::process::exit(2);
    }
    let skin_index: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);

    let glb_bytes = std::fs::read(&args[0]).expect("read glb");
    let (json, bin) = parse_glb(&glb_bytes);
    let buffers = vec![bin];
    let gltf = &json;

    let skin = &gltf["skins"][skin_index];
    let joints: Vec<usize> = skin["joints"]
        .as_array()
        .expect("skin joints")
        .iter()
        .map(|j| j.as_u64().unwrap() as usize)
        .collect();
    let node_children: Vec<Vec<usize>> = gltf["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| {
            n["children"]
                .as_array()
                .map(|c| c.iter().map(|x| x.as_u64().unwrap() as usize).collect())
                .unwrap_or_default()
        })
        .collect();
    let view = abgen::skeleton::SkinView {
        node_children: &node_children,
        joints: &joints,
        skeleton: skin["skeleton"].as_u64().map(|x| x as usize),
    };
    let root = abgen::skeleton::resolve_root_joint(&view).expect("root joint");
    println!(
        "skin {skin_index}: {} joints, root node [{root}] {:?}",
        joints.len(),
        gltf["nodes"][root]["name"].as_str().unwrap_or("")
    );

    let data = std::fs::read(&args[1]).expect("read bundle");
    let bundle = Bundle::load_bytes(&data).expect("parse bundle");
    for f in &bundle.files {
        let FileContent::Serialized(sf) = &f.content else {
            continue;
        };
        for obj in &sf.objects {
            if obj.class_id != CLASS_MESH {
                continue;
            }
            let Ok(mesh) = sf.read_typetree(obj) else {
                continue;
            };
            let boxes = bone_boxes_from_mesh(&mesh);
            if boxes.is_empty() {
                continue;
            }
            let name = mesh.get("m_Name").and_then(|n| n.as_str()).unwrap_or("");
            let input = SkinBoundsInput {
                gltf,
                buffers: &buffers,
                joints: &joints,
                root,
                bone_boxes: &boxes,
            };
            println!(
                "\n--- Mesh {name:?}  ({} of {} bones influence geometry)",
                boxes.iter().flatten().count(),
                boxes.len()
            );
            let fmt = |b: Option<([f64; 3], [f64; 3])>| -> String {
                match b {
                    Some((c, e)) => format!(
                        "center ({:7.3} {:7.3} {:7.3})  extent ({:.3} {:.3} {:.3})",
                        c[0], c[1], c[2], e[0], e[1], e[2]
                    ),
                    None => "(none)".to_string(),
                }
            };
            for (clip, b) in bake_skinned_aabb_per_clip(&input) {
                println!("  {clip:<32} {}", fmt(b));
            }
            println!(
                "  {:<32} {}",
                "== union (m_AABB)",
                fmt(bake_skinned_aabb(&input))
            );
        }
    }
}

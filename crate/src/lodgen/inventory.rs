//! Structural inventory of a LOD bundle: the counts a production `ab-cdn`
//! bundle and an abgen build can be compared on without byte parity
//! (materials, textures, meshes, vertices, triangles). Used by
//! `abgen-lod compare` and `qualify-corpus --reference-cdn`.
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::BTreeMap;

use crate::unity::bundle_file::{Bundle, FileContent};
use crate::value::Value;

const C_MATERIAL: i32 = 21;
const C_TEXTURE2D: i32 = 28;
const C_MESH: i32 = 43;

/// Property tolerance for the material diff (floats are stored as f32).
const PROP_TOL: f64 = 1e-4;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TextureEntry {
    pub name: String,
    /// Unity `TextureFormat` enum value (BC7 = 25).
    pub format: i64,
    pub width: i64,
    pub height: i64,
    pub mips: i64,
}

/// One `m_TexEnvs` slot of a material.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct TexBinding {
    pub bound: bool,
    pub scale: [f64; 2],
    pub offset: [f64; 2],
}

/// Everything `LODConversion.cs::SetLODShaderMaterial` writes into a
/// material, read back from the serialized object: the shader it was
/// re-parented onto, keyword set, render queue and `RenderType` tag, every
/// saved float, the colour vectors (`_PlaneClipping`, `_VerticalClipping`,
/// `_BaseColor`, ...) and each texture slot's binding + tiling/offset.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct MaterialEntry {
    pub name: String,
    /// `m_Shader` pptr as (fileID, pathID).
    pub shader: (i64, i64),
    pub keywords: Vec<String>,
    pub render_queue: i64,
    pub render_type: Option<String>,
    pub floats: BTreeMap<String, f64>,
    pub colors: BTreeMap<String, [f64; 4]>,
    pub textures: BTreeMap<String, TexBinding>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct BundleInventory {
    pub bytes: usize,
    pub materials: usize,
    pub textures: usize,
    pub texture_pixels: u64,
    pub meshes: usize,
    pub vertices: u64,
    pub triangles: u64,
    /// Per-texture detail for human-readable compares; kept out of the
    /// corpus report, where thousands of scenes would carry it.
    #[serde(skip)]
    pub texture_list: Vec<TextureEntry>,
    /// Per-material detail, same policy as `texture_list`; the corpus report
    /// carries the [`MaterialDiff`] instead.
    #[serde(skip)]
    pub material_list: Vec<MaterialEntry>,
}

/// Result of pairing our materials with the reference's by name and
/// comparing every property both sides declare. Properties present on one
/// side only are counted, not flagged: production ships URP defaults abgen
/// deliberately omits, and vice versa.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct MaterialDiff {
    pub matched: usize,
    pub only_ours: Vec<String>,
    pub only_reference: Vec<String>,
    /// `"{material}: {property} ours={..} ref={..}"`, one per differing property.
    pub mismatches: Vec<String>,
    pub props_only_ours: usize,
    pub props_only_reference: usize,
}

impl MaterialDiff {
    /// Same material set and every shared property agrees.
    pub fn identical(&self) -> bool {
        self.only_ours.is_empty() && self.only_reference.is_empty() && self.mismatches.is_empty()
    }
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= PROP_TOL
}

fn diff_one(name: &str, ours: &MaterialEntry, reference: &MaterialEntry, out: &mut MaterialDiff) {
    macro_rules! push {
        ($prop:expr, $a:expr, $b:expr $(,)?) => {
            out.mismatches
                .push(format!("{name}: {} ours={} ref={}", $prop, $a, $b))
        };
    }
    if ours.shader != reference.shader {
        push!(
            "shader",
            format!("{:?}", ours.shader),
            format!("{:?}", reference.shader)
        );
    }
    if ours.keywords != reference.keywords {
        push!(
            "keywords",
            format!("{:?}", ours.keywords),
            format!("{:?}", reference.keywords),
        );
    }
    if ours.render_queue != reference.render_queue {
        push!(
            "render_queue",
            ours.render_queue.to_string(),
            reference.render_queue.to_string(),
        );
    }
    if ours.render_type != reference.render_type {
        push!(
            "RenderType",
            format!("{:?}", ours.render_type),
            format!("{:?}", reference.render_type),
        );
    }
    for (prop, a) in &ours.floats {
        match reference.floats.get(prop) {
            Some(b) if !close(*a, *b) => push!(prop, a.to_string(), b.to_string()),
            Some(_) => {}
            None => out.props_only_ours += 1,
        }
    }
    out.props_only_reference += reference
        .floats
        .keys()
        .filter(|k| !ours.floats.contains_key(*k))
        .count();
    for (prop, a) in &ours.colors {
        match reference.colors.get(prop) {
            Some(b) if !a.iter().zip(b).all(|(x, y)| close(*x, *y)) => {
                push!(prop, format!("{a:?}"), format!("{b:?}"))
            }
            Some(_) => {}
            None => out.props_only_ours += 1,
        }
    }
    out.props_only_reference += reference
        .colors
        .keys()
        .filter(|k| !ours.colors.contains_key(*k))
        .count();
    for (slot, a) in &ours.textures {
        let Some(b) = reference.textures.get(slot) else {
            out.props_only_ours += 1;
            continue;
        };
        if a.bound != b.bound {
            push!(
                &format!("{slot}.bound"),
                a.bound.to_string(),
                b.bound.to_string(),
            );
        } else if a.bound
            && !(a.scale.iter().zip(&b.scale).all(|(x, y)| close(*x, *y))
                && a.offset.iter().zip(&b.offset).all(|(x, y)| close(*x, *y)))
        {
            push!(
                &format!("{slot}.st"),
                format!("{:?}/{:?}", a.scale, a.offset),
                format!("{:?}/{:?}", b.scale, b.offset),
            );
        }
    }
    out.props_only_reference += reference
        .textures
        .keys()
        .filter(|k| !ours.textures.contains_key(*k))
        .count();
}

/// Pairs materials by name (duplicates pair up in sorted order) and diffs
/// each pair; unpaired names land in `only_ours` / `only_reference`.
pub fn diff_materials(ours: &BundleInventory, reference: &BundleInventory) -> MaterialDiff {
    let mut out = MaterialDiff::default();
    let mut by_name: BTreeMap<&str, (Vec<&MaterialEntry>, Vec<&MaterialEntry>)> = BTreeMap::new();
    for m in &ours.material_list {
        by_name.entry(m.name.as_str()).or_default().0.push(m);
    }
    for m in &reference.material_list {
        by_name.entry(m.name.as_str()).or_default().1.push(m);
    }
    for (name, (a, b)) in by_name {
        let pairs = a.len().min(b.len());
        for i in 0..pairs {
            out.matched += 1;
            let label = if a.len() > 1 || b.len() > 1 {
                format!("{name}#{i}")
            } else {
                name.to_string()
            };
            diff_one(&label, a[i], b[i], &mut out);
        }
        out.only_ours
            .extend(std::iter::repeat_n(name.to_string(), a.len() - pairs));
        out.only_reference
            .extend(std::iter::repeat_n(name.to_string(), b.len() - pairs));
    }
    out
}

fn saved_pairs<'a>(v: &'a Value, list: &str) -> impl Iterator<Item = (&'a str, &'a Value)> {
    v.get("m_SavedProperties")
        .and_then(|p| p.get(list))
        .and_then(|l| l.as_array())
        .unwrap_or(&[])
        .iter()
        .filter_map(|e| {
            let pair = e.as_array()?;
            Some((pair.first()?.as_str()?, pair.get(1)?))
        })
}

fn xy(v: Option<&Value>) -> [f64; 2] {
    let g = |k: &str| {
        v.and_then(|p| p.get(k))
            .and_then(|x| x.as_f64())
            .unwrap_or(0.0)
    };
    [g("x"), g("y")]
}

fn material_entry(name: String, v: &Value) -> MaterialEntry {
    let pptr = |p: Option<&Value>| {
        let g = |k: &str| {
            p.and_then(|x| x.get(k))
                .and_then(|x| x.as_i64())
                .unwrap_or(0)
        };
        (g("m_FileID"), g("m_PathID"))
    };
    let mut keywords: Vec<String> = match v.get("m_ValidKeywords").and_then(|k| k.as_array()) {
        Some(list) => list
            .iter()
            .filter_map(|k| k.as_str().map(str::to_string))
            .collect(),
        // Pre-2021 layout: one space-separated string.
        None => v
            .get("m_ShaderKeywords")
            .and_then(|k| k.as_str())
            .map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default(),
    };
    keywords.sort();
    let render_type = v
        .get("stringTagMap")
        .and_then(|t| t.as_array())
        .and_then(|tags| {
            tags.iter().find_map(|e| {
                let pair = e.as_array()?;
                (pair.first()?.as_str()? == "RenderType")
                    .then(|| pair.get(1)?.as_str().map(str::to_string))
                    .flatten()
            })
        });
    let floats = saved_pairs(v, "m_Floats")
        .filter_map(|(k, x)| Some((k.to_string(), x.as_f64()?)))
        .collect();
    let colors = saved_pairs(v, "m_Colors")
        .filter_map(|(k, c)| {
            let g = |ch: &str| c.get(ch).and_then(|x| x.as_f64());
            Some((k.to_string(), [g("r")?, g("g")?, g("b")?, g("a")?]))
        })
        .collect();
    let textures = saved_pairs(v, "m_TexEnvs")
        .map(|(k, t)| {
            (
                k.to_string(),
                TexBinding {
                    bound: pptr(t.get("m_Texture")).1 != 0,
                    scale: xy(t.get("m_Scale")),
                    offset: xy(t.get("m_Offset")),
                },
            )
        })
        .collect();
    MaterialEntry {
        name,
        shader: pptr(v.get("m_Shader")),
        keywords,
        render_queue: v
            .get("m_CustomRenderQueue")
            .and_then(|x| x.as_i64())
            .unwrap_or(-1),
        render_type,
        floats,
        colors,
        textures,
    }
}

/// Signed `ours - reference` deltas; `vertices_pct` is relative to the
/// reference (0 when the reference has no vertices).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct InventoryDelta {
    pub bytes: i64,
    pub materials: i64,
    pub textures: i64,
    pub meshes: i64,
    pub vertices: i64,
    pub triangles: i64,
    pub vertices_pct: f64,
}

impl BundleInventory {
    pub fn delta_from(&self, reference: &BundleInventory) -> InventoryDelta {
        let d = |a: u64, b: u64| a as i64 - b as i64;
        InventoryDelta {
            bytes: d(self.bytes as u64, reference.bytes as u64),
            materials: d(self.materials as u64, reference.materials as u64),
            textures: d(self.textures as u64, reference.textures as u64),
            meshes: d(self.meshes as u64, reference.meshes as u64),
            vertices: d(self.vertices, reference.vertices),
            triangles: d(self.triangles, reference.triangles),
            vertices_pct: if reference.vertices == 0 {
                0.0
            } else {
                d(self.vertices, reference.vertices) as f64 * 100.0 / reference.vertices as f64
            },
        }
    }
}

pub fn inventory(data: &[u8]) -> Result<BundleInventory> {
    let bundle = Bundle::load_bytes(data).context("parse bundle")?;
    let mut inv = BundleInventory {
        bytes: data.len(),
        ..Default::default()
    };
    for file in &bundle.files {
        let FileContent::Serialized(sf) = &file.content else {
            continue;
        };
        for obj in &sf.objects {
            match obj.class_id {
                C_MATERIAL => {
                    let v = sf
                        .read_typetree(obj)
                        .with_context(|| format!("typetree Material pid {}", obj.path_id))?;
                    let name = v
                        .get("m_Name")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    inv.materials += 1;
                    inv.material_list.push(material_entry(name, &v));
                }
                C_TEXTURE2D => {
                    let v = sf
                        .read_typetree(obj)
                        .with_context(|| format!("typetree Texture2D pid {}", obj.path_id))?;
                    let get = |k: &str| v.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
                    let entry = TextureEntry {
                        name: v
                            .get("m_Name")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string(),
                        format: get("m_TextureFormat"),
                        width: get("m_Width"),
                        height: get("m_Height"),
                        mips: get("m_MipCount"),
                    };
                    inv.textures += 1;
                    inv.texture_pixels +=
                        (entry.width.max(0) as u64) * (entry.height.max(0) as u64);
                    inv.texture_list.push(entry);
                }
                C_MESH => {
                    let v = sf
                        .read_typetree(obj)
                        .with_context(|| format!("typetree Mesh pid {}", obj.path_id))?;
                    inv.meshes += 1;
                    inv.vertices += v
                        .get("m_VertexData")
                        .and_then(|x| x.get("m_VertexCount"))
                        .and_then(|x| x.as_i64())
                        .unwrap_or(0)
                        .max(0) as u64;
                    if let Some(subs) = v.get("m_SubMeshes").and_then(|x| x.as_array()) {
                        for s in subs {
                            // MeshTopology.Triangles == 0; strips/lines/points
                            // never appear in a LOD bundle but don't count as tris.
                            let topology = s.get("topology").and_then(|x| x.as_i64()).unwrap_or(0);
                            if topology != 0 {
                                continue;
                            }
                            let index_count =
                                s.get("indexCount").and_then(|x| x.as_i64()).unwrap_or(0);
                            inv.triangles += index_count.max(0) as u64 / 3;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    inv.texture_list.sort_by(|a, b| a.name.cmp(&b.name));
    inv.material_list.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(inv)
}

/// Bundle bytes from a local path or an `http(s)://` URL. `Ok(None)` is a
/// clean 404 (the reference CDN never built this scene), every other failure
/// is an error.
#[cfg(not(target_arch = "wasm32"))]
pub fn load_locator(locator: &str) -> Result<Option<Vec<u8>>> {
    if !(locator.starts_with("http://") || locator.starts_with("https://")) {
        return std::fs::read(locator)
            .map(Some)
            .with_context(|| format!("read {locator}"));
    }
    let response = ureq::get(locator)
        .config()
        .timeout_global(Some(std::time::Duration::from_secs(120)))
        .build()
        .header("User-Agent", crate::catalyst::UA)
        .call();
    let response = match response {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(404)) => return Ok(None),
        Err(e) => return Err(anyhow::anyhow!("GET {locator}: {e}")),
    };
    let mut bytes = Vec::new();
    use std::io::Read;
    response
        .into_body()
        .into_reader()
        .take(512 * 1024 * 1024)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read body {locator}"))?;
    Ok(Some(bytes))
}

/// Production key of a level-1 style LOD bundle under a CDN base
/// (`{base}/LOD/{level}/{sid}_{level}_{platform}`).
pub fn reference_url(cdn_base: &str, scene_id: &str, level: u32, platform: &str) -> String {
    format!(
        "{}/LOD/{level}/{}",
        cdn_base.trim_end_matches('/'),
        crate::lods::lod_bundle_name(scene_id, level, platform)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_is_ours_minus_reference_with_reference_relative_percent() {
        let ours = BundleInventory {
            bytes: 100,
            materials: 3,
            textures: 2,
            meshes: 1,
            vertices: 900,
            triangles: 300,
            ..Default::default()
        };
        let reference = BundleInventory {
            bytes: 120,
            materials: 3,
            textures: 3,
            meshes: 1,
            vertices: 1000,
            triangles: 400,
            ..Default::default()
        };
        let d = ours.delta_from(&reference);
        assert_eq!(d.bytes, -20);
        assert_eq!(d.materials, 0);
        assert_eq!(d.textures, -1);
        assert_eq!(d.vertices, -100);
        assert_eq!(d.triangles, -100);
        assert!((d.vertices_pct + 10.0).abs() < 1e-9);
        assert_eq!(
            ours.delta_from(&BundleInventory::default()).vertices_pct,
            0.0
        );
    }

    #[test]
    fn reference_url_follows_the_production_key_layout() {
        assert_eq!(
            reference_url("https://ab-cdn.decentraland.org/", "BafkAbc", 1, "mac"),
            "https://ab-cdn.decentraland.org/LOD/1/bafkabc_1_mac"
        );
    }

    fn mat(name: &str, zwrite: f64, bound: bool) -> MaterialEntry {
        MaterialEntry {
            name: name.to_string(),
            shader: (1, 42),
            keywords: vec!["_SURFACE_TYPE_TRANSPARENT".into()],
            render_queue: 3000,
            render_type: None,
            floats: [
                ("_ZWrite".to_string(), zwrite),
                ("_Surface".to_string(), 1.0),
            ]
            .into_iter()
            .collect(),
            colors: [("_PlaneClipping".to_string(), [-0.05, 16.05, -0.05, 16.05])]
                .into_iter()
                .collect(),
            textures: [(
                "_BaseMap".to_string(),
                TexBinding {
                    bound,
                    scale: [1.0, 1.0],
                    offset: [0.0, 0.0],
                },
            )]
            .into_iter()
            .collect(),
        }
    }

    #[test]
    fn material_diff_pairs_by_name_and_flags_only_shared_property_disagreements() {
        let ours = BundleInventory {
            material_list: vec![mat("opaque", 1.0, true), mat("glass", 0.0, true)],
            ..Default::default()
        };
        let mut ref_glass = mat("glass", 1.0, false);
        ref_glass.floats.insert("_QueueOffset".to_string(), 0.0);
        let reference = BundleInventory {
            material_list: vec![mat("opaque", 1.0, true), ref_glass, mat("extra", 1.0, true)],
            ..Default::default()
        };
        let d = diff_materials(&ours, &reference);
        assert_eq!(d.matched, 2);
        assert!(d.only_ours.is_empty());
        assert_eq!(d.only_reference, vec!["extra".to_string()]);
        assert_eq!(d.mismatches.len(), 2, "{:?}", d.mismatches);
        assert!(d
            .mismatches
            .iter()
            .any(|m| m == "glass: _ZWrite ours=0 ref=1"));
        assert!(d
            .mismatches
            .iter()
            .any(|m| m.starts_with("glass: _BaseMap.bound")));
        assert_eq!(d.props_only_reference, 1, "_QueueOffset is informational");
        assert!(!d.identical());
        assert!(diff_materials(&ours, &ours).identical());
    }

    #[test]
    fn texture_list_is_not_serialized() {
        let inv = BundleInventory {
            textures: 1,
            texture_list: vec![TextureEntry {
                name: "t".into(),
                format: 25,
                width: 512,
                height: 512,
                mips: 10,
            }],
            ..Default::default()
        };
        let json = serde_json::to_string(&inv).unwrap();
        assert!(json.contains("\"textures\":1"));
        assert!(!json.contains("texture_list"));
    }
}

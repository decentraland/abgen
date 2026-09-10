//! Structural inventory of a LOD bundle: the counts a production `ab-cdn`
//! bundle and an abgen build can be compared on without byte parity
//! (materials, textures, meshes, vertices, triangles). Used by
//! `abgen-lod compare` and `qualify-corpus --reference-cdn`.
use anyhow::{Context, Result};
use serde::Serialize;

use crate::unity::bundle_file::{Bundle, FileContent};

const C_MATERIAL: i32 = 21;
const C_TEXTURE2D: i32 = 28;
const C_MESH: i32 = 43;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TextureEntry {
    pub name: String,
    /// Unity `TextureFormat` enum value (BC7 = 25).
    pub format: i64,
    pub width: i64,
    pub height: i64,
    pub mips: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
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
                C_MATERIAL => inv.materials += 1,
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

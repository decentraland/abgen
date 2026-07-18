pub mod emit;
pub mod pack;

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const BVW_PLATFORM: &str = "bvwebgpu";
pub const BVW_PROFILE: &str = "bv1";
pub const BVW_MAX_PACK_BYTES: u64 = 256 * 1024 * 1024;
pub const BVW_TEXTURE_MAX: u32 = 1024;

const VIDEO_EXTS: [&str; 8] = [
    ".mp4", ".webm", ".ogg", ".ogv", ".m4a", ".mov", ".m3u8", ".ts",
];

pub fn pack_file_name(entity: &str) -> String {
    format!("{entity}_{BVW_PROFILE}.pack")
}

pub fn is_video(file: &str) -> bool {
    let f = file.to_lowercase();
    VIDEO_EXTS.iter().any(|e| f.ends_with(e))
}

pub fn kind_for(file: &str) -> &'static str {
    let f = file.to_lowercase();
    if f.ends_with(".glb") || f.ends_with(".gltf") {
        "glb"
    } else if f.ends_with(".png") || f.ends_with(".jpg") || f.ends_with(".jpeg") {
        "img"
    } else {
        "raw"
    }
}

pub fn client_path(file: &str) -> String {
    file.replace('\\', "/").to_lowercase()
}

fn file_ext(file: &str) -> &'static str {
    if file.to_lowercase().ends_with(".gltf") {
        ".gltf"
    } else {
        ".glb"
    }
}

struct SpoolDir(PathBuf);

impl Drop for SpoolDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl crate::live::Proxy {
    pub fn build_bvwebgpu_pack(self: &Arc<Self>, out_root: &Path, cid: &str) -> Result<()> {
        let started = std::time::Instant::now();
        let ctx = self.entity_ctx(cid)?;

        let mut order: Vec<String> = Vec::new();
        let mut by_path: HashMap<String, (String, String)> = HashMap::new();
        for c in &ctx.scene.content {
            if is_video(&c.file) {
                continue;
            }
            let p = client_path(&c.file);
            if !by_path.contains_key(&p) {
                order.push(p.clone());
            }
            by_path.insert(p, (c.hash.clone(), c.file.clone()));
        }
        order.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));

        let dir = out_root.join(cid).join(BVW_PLATFORM);
        std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
        let spool = SpoolDir(crate::tmppath::tmp_sibling(&dir.join("spool")));
        std::fs::create_dir_all(&spool.0)
            .with_context(|| format!("mkdir {}", spool.0.display()))?;

        let content_by_file = &ctx.content_by_file;
        let mut meta: HashMap<String, (u64, String)> = HashMap::new();
        let mut spooled: HashMap<String, PathBuf> = HashMap::new();
        let mut kinds: HashMap<String, &'static str> = HashMap::new();
        let mut spooled_bytes: u64 = 0;
        for p in &order {
            let (hash, file) = &by_path[p];
            if meta.contains_key(hash) {
                continue;
            }
            let raw = self
                .content_bytes_allow_empty(hash)
                .with_context(|| format!("content {hash} ({file})"))?;
            let kind = kind_for(file);
            let resolve_fn = |uri: &str| -> Option<Vec<u8>> {
                let h = crate::naming::uri_content_hash(uri, file, content_by_file)?;
                if let Err(e) = self.ensure_content(h) {
                    tracing::warn!(uri = %uri, hash = %h, error = %format!("{e:#}"), "bvwebgpu resolve");
                }
                self.content_store().fetch(h).ok()
            };
            let transformed = match kind {
                "glb" => emit::transform_glb(&raw, file_ext(file), Some(&resolve_fn)),
                "img" => emit::transform_img(&raw),
                _ => Ok(raw.clone()),
            };
            let (bytes, kind) = match transformed {
                Ok(bytes) => (bytes, kind),
                Err(e) => {
                    tracing::warn!(
                        entity = %cid,
                        file = %file,
                        hash = %hash,
                        error = %format!("{e:#}"),
                        "bvwebgpu transform failed; shipping raw bytes"
                    );
                    (raw, "raw")
                }
            };
            spooled_bytes += bytes.len() as u64;
            if spooled_bytes > BVW_MAX_PACK_BYTES {
                bail!(
                    "bvwebgpu pack for {cid} is at least {spooled_bytes} bytes, over the {BVW_MAX_PACK_BYTES} cap"
                );
            }
            let sp = spool.0.join(format!("{}.blob", meta.len()));
            std::fs::write(&sp, &bytes).with_context(|| format!("write {}", sp.display()))?;
            meta.insert(
                hash.clone(),
                (bytes.len() as u64, crate::hashes::sha256_hex(&bytes)),
            );
            spooled.insert(hash.clone(), sp);
            kinds.insert(hash.clone(), kind);
        }

        let entries: Vec<pack::EntrySpec> = order
            .iter()
            .map(|p| {
                let (hash, _) = &by_path[p];
                pack::EntrySpec {
                    path: p.clone(),
                    cid: hash.clone(),
                    kind: kinds[hash],
                }
            })
            .collect();
        let plan = pack::plan_pack(cid, &entries, &meta, BVW_MAX_PACK_BYTES)?;

        let write = |name: &str, bytes: &[u8]| -> Result<()> {
            let dst = dir.join(name);
            let tmp = crate::tmppath::tmp_sibling(&dst);
            std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
            std::fs::rename(&tmp, &dst).ok();
            Ok(())
        };
        let pack_name = pack_file_name(cid);
        let pack_dst = dir.join(&pack_name);
        let pack_tmp = crate::tmppath::tmp_sibling(&pack_dst);
        {
            use std::io::Write;
            let f = std::fs::File::create(&pack_tmp)
                .with_context(|| format!("create {}", pack_tmp.display()))?;
            let mut w = std::io::BufWriter::new(f);
            pack::write_pack(
                &plan,
                |c| {
                    std::fs::File::open(&spooled[c])
                        .with_context(|| format!("open {}", spooled[c].display()))
                },
                &mut w,
            )?;
            w.flush()
                .with_context(|| format!("flush {}", pack_tmp.display()))?;
        }
        let pack_bytes =
            std::fs::read(&pack_tmp).with_context(|| format!("read {}", pack_tmp.display()))?;
        std::fs::rename(&pack_tmp, &pack_dst).ok();
        write("pack.json", &plan.index_json)?;
        let br = crate::compress::brotli(&pack_bytes)?;
        write(&format!("{pack_name}.br"), &br)?;

        if self.space_configured() {
            self.space_put_key(
                &format!("{BVW_PLATFORM}/{BVW_PROFILE}/{cid}.pack"),
                &pack_bytes,
                "application/octet-stream",
            );
            self.space_put_key(
                &format!("{BVW_PLATFORM}/{BVW_PROFILE}/{cid}.pack.br"),
                &br,
                "application/octet-stream",
            );
        }
        tracing::info!(
            entity = %cid,
            files = entries.len(),
            bytes = pack_bytes.len(),
            ms = started.elapsed().as_millis() as u64,
            "bvwebgpu pack built"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const GOLDEN_PACK_SHA256: &str =
        "d5b1eeb8f0956d8a7d190a5de030c71875784f5037a7b96db7707e32f1ae70cd";

    fn fixture_routes(entity: &str) -> (crate::live::stub::Routes, Vec<u8>, Vec<u8>, Vec<u8>) {
        let png = emit::tests::png_bytes(8, 8, [180, 40, 220, 255]);
        let normal_png = emit::tests::png_bytes(4, 4, [128, 128, 255, 255]);
        let glb = {
            let mut bin: Vec<u8> = Vec::new();
            bin.extend_from_slice(&normal_png);
            emit::tests::mk_glb(
                json!({
                    "meshes": [{"primitives": [{
                        "attributes": {"POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2},
                        "indices": 3, "material": 0
                    }]}],
                    "materials": [{"normalTexture": {"index": 0}}],
                    "textures": [{"source": 0}],
                    "images": [{"bufferView": 0, "mimeType": "image/png"}],
                    "bufferViews": [{"buffer": 0, "byteOffset": 0, "byteLength": normal_png.len()}],
                    "accessors": [
                        {"componentType": 5126, "count": 0, "type": "VEC3"},
                        {"componentType": 5126, "count": 0, "type": "VEC3"},
                        {"componentType": 5126, "count": 0, "type": "VEC2"},
                        {"componentType": 5123, "count": 0, "type": "SCALAR"}
                    ]
                }),
                &bin,
            )
        };
        let tiny = emit::tests::png_bytes(2, 2, [1, 2, 3, 4]);
        let bad = b"corrupt png bytes".to_vec();
        let js = b"console.log('hi')".to_vec();
        let ent = serde_json::to_vec(&json!({
            "id": entity,
            "type": "scene",
            "pointers": ["9,9"],
            "content": [
                {"file": "Models/Scene.GLB", "hash": "bafkglb"},
                {"file": "tex\\Color.PNG", "hash": "bafkpng"},
                {"file": "tiny.png", "hash": "bafktiny"},
                {"file": "bad.png", "hash": "bafkbad"},
                {"file": "movie.webm", "hash": "bafkvid"},
                {"file": "game.js", "hash": "bafkjs"},
                {"file": "dup/game.js", "hash": "bafkjs"}
            ],
            "metadata": {}
        }))
        .unwrap();
        let routes = vec![
            (format!("/contents/{entity}"), 200, ent),
            ("/contents/bafkglb".to_string(), 200, glb),
            ("/contents/bafkpng".to_string(), 200, png.clone()),
            ("/contents/bafktiny".to_string(), 200, tiny.clone()),
            ("/contents/bafkbad".to_string(), 200, bad.clone()),
            ("/contents/bafkjs".to_string(), 200, js),
        ];
        (routes, tiny, bad, png)
    }

    fn build_once(tag: &str, routes: crate::live::stub::Routes, entity: &str) -> Vec<u8> {
        let (cat_host, _cs) = crate::live::stub::serve(routes);
        let (space_host, _ss) = crate::live::stub::serve(vec![]);
        let cache = std::env::temp_dir().join(format!(
            "abgen-bvwebgpu-golden-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&cache);
        std::fs::create_dir_all(&cache).unwrap();
        let proxy = crate::live::stub::stub_proxy_at(
            &space_host,
            &format!("http://{cat_host}"),
            false,
            &cache.join("cache"),
        );
        let out = cache.join("out");
        proxy.build_bvwebgpu_pack(&out, entity).unwrap();
        let bytes = std::fs::read(
            out.join(entity)
                .join(BVW_PLATFORM)
                .join(pack_file_name(entity)),
        )
        .unwrap();
        assert!(out
            .join(entity)
            .join(BVW_PLATFORM)
            .join(format!("{}.br", pack_file_name(entity)))
            .is_file());
        assert!(out
            .join(entity)
            .join(BVW_PLATFORM)
            .join("pack.json")
            .is_file());
        let _ = std::fs::remove_dir_all(&cache);
        bytes
    }

    #[test]
    fn empty_content_files_pack_as_zero_length_entries() {
        let entity = "bafkemptyent";
        let js = b"console.log('hi')".to_vec();
        let ent = serde_json::to_vec(&json!({
            "id": entity,
            "type": "scene",
            "pointers": ["9,9"],
            "content": [
                {"file": "models/put_models_here.txt", "hash": "bafkempty"},
                {"file": "empty.png", "hash": "bafkemptypng"},
                {"file": "game.js", "hash": "bafkjs"}
            ],
            "metadata": {}
        }))
        .unwrap();
        let routes = vec![
            (format!("/contents/{entity}"), 200, ent),
            ("/contents/bafkempty".to_string(), 200, Vec::new()),
            ("/contents/bafkemptypng".to_string(), 200, Vec::new()),
            ("/contents/bafkjs".to_string(), 200, js.clone()),
        ];
        let first = build_once("e1", routes.clone(), entity);
        let second = build_once("e2", routes, entity);
        assert_eq!(first, second);
        let parsed = pack::parse_pack(&first).unwrap();
        let by_path: std::collections::HashMap<&str, &pack::PackEntry> = parsed
            .index
            .files
            .iter()
            .map(|e| (e.path.as_str(), e))
            .collect();
        let txt = by_path["models/put_models_here.txt"];
        assert_eq!((txt.len, txt.kind.as_str()), (0, "raw"));
        assert_eq!(
            txt.sha256,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let png = by_path["empty.png"];
        assert_eq!((png.len, png.kind.as_str()), (0, "raw"));
        assert_eq!(
            pack::entry_slice(&first, &parsed, by_path["game.js"]),
            &js[..]
        );
    }

    #[test]
    fn pack_build_is_deterministic_and_matches_membership_rules() {
        let entity = "bafkgoldenent";
        let (routes, tiny, bad, _png) = fixture_routes(entity);
        let first = build_once("a", routes.clone(), entity);
        let second = build_once("b", routes, entity);
        assert_eq!(first, second, "same inputs must give byte-identical packs");

        let parsed = pack::parse_pack(&first).unwrap();
        assert_eq!(parsed.index.entity, entity);
        assert_eq!(parsed.index.profile, BVW_PROFILE);
        let paths: Vec<&str> = parsed.index.files.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "bad.png",
                "dup/game.js",
                "game.js",
                "models/scene.glb",
                "tex/color.png",
                "tiny.png"
            ]
        );
        let by_path: std::collections::HashMap<&str, &pack::PackEntry> = parsed
            .index
            .files
            .iter()
            .map(|e| (e.path.as_str(), e))
            .collect();
        assert_eq!(by_path["bad.png"].kind, "raw");
        assert_eq!(
            pack::entry_slice(&first, &parsed, by_path["bad.png"]),
            &bad[..]
        );
        assert_eq!(by_path["tiny.png"].kind, "img");
        assert_eq!(
            pack::entry_slice(&first, &parsed, by_path["tiny.png"]),
            &tiny[..]
        );
        assert_eq!(by_path["models/scene.glb"].kind, "glb");
        assert_eq!(by_path["tex/color.png"].kind, "img");
        let d1 = by_path["dup/game.js"];
        let d2 = by_path["game.js"];
        assert_eq!((d1.off, d1.len), (d2.off, d2.len));

        let dds = pack::entry_slice(&first, &parsed, by_path["tex/color.png"]);
        let dds = ddsfile::Dds::read(std::io::Cursor::new(dds)).unwrap();
        assert_eq!((dds.get_width(), dds.get_height()), (8, 8));
        assert_eq!(dds.get_num_mipmap_levels(), 4);

        let glb = pack::entry_slice(&first, &parsed, by_path["models/scene.glb"]);
        assert_eq!(&glb[..4], b"glTF");

        assert_eq!(
            crate::hashes::sha256_hex(&first),
            GOLDEN_PACK_SHA256,
            "transform output drifted: bump BVW_PROFILE and re-pin the golden"
        );
    }
}

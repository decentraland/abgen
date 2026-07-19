use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::Path;

use super::assemble::{fetch_cached, resolve_placement_hash, sanitize_glb_json_padding};
use super::placements::Placement;

#[derive(Debug)]
pub struct RollupOutcome {
    pub glb: Vec<u8>,
    pub instances: usize,
    pub unique_glbs: usize,
    pub log: Vec<String>,
}

struct SourceDoc {
    json: Value,
    buffers: Vec<Vec<u8>>,
    name: String,
}

fn glb_chunks(bytes: &[u8]) -> Result<(Vec<u8>, Option<Vec<u8>>)> {
    if bytes.len() < 20 || &bytes[0..4] != b"glTF" {
        bail!("not a GLB container");
    }
    let mut pos = 12usize;
    let mut json: Option<Vec<u8>> = None;
    let mut bin: Option<Vec<u8>> = None;
    while pos + 8 <= bytes.len() {
        let clen = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        let ctype = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap());
        let start = pos + 8;
        let end = start
            .checked_add(clen)
            .filter(|&e| e <= bytes.len())
            .ok_or_else(|| anyhow!("GLB chunk overruns container"))?;
        match ctype {
            0x4E4F_534A => json.get_or_insert_with(|| bytes[start..end].to_vec()),
            0x004E_4942 => bin.get_or_insert_with(|| bytes[start..end].to_vec()),
            _ => &mut Vec::new(),
        };
        pos = end;
    }
    Ok((json.ok_or_else(|| anyhow!("GLB has no JSON chunk"))?, bin))
}

use crate::gltf::decode_data_uri;

fn image_mime(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0xFF, 0xD8]) {
        "image/jpeg"
    } else {
        "image/png"
    }
}

fn as_arr(doc: &Value, key: &str) -> Vec<Value> {
    doc.get(key)
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

fn shift(v: &mut Value, key: &str, base: usize) {
    if let Some(n) = v.get(key).and_then(|x| x.as_u64()) {
        v[key] = json!(n as usize + base);
    }
}

fn map_via(v: &mut Value, key: &str, table: &[usize]) {
    if let Some(n) = v.get(key).and_then(|x| x.as_u64()) {
        v[key] = json!(table[n as usize]);
    }
}

// textureInfo objects are the only material sub-objects carrying an integer
// "index"; KHR material extensions nest them, so renumber recursively.
fn renumber_texture_infos(v: &mut Value, tex_table: &[usize]) {
    match v {
        Value::Object(m) => {
            if let Some(n) = m.get("index").and_then(|x| x.as_u64()) {
                m.insert("index".into(), json!(tex_table[n as usize]));
            }
            for (_, x) in m.iter_mut() {
                renumber_texture_infos(x, tex_table);
            }
        }
        Value::Array(a) => {
            for x in a {
                renumber_texture_infos(x, tex_table);
            }
        }
        _ => {}
    }
}

#[derive(Default)]
struct Merged {
    bin: Vec<u8>,
    buffer_views: Vec<Value>,
    accessors: Vec<Value>,
    samplers: Vec<Value>,
    images: Vec<Value>,
    textures: Vec<Value>,
    materials: Vec<Value>,
    meshes: Vec<Value>,
    nodes: Vec<Value>,
    skins: Vec<Value>,
    roots: Vec<usize>,
    ext_used: Vec<Value>,
    ext_required: Vec<Value>,
    image_by_hash: HashMap<[u8; 32], usize>,
    sampler_by_json: HashMap<String, usize>,
    texture_by_json: HashMap<String, usize>,
    material_by_json: HashMap<String, usize>,
    material_names: std::collections::HashSet<String>,
}

struct MergedSource {
    acc_base: usize,
    mesh_base: usize,
    node_tpl: Vec<Value>,
    skin_tpl: Vec<Value>,
    scene_roots: Vec<usize>,
}

impl Merged {
    fn append_bin(&mut self, bytes: &[u8]) -> usize {
        while !self.bin.len().is_multiple_of(4) {
            self.bin.push(0);
        }
        let at = self.bin.len();
        self.bin.extend_from_slice(bytes);
        at
    }

    fn intern_sampler(&mut self, s: &Value) -> usize {
        let key = s.to_string();
        if let Some(&i) = self.sampler_by_json.get(&key) {
            return i;
        }
        self.samplers.push(s.clone());
        self.sampler_by_json.insert(key, self.samplers.len() - 1);
        self.samplers.len() - 1
    }

    fn intern_image(&mut self, bytes: &[u8], img: &Value) -> usize {
        let hash: [u8; 32] = crate::hashes::sha256(bytes);
        if let Some(&i) = self.image_by_hash.get(&hash) {
            return i;
        }
        let at = self.append_bin(bytes);
        let bv = self.buffer_views.len();
        self.buffer_views.push(json!({
            "buffer": 0, "byteOffset": at, "byteLength": bytes.len()
        }));
        let mut out = Map::new();
        if let Some(n) = img.get("name") {
            out.insert("name".into(), n.clone());
        }
        out.insert("bufferView".into(), json!(bv));
        out.insert("mimeType".into(), json!(image_mime(bytes)));
        self.images.push(Value::Object(out));
        self.image_by_hash.insert(hash, self.images.len() - 1);
        self.images.len() - 1
    }

    fn intern_texture(&mut self, t: Value) -> usize {
        let key = t.to_string();
        if let Some(&i) = self.texture_by_json.get(&key) {
            return i;
        }
        self.textures.push(t);
        self.texture_by_json.insert(key, self.textures.len() - 1);
        self.textures.len() - 1
    }

    fn intern_material(&mut self, mut m: Value) -> usize {
        let key = m.to_string();
        if let Some(&i) = self.material_by_json.get(&key) {
            return i;
        }
        // distinct materials sharing a name would collide on content-derived
        // path IDs and .mat container keys; uniquify like production's merge
        if let Some(name) = m.get("name").and_then(|x| x.as_str()).map(String::from) {
            if !name.is_empty() {
                let mut candidate = name.clone();
                let mut k = 1;
                while !self.material_names.insert(candidate.clone()) {
                    candidate = format!("{name}_{k}");
                    k += 1;
                }
                if candidate != name {
                    m["name"] = json!(candidate);
                }
            }
        }
        self.materials.push(m);
        self.material_by_json.insert(key, self.materials.len() - 1);
        self.materials.len() - 1
    }

    fn merge_source(&mut self, src: &SourceDoc) -> Result<MergedSource> {
        let doc = &src.json;
        let buf_base: Vec<usize> = src.buffers.iter().map(|b| self.append_bin(b)).collect();

        let bv_base = self.buffer_views.len();
        for mut bv in as_arr(doc, "bufferViews") {
            let bi = bv.get("buffer").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
            let off = bv.get("byteOffset").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
            bv["buffer"] = json!(0);
            bv["byteOffset"] = json!(off + buf_base.get(bi).copied().unwrap_or(0));
            self.buffer_views.push(bv);
        }

        let acc_base = self.accessors.len();
        for mut acc in as_arr(doc, "accessors") {
            shift(&mut acc, "bufferView", bv_base);
            if let Some(sparse) = acc.get_mut("sparse") {
                for part in ["indices", "values"] {
                    if let Some(p) = sparse.get_mut(part) {
                        shift(p, "bufferView", bv_base);
                    }
                }
            }
            self.accessors.push(acc);
        }

        let src_samplers = as_arr(doc, "samplers");
        let sampler_table: Vec<usize> = src_samplers
            .iter()
            .map(|s| self.intern_sampler(s))
            .collect();

        let src_images = as_arr(doc, "images");
        let mut image_table: Vec<usize> = Vec::with_capacity(src_images.len());
        for img in &src_images {
            let bytes: Vec<u8> = if let Some(uri) = img.get("uri").and_then(|x| x.as_str()) {
                decode_data_uri(uri)
                    .ok_or_else(|| anyhow!("{}: unresolvable image uri {uri:?}", src.name))?
            } else {
                let bv = img
                    .get("bufferView")
                    .and_then(|x| x.as_u64())
                    .ok_or_else(|| anyhow!("{}: image without uri or bufferView", src.name))?
                    as usize;
                src.view_bytes(bv)
                    .ok_or_else(|| anyhow!("{}: image bufferView {bv} out of range", src.name))?
            };
            image_table.push(self.intern_image(&bytes, img));
        }

        let mut tex_table: Vec<usize> = Vec::new();
        for mut t in as_arr(doc, "textures") {
            map_via(&mut t, "source", &image_table);
            map_via(&mut t, "sampler", &sampler_table);
            if let Some(exts) = t.get_mut("extensions").and_then(|e| e.as_object_mut()) {
                for (_, ext) in exts.iter_mut() {
                    map_via(ext, "source", &image_table);
                }
            }
            tex_table.push(self.intern_texture(t));
        }

        let mut mat_table: Vec<usize> = Vec::new();
        for mut m in as_arr(doc, "materials") {
            renumber_texture_infos(&mut m, &tex_table);
            mat_table.push(self.intern_material(m));
        }

        let mesh_base = self.meshes.len();
        for mut mesh in as_arr(doc, "meshes") {
            if let Some(prims) = mesh.get_mut("primitives").and_then(|p| p.as_array_mut()) {
                for prim in prims {
                    if let Some(attrs) = prim.get_mut("attributes").and_then(|a| a.as_object_mut())
                    {
                        for (_, v) in attrs.iter_mut() {
                            if let Some(n) = v.as_u64() {
                                *v = json!(n as usize + acc_base);
                            }
                        }
                    }
                    shift(prim, "indices", acc_base);
                    map_via(prim, "material", &mat_table);
                    if let Some(targets) = prim.get_mut("targets").and_then(|t| t.as_array_mut()) {
                        for t in targets {
                            if let Some(attrs) = t.as_object_mut() {
                                for (_, v) in attrs.iter_mut() {
                                    if let Some(n) = v.as_u64() {
                                        *v = json!(n as usize + acc_base);
                                    }
                                }
                            }
                        }
                    }
                    if let Some(draco) = prim
                        .get_mut("extensions")
                        .and_then(|e| e.get_mut("KHR_draco_mesh_compression"))
                    {
                        shift(draco, "bufferView", bv_base);
                    }
                }
            }
            self.meshes.push(mesh);
        }

        for key in ["extensionsUsed", "extensionsRequired"] {
            let dst = if key == "extensionsUsed" {
                &mut self.ext_used
            } else {
                &mut self.ext_required
            };
            for e in as_arr(doc, key) {
                if !dst.contains(&e) {
                    dst.push(e);
                }
            }
        }

        let node_tpl = as_arr(doc, "nodes");
        let skin_tpl = as_arr(doc, "skins");
        let scene_idx = doc.get("scene").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
        let scenes = as_arr(doc, "scenes");
        let scene_roots: Vec<usize> = scenes
            .get(scene_idx)
            .or_else(|| scenes.first())
            .and_then(|s| s.get("nodes"))
            .and_then(|n| n.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_u64())
                    .map(|x| x as usize)
                    .collect()
            })
            .unwrap_or_default();

        Ok(MergedSource {
            acc_base,
            mesh_base,
            node_tpl,
            skin_tpl,
            scene_roots,
        })
    }

    // one full node-array copy per placement: instances get their own hierarchy
    // (and skins, whose joints are node refs) while meshes/materials stay shared
    fn instantiate(&mut self, ms: &MergedSource, placement: &Placement, name: &str) -> usize {
        let node_base = self.nodes.len();
        let skin_base = self.skins.len();
        for tpl in &ms.node_tpl {
            let mut n = tpl.clone();
            if let Some(obj) = n.as_object_mut() {
                obj.remove("camera");
                if let Some(children) = obj.get_mut("children").and_then(|c| c.as_array_mut()) {
                    for c in children {
                        if let Some(v) = c.as_u64() {
                            *c = json!(v as usize + node_base);
                        }
                    }
                }
            }
            shift(&mut n, "mesh", ms.mesh_base);
            shift(&mut n, "skin", skin_base);
            self.nodes.push(n);
        }
        for tpl in &ms.skin_tpl {
            let mut s = tpl.clone();
            shift(&mut s, "inverseBindMatrices", ms.acc_base);
            shift(&mut s, "skeleton", node_base);
            if let Some(joints) = s.get_mut("joints").and_then(|j| j.as_array_mut()) {
                for j in joints {
                    if let Some(v) = j.as_u64() {
                        *j = json!(v as usize + node_base);
                    }
                }
            }
            self.skins.push(s);
        }
        let children: Vec<usize> = ms.scene_roots.iter().map(|r| r + node_base).collect();
        // placements are Unity-space; pre-invert the parser's x-flip conversion
        // so the wrapper round-trips to the placement transform exactly
        let p = placement;
        self.nodes.push(json!({
            "name": name,
            "translation": [-p.position[0], p.position[1], p.position[2]],
            "rotation": [p.rotation[0], -p.rotation[1], -p.rotation[2], p.rotation[3]],
            "scale": p.scale,
            "children": children,
        }));
        self.nodes.len() - 1
    }

    fn into_glb(mut self, root_name: &str) -> Vec<u8> {
        let root = json!({
            "name": root_name,
            "children": self.roots,
        });
        self.nodes.push(root);
        let root_idx = self.nodes.len() - 1;

        let mut doc = Map::new();
        doc.insert(
            "asset".into(),
            json!({"version": "2.0", "generator": "abgen-lod-rollup"}),
        );
        if !self.ext_used.is_empty() {
            doc.insert("extensionsUsed".into(), Value::Array(self.ext_used));
        }
        if !self.ext_required.is_empty() {
            doc.insert("extensionsRequired".into(), Value::Array(self.ext_required));
        }
        if !self.bin.is_empty() {
            doc.insert("buffers".into(), json!([{"byteLength": self.bin.len()}]));
        }
        for (key, arr) in [
            ("bufferViews", self.buffer_views),
            ("accessors", self.accessors),
            ("samplers", self.samplers),
            ("images", self.images),
            ("textures", self.textures),
            ("materials", self.materials),
            ("meshes", self.meshes),
            ("skins", self.skins),
            ("nodes", self.nodes),
        ] {
            if !arr.is_empty() {
                doc.insert(key.into(), Value::Array(arr));
            }
        }
        doc.insert("scene".into(), json!(0));
        doc.insert("scenes".into(), json!([{"nodes": [root_idx]}]));

        let mut jb = serde_json::to_vec(&Value::Object(doc)).expect("serialize rollup json");
        while !jb.len().is_multiple_of(4) {
            jb.push(b' ');
        }
        let mut bb = self.bin;
        while !bb.len().is_multiple_of(4) {
            bb.push(0);
        }
        let mut total = 12 + 8 + jb.len();
        if !bb.is_empty() {
            total += 8 + bb.len();
        }
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(b"glTF");
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&(total as u32).to_le_bytes());
        out.extend_from_slice(&(jb.len() as u32).to_le_bytes());
        out.extend_from_slice(b"JSON");
        out.extend_from_slice(&jb);
        if !bb.is_empty() {
            out.extend_from_slice(&(bb.len() as u32).to_le_bytes());
            out.extend_from_slice(&[0x42, 0x49, 0x4E, 0x00]);
            out.extend_from_slice(&bb);
        }
        out
    }
}

impl SourceDoc {
    fn view_bytes(&self, bv: usize) -> Option<Vec<u8>> {
        let v = self.json.get("bufferViews")?.get(bv)?;
        let bi = v.get("buffer").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
        let off = v.get("byteOffset").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
        let len = v.get("byteLength").and_then(|x| x.as_u64())? as usize;
        let buf = self.buffers.get(bi)?;
        buf.get(off..off + len).map(|s| s.to_vec())
    }
}

fn load_source(
    client: &crate::catalyst::CatalystClient,
    cache_dir: Option<&Path>,
    by_file: &HashMap<String, String>,
    hash: &str,
    src_name: &str,
) -> Result<SourceDoc> {
    let mut bytes = fetch_cached(client, cache_dir, hash)?;
    sanitize_glb_json_padding(&mut bytes);
    let is_gltf = src_name.to_lowercase().ends_with(".gltf");
    let (json_bytes, bin) = if is_gltf {
        (bytes, None)
    } else {
        glb_chunks(&bytes).with_context(|| format!("parse GLB container {src_name}"))?
    };
    let json: Value = serde_json::from_slice(&json_bytes)
        .with_context(|| format!("parse glTF JSON of {src_name}"))?;

    let resolve = |uri: &str| -> Result<Vec<u8>> {
        if let Some(b) = decode_data_uri(uri) {
            return Ok(b);
        }
        let key = crate::naming::resolve_uri_to_content_file(uri, src_name)?.to_lowercase();
        let h = by_file
            .get(&key)
            .ok_or_else(|| anyhow!("{src_name}: uri {uri:?} not in entity content map"))?;
        fetch_cached(client, cache_dir, h)
    };

    let mut buffers: Vec<Vec<u8>> = Vec::new();
    for (i, b) in as_arr(&json, "buffers").iter().enumerate() {
        match b.get("uri").and_then(|x| x.as_str()) {
            Some(uri) => buffers.push(resolve(uri).with_context(|| format!("buffer {i}"))?),
            None => buffers.push(bin.clone().unwrap_or_default()),
        }
    }

    // external images inlined up front so the rollup bundle is self-contained
    let mut doc = SourceDoc {
        json,
        buffers,
        name: src_name.to_string(),
    };
    let images = as_arr(&doc.json, "images");
    let mut inlined: Vec<Value> = Vec::with_capacity(images.len());
    for img in images {
        match img.get("uri").and_then(|x| x.as_str()) {
            Some(uri) if !uri.starts_with("data:") => {
                let bytes = resolve(uri).with_context(|| format!("image uri {uri:?}"))?;
                let at = {
                    let buf0 = doc
                        .buffers
                        .first_mut()
                        .ok_or_else(|| anyhow!("{src_name}: external image but no buffer"))?;
                    while !buf0.len().is_multiple_of(4) {
                        buf0.push(0);
                    }
                    let at = buf0.len();
                    buf0.extend_from_slice(&bytes);
                    at
                };
                let views = doc.json["bufferViews"]
                    .as_array_mut()
                    .ok_or_else(|| anyhow!("{src_name}: external image but no bufferViews"))?;
                views.push(json!({"buffer": 0, "byteOffset": at, "byteLength": bytes.len()}));
                let bv = views.len() - 1;
                let mut out = img.clone();
                let obj = out.as_object_mut().unwrap();
                obj.remove("uri");
                obj.insert("bufferView".into(), json!(bv));
                obj.insert("mimeType".into(), json!(image_mime(&bytes)));
                inlined.push(out);
            }
            _ => inlined.push(img),
        }
    }
    doc.json["images"] = Value::Array(inlined);
    if let Some(buf0) = doc.buffers.first() {
        if !doc.json.get("buffers").map(|b| b.is_null()).unwrap_or(true) {
            doc.json["buffers"][0]["byteLength"] = json!(buf0.len());
        }
    }
    Ok(doc)
}

pub fn rollup(
    client: &crate::catalyst::CatalystClient,
    scene: &crate::catalyst::Scene,
    placements: &[Placement],
    level: u32,
    cache_dir: Option<&Path>,
) -> Result<RollupOutcome> {
    if placements.is_empty() {
        bail!("rollup: no placements");
    }
    let by_file = scene.content_by_file();
    let mut file_by_hash: HashMap<&str, &str> = HashMap::new();
    for c in &scene.content {
        file_by_hash
            .entry(c.hash.as_str())
            .or_insert(c.file.as_str());
    }

    let mut log: Vec<String> = Vec::new();
    let mut placed: Vec<(usize, &Placement, String)> = Vec::with_capacity(placements.len());
    for (i, p) in placements.iter().enumerate() {
        match resolve_placement_hash(p, &by_file) {
            Ok(h) => placed.push((i, p, h)),
            Err(e) => log.push(format!("skipped unresolvable placement {i}: {e}")),
        }
    }
    if placed.is_empty() {
        bail!("rollup: all {} placement(s) unresolvable", placements.len());
    }

    let mut merged = Merged::default();
    let mut by_hash: HashMap<String, MergedSource> = HashMap::new();
    let mut errs: Vec<String> = Vec::new();
    for (_, _, hash) in &placed {
        if by_hash.contains_key(hash.as_str()) {
            continue;
        }
        let src_name = file_by_hash
            .get(hash.as_str())
            .map(|f| f.to_string())
            .unwrap_or_else(|| hash.clone());
        match load_source(client, cache_dir, &by_file, hash, &src_name)
            .and_then(|doc| merged.merge_source(&doc))
        {
            Ok(ms) => {
                by_hash.insert(hash.clone(), ms);
            }
            Err(e) => errs.push(format!("{hash} ({src_name}): {e:#}")),
        }
    }
    if !errs.is_empty() {
        bail!(
            "rollup: {} asset(s) failed to fetch/merge:\n{}",
            errs.len(),
            errs.join("\n")
        );
    }

    for &(pi, p, ref hash) in &placed {
        let ms = &by_hash[hash.as_str()];
        let idx = merged.instantiate(ms, p, &format!("Entity_{pi}"));
        merged.roots.push(idx);
    }

    let unique_glbs = by_hash.len();
    let instances = placed.len();
    log.push(format!(
        "rollup: instances={instances} unique_glbs={unique_glbs} nodes={} meshes={} materials={} images={} bin_bytes={}",
        merged.nodes.len(),
        merged.meshes.len(),
        merged.materials.len(),
        merged.images.len(),
        merged.bin.len()
    ));
    let root_name = format!("{}_{}", scene.entity_id.to_lowercase(), level);
    let glb = merged.into_glb(&root_name);
    Ok(RollupOutcome {
        glb,
        instances,
        unique_glbs,
        log,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lodgen::emit::emit_glb;
    use crate::lodgen::model::{AlphaClass, LodImage, LodMaterial, LodModel, LodPrimitive};

    fn dummy_client() -> crate::catalyst::CatalystClient {
        crate::catalyst::CatalystClient::new("http://127.0.0.1:9")
    }

    fn entity(content: &[(&str, &str)]) -> crate::catalyst::Scene {
        crate::catalyst::Scene {
            entity_id: "BafRollupTest".to_string(),
            entity_type: "scene".to_string(),
            pointers: Vec::new(),
            content: content
                .iter()
                .map(|(f, h)| crate::catalyst::ContentEntry {
                    file: f.to_string(),
                    hash: h.to_string(),
                })
                .collect(),
            metadata: serde_json::json!({}),
            timestamp: None,
        }
    }

    fn temp_cache(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "abgen-lod-rollup-test-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn tiny_png(seed: u8) -> Vec<u8> {
        let mut img = image::RgbaImage::new(2, 2);
        for (i, p) in img.pixels_mut().enumerate() {
            *p = image::Rgba([seed, i as u8 * 40, 255 - seed, 255]);
        }
        let mut cur = std::io::Cursor::new(Vec::new());
        img.write_to(&mut cur, image::ImageFormat::Png).unwrap();
        cur.into_inner()
    }

    fn textured_tri_glb(mat_name: &str, marker: f32, png: &[u8]) -> Vec<u8> {
        emit_glb(&LodModel {
            root_name: "t".to_string(),
            primitives: vec![LodPrimitive {
                positions: vec![[marker, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
                normals: vec![[0.0, 0.0, 1.0]; 3],
                uvs: vec![[0.25, 0.5], [1.0, 0.0], [0.0, 1.0]],
                indices: vec![0, 1, 2],
                material: 0,
                ..Default::default()
            }],
            materials: vec![LodMaterial {
                name: mat_name.to_string(),
                class: AlphaClass::Opaque,
                base_color: [1.0, 1.0, 1.0, 1.0],
                cutoff: 0.5,
                image: Some(0),
                double_sided: false,
            }],
            images: vec![LodImage {
                bytes: png.to_vec(),
                mime: "image/png".to_string(),
            }],
            log: Vec::new(),
        })
        .unwrap()
    }

    fn merged_json(glb: &[u8]) -> Value {
        let (json, _) = glb_chunks(glb).unwrap();
        serde_json::from_slice(&json).unwrap()
    }

    fn mk(hash: &str, position: [f64; 3]) -> Placement {
        Placement {
            glb_hash: Some(hash.to_string()),
            position,
            ..Default::default()
        }
    }

    #[test]
    fn rollup_shares_meshes_and_dedups_images_across_instances() {
        let png = tiny_png(9);
        let a = textured_tri_glb("matA", 0.0, &png);
        let b = textured_tri_glb("matB", 5.0, &png);
        let cache = temp_cache("dedupe");
        std::fs::write(cache.join("ha"), &a).unwrap();
        std::fs::write(cache.join("hb"), &b).unwrap();
        let ent = entity(&[("a.glb", "ha"), ("b.glb", "hb")]);
        let out = rollup(
            &dummy_client(),
            &ent,
            &[
                mk("ha", [1.0, 0.0, 0.0]),
                mk("ha", [2.0, 0.0, 0.0]),
                mk("hb", [3.0, 0.0, 0.0]),
            ],
            0,
            Some(&cache),
        )
        .unwrap();
        assert_eq!((out.instances, out.unique_glbs), (3, 2));
        let doc = merged_json(&out.glb);
        assert_eq!(
            doc["images"].as_array().unwrap().len(),
            1,
            "same png dedups"
        );
        assert_eq!(
            doc["meshes"].as_array().unwrap().len(),
            2,
            "meshes shared per source"
        );
        assert_eq!(doc["materials"].as_array().unwrap().len(), 2);
        // root -> 3 wrappers, each with its own copy of the source's 1 node
        let nodes = doc["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 3 + 3 + 1);
        let root = nodes.last().unwrap();
        assert_eq!(root["name"], "bafrolluptest_0");
        assert_eq!(root["children"].as_array().unwrap().len(), 3);
        assert_eq!(
            doc["scenes"][0]["nodes"],
            serde_json::json!([nodes.len() - 1])
        );
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn rollup_wrapper_preinverts_the_parsers_unity_conversion() {
        let png = tiny_png(3);
        let a = textured_tri_glb("m", 0.0, &png);
        let cache = temp_cache("frames");
        std::fs::write(cache.join("ha"), &a).unwrap();
        let ent = entity(&[("a.glb", "ha")]);
        let s2 = std::f64::consts::FRAC_1_SQRT_2;
        let p = Placement {
            glb_hash: Some("ha".to_string()),
            glb_file: None,
            position: [3.0, 4.0, 5.0],
            rotation: [0.0, s2, 0.0, s2],
            scale: [1.0, 2.0, 3.0],
        };
        let out = rollup(
            &dummy_client(),
            &ent,
            std::slice::from_ref(&p),
            0,
            Some(&cache),
        )
        .unwrap();
        let doc = merged_json(&out.glb);
        let wrapper = doc["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["name"] == "Entity_0")
            .unwrap();
        assert_eq!(wrapper["translation"], serde_json::json!([-3.0, 4.0, 5.0]));
        assert_eq!(wrapper["rotation"], serde_json::json!([0.0, -s2, -0.0, s2]));
        assert_eq!(wrapper["scale"], serde_json::json!([1.0, 2.0, 3.0]));
        // parse round-trips the wrapper back to the Unity placement transform
        let parsed = crate::gltf::parse(&out.glb, ".glb", None, false, true).unwrap();
        let node = parsed.nodes.iter().find(|n| n.name == "Entity_0").unwrap();
        assert_eq!(node.translation, [3.0, 4.0, 5.0]);
        assert!((node.rotation[1] - s2).abs() < 1e-6, "{:?}", node.rotation);
        assert_eq!(node.scale, [1.0, 2.0, 3.0]);
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn rollup_skips_unresolvable_and_fails_when_nothing_resolves() {
        let png = tiny_png(1);
        let a = textured_tri_glb("m", 0.0, &png);
        let cache = temp_cache("unres");
        std::fs::write(cache.join("ha"), &a).unwrap();
        let ent = entity(&[("a.glb", "ha")]);
        let out = rollup(
            &dummy_client(),
            &ent,
            &[
                Placement {
                    glb_file: Some("missing.glb".to_string()),
                    ..Default::default()
                },
                mk("ha", [0.0, 0.0, 0.0]),
            ],
            0,
            Some(&cache),
        )
        .unwrap();
        assert_eq!(out.instances, 1);
        assert!(out.log.iter().any(|l| l.contains("unresolvable")));

        let err = rollup(
            &dummy_client(),
            &ent,
            &[Placement {
                glb_file: Some("missing.glb".to_string()),
                ..Default::default()
            }],
            0,
            Some(&cache),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("unresolvable"));
        let _ = std::fs::remove_dir_all(&cache);
    }
}

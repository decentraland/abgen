#![cfg(not(target_arch = "wasm32"))]
//! Throwaway: where do a LOD bundle's bytes go? Per-file raw size, per-class
//! serialized bytes, per-Mesh vertex layout; LODSIZES_DUMP=dir writes each
//! mesh's vertex/index buffers as files for standalone compressibility checks.
use abgen::unity::bundle_file::{Bundle, FileContent};
use std::collections::BTreeMap;

fn main() {
    let dump = std::env::var("LODSIZES_DUMP").ok();
    for path in std::env::args().skip(1) {
        let data = std::fs::read(&path).expect("read");
        let bundle = Bundle::load_bytes(&data).expect("parse");
        let short = path.rsplit('/').next().unwrap().to_string();
        println!(
            "== {short} ({} bytes on disk, unity {})",
            data.len(),
            bundle.version_engine
        );
        let mut by_class: BTreeMap<i32, (usize, usize)> = BTreeMap::new();
        let mut uncompressed = 0usize;
        for f in &bundle.files {
            let FileContent::Serialized(sf) = &f.content else {
                if let FileContent::Raw(b) = &f.content {
                    uncompressed += b.len();
                    println!("  raw file {} = {} bytes", f.name, b.len());
                }
                continue;
            };
            let total: usize = sf.objects.iter().map(|o| o.data.len()).sum();
            uncompressed += total;
            println!(
                "  serialized file {} objects={} object-bytes={}",
                f.name,
                sf.objects.len(),
                total
            );
            for obj in &sf.objects {
                let e = by_class.entry(obj.class_id).or_default();
                e.0 += 1;
                e.1 += obj.data.len();
                if obj.class_id == 43 {
                    let v = sf.read_typetree(obj).expect("mesh typetree");
                    let name = v
                        .get("m_Name")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let vd = v.get("m_VertexData");
                    let vcount = vd
                        .and_then(|x| x.get("m_VertexCount"))
                        .and_then(|x| x.as_i64())
                        .unwrap_or(-1);
                    let vbytes = vd
                        .and_then(|x| x.get("m_DataSize"))
                        .and_then(|x| x.as_bytes());
                    let ib = v.get("m_IndexBuffer").and_then(|x| x.as_bytes());
                    let channels: Vec<String> = vd
                        .and_then(|x| x.get("m_Channels"))
                        .and_then(|x| x.as_array())
                        .map(|arr| {
                            arr.iter()
                                .enumerate()
                                .filter_map(|(i, c)| {
                                    let dim = c.get("dimension")?.as_i64()?;
                                    if dim == 0 {
                                        return None;
                                    }
                                    Some(format!(
                                        "ch{i}:dim{dim}/fmt{}/s{}",
                                        c.get("format")?.as_i64()?,
                                        c.get("stream")?.as_i64()?
                                    ))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    println!(
                        "    Mesh {name:?} obj={}B verts={vcount} vertexdata={}B indexfmt={} indexbuf={}B submeshes={} channels=[{}]",
                        obj.data.len(),
                        vbytes.map(|b| b.len()).unwrap_or(0),
                        v.get("m_IndexFormat").and_then(|x| x.as_i64()).unwrap_or(-1),
                        ib.map(|b| b.len()).unwrap_or(0),
                        v.get("m_SubMeshes").and_then(|x| x.as_array()).map(|a| a.len()).unwrap_or(0),
                        channels.join(" ")
                    );
                    if let Some(dir) = &dump {
                        std::fs::create_dir_all(dir).unwrap();
                        if let Some(b) = vbytes {
                            std::fs::write(format!("{dir}/{name}.vertex.bin"), b).unwrap();
                        }
                        if let Some(b) = ib {
                            std::fs::write(format!("{dir}/{name}.index.bin"), b).unwrap();
                        }
                    }
                }
                if obj.class_id == 4 {
                    let v = sf.read_typetree(obj).expect("transform typetree");
                    let g = |k: &str, c: &str| {
                        v.get(k)
                            .and_then(|x| x.get(c))
                            .and_then(|x| x.as_f64())
                            .unwrap_or(f64::NAN)
                    };
                    println!("    Transform pid={} pos=({:.4},{:.4},{:.4}) scale=({:.6},{:.6},{:.6}) father={}", obj.path_id, g("m_LocalPosition","x"), g("m_LocalPosition","y"), g("m_LocalPosition","z"), g("m_LocalScale","x"), g("m_LocalScale","y"), g("m_LocalScale","z"), v.get("m_Father").and_then(|f| f.get("m_PathID")).and_then(|x| x.as_i64()).unwrap_or(0));
                }
                if obj.class_id == 28 {
                    let v = sf.read_typetree(obj).expect("tex typetree");
                    let img = v
                        .get("image data")
                        .and_then(|x| x.as_bytes())
                        .unwrap_or(&[]);
                    if let Some(dir) = &dump {
                        let name = v.get("m_Name").and_then(|x| x.as_str()).unwrap_or("tex");
                        std::fs::write(format!("{dir}/{name}.image.bin"), img).unwrap();
                    }
                    let img = img.len();
                    println!(
                        "    Texture2D {:?} obj={}B imagedata={img}B",
                        v.get("m_Name").and_then(|x| x.as_str()).unwrap_or(""),
                        obj.data.len()
                    );
                }
            }
        }
        println!(
            "  by class: {}",
            by_class
                .iter()
                .map(|(c, (n, b))| format!("{c}x{n}={b}B"))
                .collect::<Vec<_>>()
                .join("  ")
        );
        println!(
            "  uncompressed total={uncompressed}B  on-disk={}B  ratio={:.2}",
            data.len(),
            data.len() as f64 / uncompressed.max(1) as f64
        );
    }
}

use anyhow::{anyhow, bail, Result};
use std::collections::HashMap;

pub const MAGIC: &[u8; 8] = b"DCLBVPK\0";
pub const VERSION: u32 = 1;
const ALIGN: u64 = 16;

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct PackIndex {
    pub v: u32,
    pub profile: String,
    pub entity: String,
    pub payload: u64,
    pub files: Vec<PackEntry>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct PackEntry {
    pub path: String,
    pub cid: String,
    pub off: u64,
    pub len: u64,
    pub kind: String,
    pub c: u8,
    pub sha256: String,
}

pub struct EntrySpec {
    pub path: String,
    pub cid: String,
    pub kind: &'static str,
    pub class: u8,
}

fn align_up(v: u64, a: u64) -> u64 {
    v.div_ceil(a) * a
}

pub struct PackPlan {
    pub index_json: Vec<u8>,
    pub payload_base: u64,
    pub blob_order: Vec<(String, u64, u64)>,
    pub total: u64,
}

struct BlobInfo<'a> {
    class: u8,
    len: u64,
    min_path: &'a str,
    sha: &'a str,
}

impl BlobInfo<'_> {
    fn key(&self) -> (u8, u64, &[u8]) {
        (self.class, self.len, self.min_path.as_bytes())
    }
}

pub fn plan_pack(
    entity: &str,
    entries: &[EntrySpec],
    meta_by_cid: &HashMap<String, (u64, String)>,
    max_bytes: u64,
) -> Result<PackPlan> {
    let mut blobs: HashMap<&str, BlobInfo> = HashMap::new();
    for e in entries {
        let (len, sha) = meta_by_cid
            .get(&e.cid)
            .ok_or_else(|| anyhow!("no blob for cid {}", e.cid))?;
        let b = blobs.entry(e.cid.as_str()).or_insert(BlobInfo {
            class: e.class,
            len: *len,
            min_path: &e.path,
            sha,
        });
        b.class = b.class.min(e.class);
        if e.path.as_bytes() < b.min_path.as_bytes() {
            b.min_path = &e.path;
        }
    }

    let mut blob_seq: Vec<&str> = blobs.keys().copied().collect();
    blob_seq.sort_by(|a, b| blobs[a].key().cmp(&blobs[b].key()));
    let mut layout: HashMap<&str, (u64, u64)> = HashMap::new();
    let mut blob_order: Vec<(String, u64, u64)> = Vec::new();
    let mut cursor: u64 = 0;
    for cid in &blob_seq {
        let len = blobs[cid].len;
        let off = align_up(cursor, ALIGN);
        layout.insert(cid, (off, len));
        blob_order.push((cid.to_string(), off, len));
        cursor = off + len;
    }
    let payload = cursor;

    let mut sorted: Vec<&EntrySpec> = entries.iter().collect();
    sorted.sort_by(|a, b| {
        let (ka, kb) = (blobs[a.cid.as_str()].key(), blobs[b.cid.as_str()].key());
        ka.cmp(&kb).then(a.path.as_bytes().cmp(b.path.as_bytes()))
    });
    let files: Vec<PackEntry> = sorted
        .iter()
        .map(|e| {
            let b = &blobs[e.cid.as_str()];
            let (off, len) = layout[e.cid.as_str()];
            PackEntry {
                path: e.path.clone(),
                cid: e.cid.clone(),
                off,
                len,
                kind: e.kind.to_string(),
                c: b.class,
                sha256: b.sha.to_string(),
            }
        })
        .collect();
    let index = PackIndex {
        v: VERSION,
        profile: super::BVW_PROFILE.to_string(),
        entity: entity.to_string(),
        payload,
        files,
    };
    let index_json = serde_json::to_vec(&index)?;

    let payload_base = align_up(8 + 4 + 4 + index_json.len() as u64, ALIGN);
    let total = payload_base + payload;
    if total > max_bytes {
        bail!("bvwebgpu pack for {entity} is {total} bytes, over the {max_bytes} cap");
    }
    Ok(PackPlan {
        index_json,
        payload_base,
        blob_order,
        total,
    })
}

fn pad_to<W: std::io::Write>(out: &mut W, cursor: &mut u64, target: u64) -> Result<()> {
    let zeros = [0u8; ALIGN as usize];
    while *cursor < target {
        let n = (target - *cursor).min(ALIGN) as usize;
        out.write_all(&zeros[..n])?;
        *cursor += n as u64;
    }
    Ok(())
}

pub fn write_pack<W: std::io::Write, R: std::io::Read>(
    plan: &PackPlan,
    mut open: impl FnMut(&str) -> Result<R>,
    out: &mut W,
) -> Result<()> {
    out.write_all(MAGIC)?;
    out.write_all(&VERSION.to_le_bytes())?;
    out.write_all(&(plan.index_json.len() as u32).to_le_bytes())?;
    out.write_all(&plan.index_json)?;
    let mut cursor = 8 + 4 + 4 + plan.index_json.len() as u64;
    pad_to(out, &mut cursor, plan.payload_base)?;
    for (cid, off, len) in &plan.blob_order {
        pad_to(out, &mut cursor, plan.payload_base + off)?;
        let mut r = open(cid)?;
        let copied = std::io::copy(&mut r, out)?;
        if copied != *len {
            bail!("blob {cid} yielded {copied} bytes, planned {len}");
        }
        cursor += copied;
    }
    debug_assert_eq!(cursor, plan.total);
    Ok(())
}

pub fn build_pack(
    entity: &str,
    entries: &[EntrySpec],
    blobs_by_cid: &HashMap<String, Vec<u8>>,
    max_bytes: u64,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let meta: HashMap<String, (u64, String)> = blobs_by_cid
        .iter()
        .map(|(c, b)| (c.clone(), (b.len() as u64, crate::hashes::sha256_hex(b))))
        .collect();
    let plan = plan_pack(entity, entries, &meta, max_bytes)?;
    let mut out = Vec::with_capacity(plan.total as usize);
    write_pack(
        &plan,
        |cid| Ok(std::io::Cursor::new(blobs_by_cid[cid].as_slice())),
        &mut out,
    )?;
    Ok((out, plan.index_json))
}

pub struct ParsedPack {
    pub index: PackIndex,
    pub payload_base: u64,
}

pub fn parse_pack(bytes: &[u8]) -> Result<ParsedPack> {
    if bytes.len() < 16 || &bytes[..8] != MAGIC {
        bail!("bad pack magic");
    }
    let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    if version != VERSION {
        bail!("unsupported pack version {version}");
    }
    let index_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as u64;
    if 16 + index_len > bytes.len() as u64 {
        bail!(
            "index length {index_len} overruns pack of {} bytes",
            bytes.len()
        );
    }
    let index: PackIndex = serde_json::from_slice(&bytes[16..16 + index_len as usize])?;
    let payload_base = align_up(16 + index_len, ALIGN);
    if payload_base + index.payload != bytes.len() as u64 {
        bail!(
            "payload {} at base {payload_base} inconsistent with pack of {} bytes",
            index.payload,
            bytes.len()
        );
    }
    let mut spans: Vec<(u64, u64)> = Vec::new();
    let mut by_cid: HashMap<&str, (u64, u64, u8)> = HashMap::new();
    let mut prev_key: Option<(u8, u64)> = None;
    for e in &index.files {
        if e.c > 3 {
            bail!("entry {} class {} out of range", e.path, e.c);
        }
        if e.off % ALIGN != 0 {
            bail!("entry {} offset {} not {ALIGN}-aligned", e.path, e.off);
        }
        if e.off + e.len > index.payload {
            bail!("entry {} overruns payload", e.path);
        }
        match by_cid.get(e.cid.as_str()) {
            Some(&(off, len, c)) => {
                if (off, len, c) != (e.off, e.len, e.c) {
                    bail!("cid {} has divergent spans", e.cid);
                }
            }
            None => {
                by_cid.insert(&e.cid, (e.off, e.len, e.c));
                spans.push((e.off, e.len));
                if let Some(k) = prev_key {
                    if k > (e.c, e.len) {
                        bail!("index not in demand order at {}", e.path);
                    }
                }
                prev_key = Some((e.c, e.len));
            }
        }
    }
    spans.sort_unstable();
    for w in spans.windows(2) {
        if w[0].0 + w[0].1 > w[1].0 {
            bail!("overlapping blob spans");
        }
    }
    Ok(ParsedPack {
        index,
        payload_base,
    })
}

pub fn entry_slice<'a>(bytes: &'a [u8], pack: &ParsedPack, e: &PackEntry) -> &'a [u8] {
    let start = (pack.payload_base + e.off) as usize;
    &bytes[start..start + e.len as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(path: &str, cid: &str, kind: &'static str) -> EntrySpec {
        EntrySpec {
            path: path.to_string(),
            cid: cid.to_string(),
            kind,
            class: crate::bvwebgpu::class_for(path),
        }
    }

    fn blobs(pairs: &[(&str, &[u8])]) -> HashMap<String, Vec<u8>> {
        pairs
            .iter()
            .map(|(c, b)| (c.to_string(), b.to_vec()))
            .collect()
    }

    #[test]
    fn build_and_parse_round_trip_with_dedup() {
        let entries = vec![
            spec("models/z.glb", "cid1", "glb"),
            spec("a.png", "cid2", "img"),
            spec("copy/z.glb", "cid1", "glb"),
            spec("main.crdt", "cid3", "raw"),
        ];
        let b = blobs(&[
            ("cid1", b"GLBBYTESGLBBYTESGLB"),
            ("cid2", b"DDS"),
            ("cid3", b"c"),
        ]);
        let (pack, index_json) = build_pack("bafkent", &entries, &b, u64::MAX).unwrap();
        let parsed = parse_pack(&pack).unwrap();
        assert_eq!(parsed.index.v, 1);
        assert_eq!(parsed.index.profile, "bv4");
        assert_eq!(parsed.index.entity, "bafkent");
        let paths: Vec<&str> = parsed.index.files.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["main.crdt", "copy/z.glb", "models/z.glb", "a.png"]
        );
        let by_path: HashMap<&str, &PackEntry> = parsed
            .index
            .files
            .iter()
            .map(|e| (e.path.as_str(), e))
            .collect();
        let e1 = by_path["copy/z.glb"];
        let e2 = by_path["models/z.glb"];
        assert_eq!((e1.off, e1.len, e1.c), (e2.off, e2.len, e2.c));
        assert_eq!(e1.sha256, e2.sha256);
        assert_eq!(by_path["main.crdt"].c, 0);
        assert_eq!(by_path["models/z.glb"].c, 1);
        assert_eq!(by_path["a.png"].c, 2);
        for e in &parsed.index.files {
            assert_eq!(e.off % 16, 0);
            let got = entry_slice(&pack, &parsed, e);
            assert_eq!(got, b[&e.cid].as_slice());
            assert_eq!(e.sha256, crate::hashes::sha256_hex(got));
        }
        assert!(index_json.starts_with(b"{\"v\":1,\"profile\":\"bv4\",\"entity\":\"bafkent\","));
        let again = build_pack("bafkent", &entries, &b, u64::MAX).unwrap().0;
        assert_eq!(pack, again);
    }

    #[test]
    fn cap_is_enforced() {
        let entries = vec![spec("a.bin", "cid1", "raw")];
        let b = blobs(&[("cid1", &[0u8; 4096][..])]);
        let err = build_pack("bafkent", &entries, &b, 128).unwrap_err();
        assert!(err.to_string().contains("over the 128"), "{err}");
        assert!(build_pack("bafkent", &entries, &b, 1 << 20).is_ok());
    }

    #[test]
    fn parse_rejects_corruption() {
        let entries = vec![spec("a.bin", "cid1", "raw"), spec("b.bin", "cid2", "raw")];
        let b = blobs(&[("cid1", b"AAAA"), ("cid2", b"BB")]);
        let (pack, _) = build_pack("bafkent", &entries, &b, u64::MAX).unwrap();

        assert!(parse_pack(&pack[..12]).is_err());
        let mut bad_magic = pack.clone();
        bad_magic[0] = b'X';
        assert!(parse_pack(&bad_magic).is_err());
        let mut bad_version = pack.clone();
        bad_version[8] = 9;
        assert!(parse_pack(&bad_version).is_err());
        let mut bad_index_len = pack.clone();
        bad_index_len[12..16].copy_from_slice(&(u32::MAX).to_le_bytes());
        assert!(parse_pack(&bad_index_len).is_err());
        assert!(parse_pack(&pack[..pack.len() - 1]).is_err());

        let mangle = |f: &dyn Fn(&mut PackIndex)| {
            let parsed = parse_pack(&pack).unwrap();
            let mut idx = parsed.index.clone();
            f(&mut idx);
            let ij = serde_json::to_vec(&idx).unwrap();
            let base = ((16 + ij.len() as u64).div_ceil(16)) * 16;
            let mut out = Vec::new();
            out.extend_from_slice(MAGIC);
            out.extend_from_slice(&VERSION.to_le_bytes());
            out.extend_from_slice(&(ij.len() as u32).to_le_bytes());
            out.extend_from_slice(&ij);
            out.resize(base as usize, 0);
            out.extend_from_slice(&pack[parsed.payload_base as usize..]);
            parse_pack(&out)
        };
        assert!(mangle(&|i| i.files[0].off = 8).is_err());
        assert!(mangle(&|i| i.files[0].len = i.payload + 1).is_err());
        assert!(mangle(&|i| {
            i.files[0].off = i.files[1].off;
            i.files[0].len = 2;
        })
        .is_err());
        assert!(mangle(&|i| i.files.swap(0, 1)).is_err());
        assert!(mangle(&|i| {
            i.files[1].cid = i.files[0].cid.clone();
            i.files[1].len = 1;
        })
        .is_err());
        assert!(mangle(&|i| i.files[0].c = 9).is_err());
        assert!(mangle(&|i| {
            i.files[1] = i.files[0].clone();
            i.files[1].c = 2;
        })
        .is_err());
        assert!(mangle(&|_| {}).is_ok());
    }

    #[test]
    fn bv3_style_index_is_rejected() {
        let entries = vec![spec("b.png", "cid1", "img"), spec("a.crdt", "cid2", "raw")];
        let b = blobs(&[("cid1", b"PNGBYTES"), ("cid2", b"CC")]);
        let (pack, _) = build_pack("bafkent", &entries, &b, u64::MAX).unwrap();
        let parsed = parse_pack(&pack).unwrap();
        let mut idx = serde_json::to_value(&parsed.index).unwrap();
        let files = idx["files"].as_array_mut().unwrap();
        files.sort_by_key(|f| f["path"].as_str().unwrap().to_string());
        for f in files.iter_mut() {
            f.as_object_mut().unwrap().remove("c");
        }
        let ij = serde_json::to_vec(&idx).unwrap();
        let base = (16 + ij.len() as u64).div_ceil(16) * 16;
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&(ij.len() as u32).to_le_bytes());
        out.extend_from_slice(&ij);
        out.resize(base as usize, 0);
        out.extend_from_slice(&pack[parsed.payload_base as usize..]);
        let err = match parse_pack(&out) {
            Err(e) => e,
            Ok(_) => panic!("bv3-style index must be rejected"),
        };
        assert!(err.to_string().contains("missing field `c`"), "{err}");
    }

    #[test]
    fn index_field_order_is_pinned() {
        let entries = vec![spec("main.crdt", "cid1", "raw")];
        let b = blobs(&[("cid1", b"X")]);
        let (_, index_json) = build_pack("bafkent", &entries, &b, u64::MAX).unwrap();
        let s = String::from_utf8(index_json).unwrap();
        assert!(
            s.contains(
                "{\"path\":\"main.crdt\",\"cid\":\"cid1\",\"off\":0,\"len\":1,\"kind\":\"raw\",\"c\":0,\"sha256\":\""
            ),
            "{s}"
        );
    }
}

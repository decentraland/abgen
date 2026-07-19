use crate::lodgen::placements::{placements_from_components, ManifestPlacements, Trs};
use std::collections::{HashMap, HashSet};

pub const PUT_COMPONENT: u32 = 1;

pub const TRANSFORM: u32 = 1;
pub const MATERIAL: u32 = 1017;
pub const MESH_RENDERER: u32 = 1018;
pub const GLTF_CONTAINER: u32 = 1041;
pub const UI_CANVAS_INFORMATION: u32 = 1054;
pub const CAMERA_MODE: u32 = 1072;
pub const VISIBILITY: u32 = 1081;

const TRACKED: [u32; 5] = [
    TRANSFORM,
    MATERIAL,
    MESH_RENDERER,
    GLTF_CONTAINER,
    VISIBILITY,
];

const HEADER_LEN: usize = 8;

pub const TRANSFORM_DEFAULT_PAYLOAD: &[u8] =
    include_bytes!("../testdata/getstate_transform_default.bin");
pub const UI_CANVAS_INFO_PAYLOAD: &[u8] = include_bytes!("../testdata/getstate_ui_canvas_info.bin");
pub const CAMERA_MODE_PAYLOAD: &[u8] = include_bytes!("../testdata/getstate_camera_mode.bin");

struct Put<'a> {
    entity: u32,
    component: u32,
    timestamp: u32,
    data: &'a [u8],
}

fn read_u32(buf: &[u8], off: usize) -> Option<u32> {
    let bytes = buf.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes(bytes.try_into().unwrap()))
}

struct Messages<'a> {
    buf: &'a [u8],
    off: usize,
}

impl<'a> Iterator for Messages<'a> {
    type Item = Put<'a>;

    fn next(&mut self) -> Option<Put<'a>> {
        loop {
            let len = read_u32(self.buf, self.off)? as usize;
            let ty = read_u32(self.buf, self.off + 4)?;
            let end = self.off.checked_add(len)?;
            if len < HEADER_LEN || end > self.buf.len() {
                return None;
            }
            let body = &self.buf[self.off + HEADER_LEN..end];
            self.off = end;
            if ty != PUT_COMPONENT || body.len() < 16 {
                continue;
            }
            let data_len = read_u32(body, 12).unwrap() as usize;
            let Some(data) = body.get(16..16 + data_len) else {
                continue;
            };
            return Some(Put {
                entity: read_u32(body, 0).unwrap(),
                component: read_u32(body, 4).unwrap(),
                timestamp: read_u32(body, 8).unwrap(),
                data,
            });
        }
    }
}

#[derive(Default)]
pub struct LwwState {
    cells: HashMap<(u32, u32), (u32, u64, Vec<u8>)>,
    next_seq: u64,
}

impl LwwState {
    pub fn ingest(&mut self, bytes: &[u8]) {
        for put in (Messages { buf: bytes, off: 0 }) {
            if !TRACKED.contains(&put.component) {
                continue;
            }
            use std::collections::hash_map::Entry;
            match self.cells.entry((put.entity, put.component)) {
                Entry::Occupied(mut e) => {
                    if put.timestamp >= e.get().0 {
                        let seq = e.get().1;
                        *e.get_mut() = (put.timestamp, seq, put.data.to_vec());
                    }
                }
                Entry::Vacant(v) => {
                    v.insert((put.timestamp, self.next_seq, put.data.to_vec()));
                    self.next_seq += 1;
                }
            }
        }
    }

    pub fn project(&self, content_by_file: &HashMap<String, String>) -> ManifestPlacements {
        let mut ordered: Vec<(&(u32, u32), &(u32, u64, Vec<u8>))> = self.cells.iter().collect();
        ordered.sort_by_key(|(_, (_, seq, _))| *seq);
        let mut transforms: HashMap<i64, Trs> = HashMap::new();
        let mut gltf_srcs: Vec<(i64, String)> = Vec::new();
        let mut mesh_renderer_entities: HashSet<i64> = HashSet::new();
        for ((entity, component), (_, _, data)) in ordered {
            let eid = i64::from(*entity);
            match *component {
                TRANSFORM => {
                    if let Some(t) = decode_transform(data) {
                        transforms.insert(eid, t);
                    }
                }
                GLTF_CONTAINER => {
                    if data.is_empty() {
                        continue;
                    }
                    if let Some(src) = gltf_src(data) {
                        gltf_srcs.push((eid, src));
                    }
                }
                MESH_RENDERER => {
                    mesh_renderer_entities.insert(eid);
                }
                _ => {}
            }
        }
        placements_from_components(
            transforms,
            gltf_srcs,
            mesh_renderer_entities,
            content_by_file,
        )
    }
}

fn decode_transform(data: &[u8]) -> Option<Trs> {
    if data.is_empty() {
        return Some(Trs::default());
    }
    if data.len() != 44 {
        return None;
    }
    let f = |i: usize| f32::from_le_bytes(data[i * 4..i * 4 + 4].try_into().unwrap()) as f64;
    Some(Trs {
        position: [f(0), f(1), f(2)],
        rotation: [f(3), f(4), f(5), f(6)],
        scale: [f(7), f(8), f(9)],
        parent: i64::from(u32::from_le_bytes(data[40..44].try_into().unwrap())),
    })
}

fn read_varint(data: &[u8], mut off: usize) -> Option<(u64, usize)> {
    let mut val = 0u64;
    let mut shift = 0u32;
    loop {
        let b = *data.get(off)?;
        off += 1;
        val |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some((val, off));
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}

fn gltf_src(data: &[u8]) -> Option<String> {
    let mut off = 0usize;
    let mut src: Option<String> = None;
    while off < data.len() {
        let (tag, next) = read_varint(data, off)?;
        off = next;
        let field = tag >> 3;
        match tag & 7 {
            0 => {
                let (_, next) = read_varint(data, off)?;
                off = next;
            }
            1 => {
                off = off.checked_add(8)?;
                if off > data.len() {
                    return None;
                }
            }
            2 => {
                let (len, next) = read_varint(data, off)?;
                off = next;
                let end = off.checked_add(usize::try_from(len).ok()?)?;
                if end > data.len() {
                    return None;
                }
                if field == 1 {
                    src = Some(String::from_utf8_lossy(&data[off..end]).into_owned());
                }
                off = end;
            }
            5 => {
                off = off.checked_add(4)?;
                if off > data.len() {
                    return None;
                }
            }
            _ => return None,
        }
    }
    Some(src.unwrap_or_default())
}

fn encode_put(out: &mut Vec<u8>, entity: u32, component: u32, timestamp: u32, data: &[u8]) {
    out.extend_from_slice(&((HEADER_LEN + 16 + data.len()) as u32).to_le_bytes());
    out.extend_from_slice(&PUT_COMPONENT.to_le_bytes());
    out.extend_from_slice(&entity.to_le_bytes());
    out.extend_from_slice(&component.to_le_bytes());
    out.extend_from_slice(&timestamp.to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
}

pub fn synthetic_initial_state(main_crdt: Option<&[u8]>) -> Vec<u8> {
    let mut out = Vec::new();
    encode_put(&mut out, 1, TRANSFORM, 1, TRANSFORM_DEFAULT_PAYLOAD);
    encode_put(&mut out, 2, TRANSFORM, 1, TRANSFORM_DEFAULT_PAYLOAD);
    encode_put(
        &mut out,
        0,
        UI_CANVAS_INFORMATION,
        1,
        UI_CANVAS_INFO_PAYLOAD,
    );
    encode_put(&mut out, 2, CAMERA_MODE, 1, CAMERA_MODE_PAYLOAD);
    if let Some(bytes) = main_crdt {
        out.extend_from_slice(bytes);
    }
    out
}

pub fn placements_from_crdt(
    stream: &[u8],
    content_by_file: &HashMap<String, String>,
) -> ManifestPlacements {
    let mut state = LwwState::default();
    state.ingest(stream);
    state.project(content_by_file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lodgen::placements::parse_lod_manifest_full;

    fn transform_bytes(p: [f32; 3], r: [f32; 4], s: [f32; 3], parent: u32) -> Vec<u8> {
        let mut out = Vec::new();
        for v in p.into_iter().chain(r).chain(s) {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&parent.to_le_bytes());
        out
    }

    fn gltf_bytes(src: &str) -> Vec<u8> {
        let mut out = vec![0x0a, src.len() as u8];
        out.extend_from_slice(src.as_bytes());
        out
    }

    #[test]
    fn codec_round_trips() {
        let mut stream = Vec::new();
        encode_put(&mut stream, 513, TRANSFORM, 7, &[0xde, 0xad, 0xbe, 0xef]);
        encode_put(&mut stream, 0, GLTF_CONTAINER, 9, &[]);
        let puts: Vec<Put> = Messages {
            buf: &stream,
            off: 0,
        }
        .collect();
        assert_eq!(puts.len(), 2);
        assert_eq!(puts[0].entity, 513);
        assert_eq!(puts[0].component, TRANSFORM);
        assert_eq!(puts[0].timestamp, 7);
        assert_eq!(puts[0].data, &[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(puts[1].entity, 0);
        assert_eq!(puts[1].component, GLTF_CONTAINER);
        assert_eq!(puts[1].timestamp, 9);
        assert!(puts[1].data.is_empty());
    }

    #[test]
    fn lww_tie_later_in_stream_wins() {
        let mut stream = Vec::new();
        encode_put(&mut stream, 5, TRANSFORM, 3, b"first");
        encode_put(&mut stream, 5, TRANSFORM, 3, b"second");
        encode_put(&mut stream, 5, TRANSFORM, 2, b"stale");
        let mut state = LwwState::default();
        state.ingest(&stream);
        let cell = state.cells.get(&(5, TRANSFORM)).unwrap();
        assert_eq!(cell.0, 3);
        assert_eq!(cell.2, b"second");
        assert_eq!(state.cells.len(), 1);
    }

    #[test]
    fn unknown_types_are_length_skipped() {
        let mut stream = Vec::new();
        encode_put(&mut stream, 1, TRANSFORM, 1, b"a");
        stream.extend_from_slice(&12u32.to_le_bytes());
        stream.extend_from_slice(&3u32.to_le_bytes());
        stream.extend_from_slice(&1u32.to_le_bytes());
        stream.extend_from_slice(&20u32.to_le_bytes());
        stream.extend_from_slice(&6u32.to_le_bytes());
        stream.extend_from_slice(&[0u8; 12]);
        encode_put(&mut stream, 2, TRANSFORM, 1, b"b");
        let puts: Vec<Put> = Messages {
            buf: &stream,
            off: 0,
        }
        .collect();
        assert_eq!(puts.len(), 2);
        assert_eq!(puts[0].data, b"a");
        assert_eq!(puts[1].data, b"b");
    }

    #[test]
    fn truncated_tail_is_tolerated() {
        let mut stream = Vec::new();
        encode_put(&mut stream, 1, TRANSFORM, 1, b"ok");
        let full = stream.clone();
        stream.extend_from_slice(&[0x05, 0x00]);
        let puts: Vec<Put> = Messages {
            buf: &stream,
            off: 0,
        }
        .collect();
        assert_eq!(puts.len(), 1);
        assert_eq!(puts[0].data, b"ok");
        let mut oversized = full.clone();
        oversized.extend_from_slice(&999u32.to_le_bytes());
        oversized.extend_from_slice(&1u32.to_le_bytes());
        let puts: Vec<Put> = Messages {
            buf: &oversized,
            off: 0,
        }
        .collect();
        assert_eq!(puts.len(), 1);
        let mut undersized = full;
        undersized.extend_from_slice(&4u32.to_le_bytes());
        undersized.extend_from_slice(&1u32.to_le_bytes());
        let puts: Vec<Put> = Messages {
            buf: &undersized,
            off: 0,
        }
        .collect();
        assert_eq!(puts.len(), 1);
    }

    #[test]
    fn transform_decodes_44_bytes() {
        let s2 = std::f32::consts::FRAC_1_SQRT_2;
        let bytes = transform_bytes(
            [1.5, -2.0, 3.25],
            [0.0, s2, 0.0, s2],
            [16.0, 1.0, 16.0],
            512,
        );
        let t = decode_transform(&bytes).unwrap();
        assert_eq!(t.position, [1.5, -2.0, 3.25]);
        assert_eq!(t.rotation, [0.0, s2 as f64, 0.0, s2 as f64]);
        assert_eq!(t.scale, [16.0, 1.0, 16.0]);
        assert_eq!(t.parent, 512);
        let d = decode_transform(&[]).unwrap();
        assert_eq!(d.position, [0.0; 3]);
        assert_eq!(d.rotation, [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(d.scale, [1.0; 3]);
        assert_eq!(d.parent, 0);
        assert!(decode_transform(&[0u8; 43]).is_none());
        assert!(decode_transform(&[0u8; 45]).is_none());
        let fixture = decode_transform(TRANSFORM_DEFAULT_PAYLOAD).unwrap();
        assert_eq!(fixture.position, d.position);
        assert_eq!(fixture.rotation, d.rotation);
        assert_eq!(fixture.scale, d.scale);
        assert_eq!(fixture.parent, 0);
    }

    #[test]
    fn gltf_src_skips_unknown_fields() {
        let mut data = Vec::new();
        data.extend_from_slice(&[0x10, 0xac, 0x02]);
        data.extend_from_slice(&[0x1d, 1, 2, 3, 4]);
        data.extend_from_slice(&[0x21, 1, 2, 3, 4, 5, 6, 7, 8]);
        data.extend_from_slice(&[0x2a, 0x03, b'x', b'y', b'z']);
        data.extend_from_slice(&gltf_bytes("models/scene.glb"));
        assert_eq!(gltf_src(&data).as_deref(), Some("models/scene.glb"));
        assert_eq!(gltf_src(&[0x10, 0x02]).as_deref(), Some(""));
        assert!(gltf_src(&[0x0a, 0x10, b'x']).is_none());
        assert!(gltf_src(&[0x0f]).is_none());
    }

    #[test]
    fn synthetic_initial_state_matches_fixture_frames() {
        assert_eq!(TRANSFORM_DEFAULT_PAYLOAD.len(), 44);
        assert!(UI_CANVAS_INFO_PAYLOAD.is_empty());
        assert!(CAMERA_MODE_PAYLOAD.is_empty());
        let mut want = Vec::new();
        for (entity, component, payload) in [
            (1u32, TRANSFORM, TRANSFORM_DEFAULT_PAYLOAD),
            (2, TRANSFORM, TRANSFORM_DEFAULT_PAYLOAD),
            (0, UI_CANVAS_INFORMATION, UI_CANVAS_INFO_PAYLOAD),
            (2, CAMERA_MODE, CAMERA_MODE_PAYLOAD),
        ] {
            want.extend_from_slice(&((24 + payload.len()) as u32).to_le_bytes());
            want.extend_from_slice(&1u32.to_le_bytes());
            want.extend_from_slice(&entity.to_le_bytes());
            want.extend_from_slice(&component.to_le_bytes());
            want.extend_from_slice(&1u32.to_le_bytes());
            want.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            want.extend_from_slice(payload);
        }
        assert_eq!(synthetic_initial_state(None), want);
        let tail = [9u8, 8, 7];
        let with = synthetic_initial_state(Some(&tail));
        assert_eq!(&with[..want.len()], &want[..]);
        assert_eq!(&with[want.len()..], &tail);
    }

    #[test]
    fn crdt_stream_matches_manifest_parse() {
        let s2 = std::f32::consts::FRAC_1_SQRT_2;
        let mut stream = synthetic_initial_state(None);
        encode_put(
            &mut stream,
            600,
            TRANSFORM,
            1,
            &transform_bytes([10.0, 0.0, 0.0], [0.0, s2, 0.0, s2], [2.0, 2.0, 2.0], 0),
        );
        encode_put(
            &mut stream,
            601,
            TRANSFORM,
            1,
            &transform_bytes([1.0, 0.0, 0.0], [0.0, 0.0, 0.0, 1.0], [1.0, 1.0, 1.0], 600),
        );
        encode_put(
            &mut stream,
            601,
            GLTF_CONTAINER,
            1,
            &gltf_bytes("stale.glb"),
        );
        encode_put(
            &mut stream,
            601,
            GLTF_CONTAINER,
            1,
            &gltf_bytes("Models/Child.GLB"),
        );
        encode_put(&mut stream, 700, MESH_RENDERER, 1, &[]);
        encode_put(
            &mut stream,
            701,
            GLTF_CONTAINER,
            2,
            &gltf_bytes("models/missing.glb"),
        );
        encode_put(&mut stream, 800, MATERIAL, 1, &[0x08, 0x01]);
        encode_put(&mut stream, 800, VISIBILITY, 1, &[]);
        let mut content = HashMap::new();
        content.insert("models/child.glb".to_string(), "hchild".to_string());
        let got = placements_from_crdt(&stream, &content);
        let s2d = s2 as f64;
        let manifest = serde_json::json!([
            {
                "entityId": 1,
                "componentName": "core::Transform",
                "data": {
                    "position": {"x": 0.0, "y": 0.0, "z": 0.0},
                    "rotation": {"x": 0.0, "y": 0.0, "z": 0.0, "w": 1.0},
                    "scale": {"x": 1.0, "y": 1.0, "z": 1.0},
                    "parent": 0
                }
            },
            {
                "entityId": 2,
                "componentName": "core::Transform",
                "data": {
                    "position": {"x": 0.0, "y": 0.0, "z": 0.0},
                    "rotation": {"x": 0.0, "y": 0.0, "z": 0.0, "w": 1.0},
                    "scale": {"x": 1.0, "y": 1.0, "z": 1.0},
                    "parent": 0
                }
            },
            {
                "entityId": 600,
                "componentName": "core::Transform",
                "data": {
                    "position": {"x": 10.0, "y": 0.0, "z": 0.0},
                    "rotation": {"x": 0.0, "y": s2d, "z": 0.0, "w": s2d},
                    "scale": {"x": 2.0, "y": 2.0, "z": 2.0},
                    "parent": 0
                }
            },
            {
                "entityId": 601,
                "componentName": "core::Transform",
                "data": {
                    "position": {"x": 1.0, "y": 0.0, "z": 0.0},
                    "rotation": {"x": 0.0, "y": 0.0, "z": 0.0, "w": 1.0},
                    "scale": {"x": 1.0, "y": 1.0, "z": 1.0},
                    "parent": 600
                }
            },
            {
                "entityId": 601,
                "componentName": "core::GltfContainer",
                "data": {"src": "Models/Child.GLB"}
            },
            {
                "entityId": 700,
                "componentName": "core::MeshRenderer",
                "data": null
            },
            {
                "entityId": 701,
                "componentName": "core::GltfContainer",
                "data": {"src": "models/missing.glb"}
            }
        ]);
        let want =
            parse_lod_manifest_full(&serde_json::to_vec(&manifest).unwrap(), &content).unwrap();
        assert_eq!(got, want);
        assert_eq!(got.placements.len(), 2);
        assert_eq!(got.skipped_mesh_renderer, 1);
        assert_eq!(got.unresolved_src, 1);
        let child = got
            .placements
            .iter()
            .find(|p| p.glb_hash.as_deref() == Some("hchild"))
            .unwrap();
        assert_eq!(child.glb_file.as_deref(), Some("Models/Child.GLB"));
        assert_eq!(child.scale, [2.0, 2.0, 2.0]);
    }
}

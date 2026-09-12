//! SDK7 `core::MeshRenderer` primitives — box / sphere / plane / cylinder — and the
//! `core::Material` that colours them.
//!
//! Production's `ManifestParser` only reads `GltfContainer`, so every scene built from
//! primitives is missing from the Unity LODs. Here the MeshRenderer and Material rows
//! (JSON manifest or raw CRDT protobuf) are decoded into a [`PrimitiveSpec`] and a
//! [`PrimitiveMaterial`], placed by the entity's world transform exactly like a GLB, and
//! turned into geometry in `assemble.rs`.
//!
//! Geometry is a port of unity-explorer's `Utility.Primitives` factories (BoxFactory,
//! PlaneFactory, SphereFactory, CylinderVariantsFactory), vertex order included: the
//! scene-provided `uvs` arrays index those vertices, so the default UV sets and the
//! custom-UV assignment (sequential pairs, `PrimitivesUtility.FloatArrayToV2List`) are
//! reproduced verbatim. The meshes are built in Unity space (left-handed, UV origin
//! bottom-left, Unity winding) — the same frame `crate::gltf::parse` delivers a glTFast-
//! imported GLB in — so `assemble.rs` applies one export conversion to both.
//!
//! Materials follow the explorer's `PBMaterialExtensions` defaults and
//! `MaterialTransparencyMode.ResolveAutoMode`. Only base colour + texture survive, like
//! every other LOD material; emissive/bump/alpha maps are never fetched. Video and
//! avatar textures have no static content and fall back to the colour.

use std::collections::HashMap;

use super::model::AlphaClass;

// ── wire shapes ──────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PrimitiveShape {
    Box,
    Sphere,
    Plane,
    Cylinder,
}

impl PrimitiveShape {
    pub fn name(&self) -> &'static str {
        match self {
            PrimitiveShape::Box => "box",
            PrimitiveShape::Sphere => "sphere",
            PrimitiveShape::Plane => "plane",
            PrimitiveShape::Cylinder => "cylinder",
        }
    }
}

/// The `PBMeshRenderer` oneof, reduced to what the geometry needs.
#[derive(Clone, Debug, PartialEq)]
pub struct PrimitiveSpec {
    pub shape: PrimitiveShape,
    /// Custom texture coordinates as flat `(u, v)` pairs (box: up to 48 pairs, plane: up
    /// to 8). Empty means the factory defaults. Ignored for sphere and cylinder.
    pub uvs: Vec<f32>,
    /// Cylinder only; `None` means the explorer default of 0.5. A zero radius makes a cone.
    pub radius_top: Option<f32>,
    pub radius_bottom: Option<f32>,
}

impl PrimitiveSpec {
    pub fn simple(shape: PrimitiveShape) -> Self {
        PrimitiveSpec {
            shape,
            uvs: Vec::new(),
            radius_top: None,
            radius_bottom: None,
        }
    }

    /// Stable identity for geometry sharing between placements.
    pub fn cache_key(&self) -> String {
        format!(
            "{}|{:?}|{:?}|{:?}",
            self.shape.name(),
            self.radius_top,
            self.radius_bottom,
            self.uvs
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WrapMode {
    Repeat,
    Clamp,
    Mirror,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TextureSource {
    /// Content hash from the entity's own content map.
    Hash(String),
    /// Absolute `http(s)` URL as written in the scene.
    Url(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct PrimitiveTexture {
    pub source: TextureSource,
    pub wrap: WrapMode,
}

/// A `PBMaterial` reduced to the LOD's base-colour model.
#[derive(Clone, Debug, PartialEq)]
pub struct PrimitiveMaterial {
    /// Linear RGBA exactly as the scene wrote it (the explorer uploads it with
    /// `Material.SetColor`, unconverted, and glTF's `baseColorFactor` is linear too).
    pub color: [f64; 4],
    pub class: AlphaClass,
    pub cutoff: f64,
    pub texture: Option<PrimitiveTexture>,
}

impl Default for PrimitiveMaterial {
    fn default() -> Self {
        PrimitiveMaterial {
            color: [1.0; 4],
            class: AlphaClass::Opaque,
            cutoff: 0.5,
            texture: None,
        }
    }
}

impl PrimitiveMaterial {
    /// True when the material can never put a pixel on screen: a blended surface whose
    /// colour is fully transparent, with no texture that could carry an alpha of its own.
    ///
    /// Scenes build invisible collision and trigger volumes this way — a box scaled to the
    /// whole scene, drawn with `albedoColor.a == 0` — and the player never sees them.
    /// Production's Unity pipeline read only `GltfContainer`, so it never met one; the LOD
    /// does, and keeping them costs bytes and leaves a ghost surface around the scene
    /// (Hall of Fame's 32 x 20 x 32 box, found 2026-09-11).
    ///
    /// A resolved texture keeps the primitive even at `a == 0`, since the sampled texel
    /// supplies the alpha. A texture the deployment does not ship resolves to `None`, which
    /// is also what the explorer renders: colour only, so fully transparent there too.
    pub fn draws_nothing(&self) -> bool {
        matches!(self.class, AlphaClass::Blend) && self.texture.is_none() && self.color[3] <= 0.0
    }
}

/// One MeshRenderer entity, resolved to a world TRS in descriptor (Unity) space.
#[derive(Clone, Debug, PartialEq)]
pub struct PrimitivePlacement {
    pub spec: PrimitiveSpec,
    pub material: PrimitiveMaterial,
    pub position: [f64; 3],
    pub rotation: [f64; 4],
    pub scale: [f64; 3],
}

// ── material rules ───────────────────────────────────────────────────────────

pub const TRANSPARENCY_OPAQUE: u32 = 0;
pub const TRANSPARENCY_ALPHA_TEST: u32 = 1;
pub const TRANSPARENCY_ALPHA_BLEND: u32 = 2;
pub const TRANSPARENCY_ALPHA_TEST_AND_BLEND: u32 = 3;
pub const TRANSPARENCY_AUTO: u32 = 4;

/// The `Texture` message fields the LOD consults.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TextureFields {
    pub src: String,
    /// `TextureWrapMode` as written; absent means the explorer default (Clamp).
    pub wrap_mode: Option<u32>,
}

/// `PBMaterial` decoded but not yet interpreted: the same struct comes out of the JSON
/// manifest and the protobuf payload so both lanes share [`resolve_material`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MaterialFields {
    /// `pbr` variant when true, `unlit` otherwise.
    pub pbr: bool,
    /// `albedoColor` (pbr) / `diffuseColor` (unlit).
    pub color: Option<[f64; 4]>,
    pub alpha_test: Option<f64>,
    /// pbr only; unlit is always AUTO.
    pub transparency_mode: Option<u32>,
    /// `texture` when it is the `Texture` case (video/avatar textures have no content).
    pub texture: Option<TextureFields>,
    /// unlit only: `alphaTexture` is a `Texture` case. The explorer's PBR path passes no
    /// alpha texture into `ResolveAutoMode`, so the deprecated pbr field is not read.
    pub has_alpha_texture: bool,
}

fn wrap_mode(raw: Option<u32>) -> WrapMode {
    // `TextureDefaultsExtensions.GetWrapMode`: HasWrapMode ? value : Clamp.
    match raw {
        Some(0) => WrapMode::Repeat,
        Some(2) => WrapMode::Mirror,
        _ => WrapMode::Clamp,
    }
}

/// `PBMaterialExtensions` defaults + `MaterialTransparencyMode.ResolveAutoMode`.
/// `lowered` maps lower-cased content file names to hashes. Returns the material and
/// whether its texture named a scene file that the entity does not ship (counted, colour
/// only). An `http(s)` src is kept as a URL; an empty src is no texture.
pub fn resolve_material(
    fields: Option<&MaterialFields>,
    lowered: &HashMap<String, &String>,
) -> (PrimitiveMaterial, bool) {
    let Some(m) = fields else {
        return (PrimitiveMaterial::default(), false);
    };
    let color = m.color.unwrap_or([1.0; 4]);
    let mut mode = if m.pbr {
        m.transparency_mode.unwrap_or(TRANSPARENCY_AUTO)
    } else {
        TRANSPARENCY_AUTO
    };
    if mode == TRANSPARENCY_AUTO {
        let has_alpha_texture = !m.pbr && m.has_alpha_texture;
        mode = if has_alpha_texture || color[3] < 1.0 {
            TRANSPARENCY_ALPHA_BLEND
        } else {
            TRANSPARENCY_OPAQUE
        };
    }
    let class = match mode {
        TRANSPARENCY_ALPHA_TEST => AlphaClass::Mask,
        TRANSPARENCY_ALPHA_BLEND | TRANSPARENCY_ALPHA_TEST_AND_BLEND => AlphaClass::Blend,
        _ => AlphaClass::Opaque,
    };
    let mut out = PrimitiveMaterial {
        color,
        class,
        cutoff: m.alpha_test.unwrap_or(0.5),
        texture: None,
    };
    let mut missing = false;
    if let Some(tex) = &m.texture {
        if !tex.src.is_empty() {
            let wrap = wrap_mode(tex.wrap_mode);
            let lower = tex.src.to_ascii_lowercase();
            if lower.starts_with("http://") || lower.starts_with("https://") {
                out.texture = Some(PrimitiveTexture {
                    source: TextureSource::Url(tex.src.clone()),
                    wrap,
                });
            } else {
                let mut key = lower.as_str();
                loop {
                    if let Some(rest) = key.strip_prefix("./") {
                        key = rest;
                    } else if let Some(rest) = key.strip_prefix('/') {
                        key = rest;
                    } else {
                        break;
                    }
                }
                match lowered.get(key) {
                    Some(hash) => {
                        out.texture = Some(PrimitiveTexture {
                            source: TextureSource::Hash((*hash).clone()),
                            wrap,
                        });
                    }
                    None => missing = true,
                }
            }
        }
    }
    (out, missing)
}

// ── JSON manifest decoding ───────────────────────────────────────────────────

fn json_f32s(v: Option<&serde_json::Value>) -> Vec<f32> {
    v.and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_f64())
                .map(|x| x as f32)
                .collect()
        })
        .unwrap_or_default()
}

fn json_f32(v: Option<&serde_json::Value>) -> Option<f32> {
    v.and_then(|x| x.as_f64()).map(|x| x as f32)
}

/// `core::MeshRenderer` row `data` as the npm manifest builder serialises the protobuf
/// oneof (`{"mesh": {"$case": "box", "box": {"uvs": [...]}}}`). `None` for a null/DELETE
/// payload or an unset oneof.
pub fn mesh_renderer_from_json(data: &serde_json::Value) -> Option<PrimitiveSpec> {
    let mesh = data.get("mesh")?;
    let case = mesh.get("$case").and_then(|c| c.as_str())?;
    match case {
        "box" => Some(PrimitiveSpec {
            shape: PrimitiveShape::Box,
            uvs: json_f32s(mesh.get("box").and_then(|b| b.get("uvs"))),
            radius_top: None,
            radius_bottom: None,
        }),
        "plane" => Some(PrimitiveSpec {
            shape: PrimitiveShape::Plane,
            uvs: json_f32s(mesh.get("plane").and_then(|b| b.get("uvs"))),
            radius_top: None,
            radius_bottom: None,
        }),
        "sphere" => Some(PrimitiveSpec::simple(PrimitiveShape::Sphere)),
        "cylinder" => {
            let c = mesh.get("cylinder");
            Some(PrimitiveSpec {
                shape: PrimitiveShape::Cylinder,
                uvs: Vec::new(),
                radius_top: json_f32(c.and_then(|c| c.get("radiusTop"))),
                radius_bottom: json_f32(c.and_then(|c| c.get("radiusBottom"))),
            })
        }
        _ => None,
    }
}

fn json_color(v: Option<&serde_json::Value>) -> Option<[f64; 4]> {
    let c = v?;
    let f = |k: &str, d: f64| c.get(k).and_then(|x| x.as_f64()).unwrap_or(d);
    Some([f("r", 0.0), f("g", 0.0), f("b", 0.0), f("a", 1.0)])
}

fn json_texture(v: Option<&serde_json::Value>) -> Option<TextureFields> {
    let tex = v?.get("tex")?;
    if tex.get("$case").and_then(|c| c.as_str()) != Some("texture") {
        return None;
    }
    let t = tex.get("texture")?;
    Some(TextureFields {
        src: t
            .get("src")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
        wrap_mode: t.get("wrapMode").and_then(|w| w.as_u64()).map(|w| w as u32),
    })
}

/// `core::Material` row `data` (`{"material": {"$case": "pbr", "pbr": {...}}}`).
pub fn material_from_json(data: &serde_json::Value) -> Option<MaterialFields> {
    let mat = data.get("material")?;
    let case = mat.get("$case").and_then(|c| c.as_str())?;
    let pbr = match case {
        "pbr" => true,
        "unlit" => false,
        _ => return None,
    };
    let props = mat.get(case)?;
    Some(MaterialFields {
        pbr,
        color: json_color(props.get(if pbr { "albedoColor" } else { "diffuseColor" })),
        alpha_test: props.get("alphaTest").and_then(|x| x.as_f64()),
        transparency_mode: if pbr {
            props
                .get("transparencyMode")
                .and_then(|x| x.as_u64())
                .map(|x| x as u32)
        } else {
            None
        },
        texture: json_texture(props.get("texture")),
        has_alpha_texture: !pbr && json_texture(props.get("alphaTexture")).is_some(),
    })
}

// ── protobuf decoding ────────────────────────────────────────────────────────

/// One protobuf field as it sits on the wire.
enum Field<'a> {
    Varint(u64),
    Fixed32([u8; 4]),
    Bytes(&'a [u8]),
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

/// Walks a message, calling `f(field_number, field)` for each field; `None` on a
/// malformed payload.
fn for_each_field<'a>(data: &'a [u8], mut f: impl FnMut(u64, Field<'a>)) -> Option<()> {
    let mut off = 0usize;
    while off < data.len() {
        let (tag, next) = read_varint(data, off)?;
        off = next;
        let field = tag >> 3;
        match tag & 7 {
            0 => {
                let (v, next) = read_varint(data, off)?;
                off = next;
                f(field, Field::Varint(v));
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
                f(field, Field::Bytes(&data[off..end]));
                off = end;
            }
            5 => {
                let end = off.checked_add(4)?;
                if end > data.len() {
                    return None;
                }
                f(field, Field::Fixed32(data[off..end].try_into().unwrap()));
                off = end;
            }
            _ => return None,
        }
    }
    Some(())
}

fn f32_of(b: [u8; 4]) -> f32 {
    f32::from_le_bytes(b)
}

/// `repeated float`: packed (one length-delimited run of 4-byte floats) or, for a
/// non-conforming writer, one fixed32 per element.
fn push_floats(out: &mut Vec<f32>, field: Field<'_>) {
    match field {
        Field::Bytes(b) => {
            for chunk in b.chunks_exact(4) {
                out.push(f32_of(chunk.try_into().unwrap()));
            }
        }
        Field::Fixed32(b) => out.push(f32_of(b)),
        Field::Varint(_) => {}
    }
}

/// `PBMeshRenderer`: `oneof mesh { BoxMesh box = 1; SphereMesh sphere = 2; CylinderMesh
/// cylinder = 3; PlaneMesh plane = 4; }`; `BoxMesh.uvs = 1`, `PlaneMesh.uvs = 1`,
/// `CylinderMesh.radius_top = 1`, `radius_bottom = 2`. Proto3 keeps the last oneof
/// member on the wire. `None` for an unset oneof (including the empty message) or a
/// malformed payload.
pub fn mesh_renderer_from_proto(data: &[u8]) -> Option<PrimitiveSpec> {
    let mut spec: Option<PrimitiveSpec> = None;
    let mut ok = true;
    for_each_field(data, |field, value| {
        let Field::Bytes(body) = value else {
            return;
        };
        match field {
            1 | 4 => {
                let shape = if field == 1 {
                    PrimitiveShape::Box
                } else {
                    PrimitiveShape::Plane
                };
                let mut uvs = Vec::new();
                if for_each_field(body, |inner, v| {
                    if inner == 1 {
                        push_floats(&mut uvs, v);
                    }
                })
                .is_none()
                {
                    ok = false;
                }
                spec = Some(PrimitiveSpec {
                    shape,
                    uvs,
                    radius_top: None,
                    radius_bottom: None,
                });
            }
            2 => spec = Some(PrimitiveSpec::simple(PrimitiveShape::Sphere)),
            3 => {
                let mut top = None;
                let mut bottom = None;
                if for_each_field(body, |inner, v| {
                    if let Field::Fixed32(b) = v {
                        match inner {
                            1 => top = Some(f32_of(b)),
                            2 => bottom = Some(f32_of(b)),
                            _ => {}
                        }
                    }
                })
                .is_none()
                {
                    ok = false;
                }
                spec = Some(PrimitiveSpec {
                    shape: PrimitiveShape::Cylinder,
                    uvs: Vec::new(),
                    radius_top: top,
                    radius_bottom: bottom,
                });
            }
            _ => {}
        }
    })?;
    if ok {
        spec
    } else {
        None
    }
}

fn color_from_proto(body: &[u8]) -> Option<[f64; 4]> {
    // Color4 { r = 1; g = 2; b = 3; a = 4 }: proto3 omits zero components, so a channel
    // that is not on the wire is 0 — including alpha.
    let mut c = [0.0f64; 4];
    for_each_field(body, |field, v| {
        if let Field::Fixed32(b) = v {
            if (1..=4).contains(&field) {
                c[(field - 1) as usize] = f64::from(f32_of(b));
            }
        }
    })?;
    Some(c)
}

/// `TextureUnion { oneof tex { Texture texture = 1; AvatarTexture = 2; VideoTexture = 3 } }`
/// → the `Texture` case only. `Texture { string src = 1; optional TextureWrapMode wrap_mode = 2 }`.
fn texture_from_proto(body: &[u8]) -> Option<TextureFields> {
    let mut out: Option<TextureFields> = None;
    for_each_field(body, |field, v| {
        match (field, v) {
            (1, Field::Bytes(tex)) => {
                let mut t = TextureFields::default();
                let _ = for_each_field(tex, |inner, iv| match (inner, iv) {
                    (1, Field::Bytes(s)) => t.src = String::from_utf8_lossy(s).into_owned(),
                    (2, Field::Varint(w)) => t.wrap_mode = Some(w as u32),
                    _ => {}
                });
                out = Some(t);
            }
            // A later avatar/video member replaces the texture case (proto3 oneof).
            (2 | 3, Field::Bytes(_)) => out = None,
            _ => {}
        }
    })?;
    out
}

/// `PBMaterial { oneof material { UnlitMaterial unlit = 1; PbrMaterial pbr = 2 } }`.
/// Unlit: `texture = 1, alpha_test = 2, diffuse_color = 4, alpha_texture = 5`.
/// Pbr: `texture = 1, alpha_test = 2, albedo_color = 7, transparency_mode = 10`.
/// `None` for an unset oneof or a malformed payload.
pub fn material_from_proto(data: &[u8]) -> Option<MaterialFields> {
    let mut out: Option<MaterialFields> = None;
    let mut ok = true;
    for_each_field(data, |field, value| {
        let Field::Bytes(body) = value else {
            return;
        };
        let pbr = match field {
            1 => false,
            2 => true,
            _ => return,
        };
        let mut m = MaterialFields {
            pbr,
            ..Default::default()
        };
        let walked = for_each_field(body, |inner, v| match (inner, v) {
            (1, Field::Bytes(b)) => m.texture = texture_from_proto(b),
            (2, Field::Fixed32(b)) => m.alpha_test = Some(f64::from(f32_of(b))),
            (4, Field::Bytes(b)) if !pbr => m.color = color_from_proto(b),
            (5, Field::Bytes(b)) if !pbr => m.has_alpha_texture = texture_from_proto(b).is_some(),
            (7, Field::Bytes(b)) if pbr => m.color = color_from_proto(b),
            (10, Field::Varint(t)) if pbr => m.transparency_mode = Some(t as u32),
            _ => {}
        });
        if walked.is_none() {
            ok = false;
        }
        out = Some(m);
    })?;
    if ok {
        out
    } else {
        None
    }
}

// ── geometry (Unity space) ───────────────────────────────────────────────────

pub const BOX_VERTICES: usize = 24;
pub const PLANE_VERTICES: usize = 8;
pub const SPHERE_LONGITUDE: usize = 24;
pub const SPHERE_LATITUDE: usize = 16;
pub const SPHERE_RADIUS: f64 = 0.5;
pub const CYLINDER_SEGMENTS: usize = 50;
pub const CYLINDER_HEIGHT: f64 = 1.0;
pub const CYLINDER_RADIUS: f32 = 0.5;

/// A primitive mesh in Unity space: left-handed positions/normals, UV origin bottom-left,
/// Unity front-face winding — the frame `crate::gltf::parse` leaves an imported GLB in.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PrimitiveGeometry {
    pub positions: Vec<[f64; 3]>,
    pub normals: Vec<[f64; 3]>,
    pub uvs: Vec<[f64; 2]>,
    pub indices: Vec<u32>,
}

/// Builds the explorer's mesh for `spec`, custom UVs applied.
pub fn build_geometry(spec: &PrimitiveSpec) -> PrimitiveGeometry {
    let mut mesh = match spec.shape {
        PrimitiveShape::Box => box_mesh(),
        PrimitiveShape::Plane => plane_mesh(),
        PrimitiveShape::Sphere => sphere_mesh(),
        PrimitiveShape::Cylinder => cylinder_mesh(
            f64::from(spec.radius_top.unwrap_or(CYLINDER_RADIUS)),
            f64::from(spec.radius_bottom.unwrap_or(CYLINDER_RADIUS)),
        ),
    };
    if !spec.uvs.is_empty() && matches!(spec.shape, PrimitiveShape::Box | PrimitiveShape::Plane) {
        apply_custom_uvs(&mut mesh, &spec.uvs);
    }
    mesh
}

/// `PrimitivesUtility.FloatArrayToV2List`: sequential `(u, v)` pairs overwrite the
/// defaults from vertex 0; extra pairs are ignored, a short array leaves the tail.
fn apply_custom_uvs(mesh: &mut PrimitiveGeometry, uvs: &[f32]) {
    for (dst, src) in mesh.uvs.iter_mut().zip(uvs.chunks_exact(2)) {
        *dst = [f64::from(src[0]), f64::from(src[1])];
    }
}

fn add(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

const RIGHT: [f64; 3] = [1.0, 0.0, 0.0];
const DOWN: [f64; 3] = [0.0, -1.0, 0.0];
const BACK: [f64; 3] = [0.0, 0.0, -1.0];
const FORWARD: [f64; 3] = [0.0, 0.0, 1.0];

/// `BoxFactory`: faces top, bottom, left, right, front, back; 4 vertices each; unit cube
/// centred on the origin.
fn box_mesh() -> PrimitiveGeometry {
    let s = 0.5;
    let mut positions: Vec<[f64; 3]> = Vec::with_capacity(BOX_VERTICES);
    let mut quad = |start: [f64; 3], d1: [f64; 3], d2: [f64; 3]| {
        positions.push(start);
        positions.push(add(start, d1));
        positions.push(add(add(start, d1), d2));
        positions.push(add(start, d2));
    };
    quad([-s, s, s], RIGHT, BACK); // top
    quad([-s, -s, s], RIGHT, BACK); // bottom
    quad([-s, s, s], BACK, DOWN); // left
    quad([s, s, s], BACK, DOWN); // right
    quad([-s, s, s], RIGHT, DOWN); // front
    quad([-s, s, -s], RIGHT, DOWN); // back

    let face_normals: [[f64; 3]; 6] = [
        [0.0, 1.0, 0.0],
        [0.0, -1.0, 0.0],
        [-1.0, 0.0, 0.0],
        [1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0],
        [0.0, 0.0, -1.0],
    ];
    let normals: Vec<[f64; 3]> = face_normals
        .iter()
        .flat_map(|n| std::iter::repeat_n(*n, 4))
        .collect();

    let uvs: Vec<[f64; 2]> = vec![
        [1.0, 1.0],
        [1.0, 0.0],
        [0.0, 0.0],
        [0.0, 1.0], // top
        [1.0, 0.0],
        [1.0, 1.0],
        [0.0, 1.0],
        [0.0, 0.0], // bottom
        [1.0, 1.0],
        [1.0, 0.0],
        [0.0, 0.0],
        [0.0, 1.0], // left
        [1.0, 0.0],
        [1.0, 1.0],
        [0.0, 1.0],
        [0.0, 0.0], // right
        [0.0, 0.0],
        [1.0, 0.0],
        [1.0, 1.0],
        [0.0, 1.0], // front
        [0.0, 1.0],
        [1.0, 1.0],
        [1.0, 0.0],
        [0.0, 0.0], // back
    ];

    let indices: Vec<u32> = vec![
        0, 1, 2, 0, 2, 3, // top
        4, 6, 5, 4, 7, 6, // bottom
        8, 9, 10, 8, 10, 11, // left
        12, 14, 13, 12, 15, 14, // right
        16, 18, 17, 16, 19, 18, // front
        20, 21, 22, 20, 22, 23, // back
    ];
    PrimitiveGeometry {
        positions,
        normals,
        uvs,
        indices,
    }
}

/// `PlaneFactory`: two-sided unit quad in XY (8 vertices; side B is side A reversed).
fn plane_mesh() -> PrimitiveGeometry {
    let h = 0.5;
    PrimitiveGeometry {
        positions: vec![
            [-h, -h, 0.0],
            [-h, h, 0.0],
            [h, h, 0.0],
            [h, -h, 0.0],
            [h, -h, 0.0],
            [h, h, 0.0],
            [-h, h, 0.0],
            [-h, -h, 0.0],
        ],
        normals: vec![BACK, BACK, BACK, BACK, FORWARD, FORWARD, FORWARD, FORWARD],
        uvs: vec![
            [0.0, 0.0],
            [0.0, 1.0],
            [1.0, 1.0],
            [1.0, 0.0],
            [1.0, 0.0],
            [1.0, 1.0],
            [0.0, 1.0],
            [0.0, 0.0],
        ],
        indices: vec![0, 1, 2, 2, 3, 0, 4, 5, 6, 6, 7, 4],
    }
}

/// `SphereFactory`: UV sphere, 24 longitude x 16 latitude rings, radius 0.5.
fn sphere_mesh() -> PrimitiveGeometry {
    let nb_long = SPHERE_LONGITUDE;
    let nb_lat = SPHERE_LATITUDE;
    let radius = SPHERE_RADIUS;
    let count = (nb_long + 1) * nb_lat + 2;
    let mut positions = vec![[0.0f64; 3]; count];
    let mut uvs = vec![[0.0f64; 2]; count];

    positions[0] = [0.0, radius, 0.0];
    for lat in 0..nb_lat {
        let a1 = std::f64::consts::PI * (lat as f64 + 1.0) / (nb_lat as f64 + 1.0);
        let (sin1, cos1) = a1.sin_cos();
        for lon in 0..=nb_long {
            let a2 = 2.0 * std::f64::consts::PI * (if lon == nb_long { 0 } else { lon }) as f64
                / nb_long as f64;
            let i = lon + lat * (nb_long + 1) + 1;
            positions[i] = [
                sin1 * a2.cos() * radius,
                cos1 * radius,
                sin1 * a2.sin() * radius,
            ];
            uvs[i] = [
                1.0 - lon as f64 / nb_long as f64,
                (lat as f64 + 1.0) / (nb_lat as f64 + 1.0),
            ];
        }
    }
    positions[count - 1] = [0.0, -radius, 0.0];
    uvs[0] = [0.0, 1.0];
    uvs[count - 1] = [0.0, 0.0];

    let normals: Vec<[f64; 3]> = positions
        .iter()
        .map(|p| {
            let l = (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt();
            let l = if l == 0.0 { 1.0 } else { l };
            [p[0] / l, p[1] / l, p[2] / l]
        })
        .collect();

    let mut indices: Vec<u32> = Vec::new();
    for lon in 0..nb_long {
        indices.extend_from_slice(&[(lon + 2) as u32, (lon + 1) as u32, 0]); // top cap
    }
    for lat in 0..nb_lat - 1 {
        for lon in 0..nb_long {
            let current = lon + lat * (nb_long + 1) + 1;
            let next = current + nb_long + 1;
            indices.extend_from_slice(&[
                current as u32,
                (current + 1) as u32,
                (next + 1) as u32,
                current as u32,
                (next + 1) as u32,
                next as u32,
            ]);
        }
    }
    for lon in 0..nb_long {
        indices.extend_from_slice(&[
            (count - 1) as u32,
            (count - (lon + 2) - 1) as u32,
            (count - (lon + 1) - 1) as u32,
        ]); // bottom cap
    }
    PrimitiveGeometry {
        positions,
        normals,
        uvs,
        indices,
    }
}

/// `CylinderVariantsFactory`: cylinder / cone / truncated cone, height 1 centred on the
/// origin, 50 segments.
fn cylinder_mesh(radius_top: f64, radius_bottom: f64) -> PrimitiveGeometry {
    use std::f64::consts::PI;
    let n = CYLINDER_SEGMENTS;
    let length = CYLINDER_HEIGHT;
    let n2 = n + 1;
    let y_off = -length / 2.0;
    let total = 4 * n2;
    let mut positions = vec![[0.0f64; 3]; total];
    let mut normals = vec![[0.0f64; 3]; total];
    let mut uvs = vec![[0.0f64; 2]; total];

    let slope = ((radius_bottom - radius_top) / length).atan();
    let (slope_sin, slope_cos) = slope.sin_cos();

    for i in 0..n {
        let angle = 2.0 * PI * i as f64 / n as f64;
        let (a_sin, a_cos) = angle.sin_cos();
        let angle_half = 2.0 * PI * (i as f64 + 0.5) / n as f64; // degenerate normals at cone tips
        let (h_sin, h_cos) = angle_half.sin_cos();

        positions[i] = [radius_top * a_cos, length + y_off, radius_top * a_sin];
        positions[i + n2] = [radius_bottom * a_cos, y_off, radius_bottom * a_sin];
        normals[i] = if radius_top == 0.0 {
            [h_cos * slope_cos, -slope_sin, h_sin * slope_cos]
        } else {
            [a_cos * slope_cos, -slope_sin, a_sin * slope_cos]
        };
        normals[i + n2] = if radius_bottom == 0.0 {
            [h_cos * slope_cos, -slope_sin, h_sin * slope_cos]
        } else {
            [a_cos * slope_cos, -slope_sin, a_sin * slope_cos]
        };
        uvs[i] = [1.0 - i as f64 / n as f64, 1.0];
        uvs[i + n2] = [1.0 - i as f64 / n as f64, 0.0];
    }
    positions[n] = positions[0];
    positions[n + n2] = positions[n2];
    uvs[n] = [0.0, 1.0];
    uvs[n + n2] = [0.0, 0.0];
    normals[n] = normals[0];
    normals[n + n2] = normals[n2];

    let top_start = 2 * n2;
    let top_end = top_start + n;
    for i in 0..n {
        let angle = 2.0 * PI * i as f64 / n as f64;
        let (a_sin, a_cos) = angle.sin_cos();
        positions[top_start + i] = [radius_top * a_cos, length + y_off, radius_top * a_sin];
        normals[top_start + i] = [0.0, 1.0, 0.0];
        uvs[top_start + i] = [a_cos / 2.0 + 0.5, a_sin / 2.0 + 0.5];
    }
    positions[top_end] = [0.0, length + y_off, 0.0];
    normals[top_end] = [0.0, 1.0, 0.0];
    uvs[top_end] = [0.5, 0.5];

    let bottom_start = top_end + 1;
    let bottom_end = bottom_start + n;
    for i in 0..n {
        let angle = 2.0 * PI * i as f64 / n as f64;
        let (a_sin, a_cos) = angle.sin_cos();
        positions[bottom_start + i] = [radius_bottom * a_cos, y_off, radius_bottom * a_sin];
        normals[bottom_start + i] = [0.0, -1.0, 0.0];
        uvs[bottom_start + i] = [a_cos / 2.0 + 0.5, a_sin / 2.0 + 0.5];
    }
    positions[bottom_end] = [0.0, y_off, 0.0];
    normals[bottom_end] = [0.0, -1.0, 0.0];
    uvs[bottom_end] = [0.5, 0.5];

    let mut indices: Vec<u32> = Vec::new();
    let u = |x: usize| x as u32;
    if radius_top == 0.0 {
        for i in 0..n {
            indices.extend_from_slice(&[u(i + n2), u(i), u(i + 1 + n2)]); // cone, apex on top
        }
    } else if radius_bottom == 0.0 {
        for i in 0..n {
            indices.extend_from_slice(&[u(i), u(i + 1), u(i + n2)]); // cone, apex at bottom
        }
    } else {
        for i in 0..n {
            indices.extend_from_slice(&[
                u(i),
                u(i + 1),
                u(i + n2),
                u(i + 1 + n2),
                u(i + n2),
                u(i + 1),
            ]);
        }
    }
    for i in 0..n {
        let next = if i + 1 == n {
            top_start
        } else {
            top_start + i + 1
        };
        indices.extend_from_slice(&[u(next), u(top_start + i), u(top_end)]);
    }
    for i in 0..n {
        let next = if i + 1 == n {
            bottom_start
        } else {
            bottom_start + i + 1
        };
        indices.extend_from_slice(&[u(bottom_end), u(bottom_start + i), u(next)]);
    }
    PrimitiveGeometry {
        positions,
        normals,
        uvs,
        indices,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
        [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
    }

    fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
        [
            a[1] * b[2] - a[2] * b[1],
            a[2] * b[0] - a[0] * b[2],
            a[0] * b[1] - a[1] * b[0],
        ]
    }

    fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
        a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
    }

    /// In the explorer's Unity meshes `cross(b - a, c - a)` points along the vertex
    /// normals for every non-degenerate triangle (checked by hand on the box top face,
    /// the plane's side A and the sphere's top cap). A port that broke vertex order or
    /// winding would flip some of these.
    fn winding_matches_normals(g: &PrimitiveGeometry) -> bool {
        g.indices.chunks_exact(3).all(|t| {
            let (a, b, c) = (
                g.positions[t[0] as usize],
                g.positions[t[1] as usize],
                g.positions[t[2] as usize],
            );
            let face = cross(sub(b, a), sub(c, a));
            let len = dot(face, face).sqrt();
            if len < 1e-12 {
                return true;
            }
            let n = add(
                add(g.normals[t[0] as usize], g.normals[t[1] as usize]),
                g.normals[t[2] as usize],
            );
            dot(face, n) > 0.0
        })
    }

    fn consistent(g: &PrimitiveGeometry) {
        assert_eq!(g.normals.len(), g.positions.len());
        assert_eq!(g.uvs.len(), g.positions.len());
        assert_eq!(g.indices.len() % 3, 0);
        assert!(g.indices.iter().all(|&i| (i as usize) < g.positions.len()));
        assert!(winding_matches_normals(g), "winding disagrees with normals");
    }

    #[test]
    fn box_matches_explorer_layout() {
        let g = build_geometry(&PrimitiveSpec::simple(PrimitiveShape::Box));
        consistent(&g);
        assert_eq!(g.positions.len(), BOX_VERTICES);
        assert_eq!(g.indices.len(), 36);
        // top face starts at (-0.5, 0.5, 0.5) and walks +X then -Z
        assert_eq!(g.positions[0], [-0.5, 0.5, 0.5]);
        assert_eq!(g.positions[1], [0.5, 0.5, 0.5]);
        assert_eq!(g.positions[2], [0.5, 0.5, -0.5]);
        assert_eq!(g.normals[0], [0.0, 1.0, 0.0]);
        assert_eq!(g.normals[23], [0.0, 0.0, -1.0]);
        assert_eq!(g.uvs[16], [0.0, 0.0]);
        let (mn, mx) = bounds(&g);
        assert_eq!(mn, [-0.5, -0.5, -0.5]);
        assert_eq!(mx, [0.5, 0.5, 0.5]);
    }

    #[test]
    fn custom_uvs_apply_sequentially_and_only_to_box_and_plane() {
        let mut uvs: Vec<f32> = Vec::new();
        for i in 0..6 {
            uvs.push(i as f32 * 0.1);
            uvs.push(1.0 - i as f32 * 0.1);
        }
        let spec = PrimitiveSpec {
            shape: PrimitiveShape::Box,
            uvs: uvs.clone(),
            radius_top: None,
            radius_bottom: None,
        };
        let g = build_geometry(&spec);
        for i in 0..6 {
            assert!((g.uvs[i][0] - i as f64 * 0.1).abs() < 1e-6);
            assert!((g.uvs[i][1] - (1.0 - i as f64 * 0.1)).abs() < 1e-6);
        }
        assert_eq!(g.uvs[6], [0.0, 1.0]); // untouched default (bottom face, vertex 2)

        let plane = build_geometry(&PrimitiveSpec {
            shape: PrimitiveShape::Plane,
            uvs: vec![0.25; 40],
            radius_top: None,
            radius_bottom: None,
        });
        assert!(plane.uvs.iter().all(|uv| *uv == [0.25, 0.25]));

        let sphere = build_geometry(&PrimitiveSpec {
            shape: PrimitiveShape::Sphere,
            uvs: vec![0.25; 40],
            radius_top: None,
            radius_bottom: None,
        });
        assert_eq!(sphere.uvs[0], [0.0, 1.0]);
    }

    #[test]
    fn plane_is_two_sided() {
        let g = build_geometry(&PrimitiveSpec::simple(PrimitiveShape::Plane));
        consistent(&g);
        assert_eq!(g.positions.len(), PLANE_VERTICES);
        assert_eq!(g.indices.len(), 12);
        assert_eq!(g.normals[0], [0.0, 0.0, -1.0]);
        assert_eq!(g.normals[4], [0.0, 0.0, 1.0]);
        assert!(g.positions.iter().all(|p| p[2] == 0.0));
    }

    #[test]
    fn sphere_counts_and_radius() {
        let g = build_geometry(&PrimitiveSpec::simple(PrimitiveShape::Sphere));
        consistent(&g);
        assert_eq!(
            g.positions.len(),
            (SPHERE_LONGITUDE + 1) * SPHERE_LATITUDE + 2
        );
        assert_eq!(g.indices.len() / 3, 768);
        for p in &g.positions {
            let r = dot(*p, *p).sqrt();
            assert!((r - SPHERE_RADIUS).abs() < 1e-9, "{r}");
        }
        assert_eq!(g.positions[0], [0.0, 0.5, 0.0]);
        assert_eq!(g.positions[g.positions.len() - 1], [0.0, -0.5, 0.0]);
    }

    #[test]
    fn cylinder_and_cones() {
        let cyl = build_geometry(&PrimitiveSpec::simple(PrimitiveShape::Cylinder));
        consistent(&cyl);
        assert_eq!(cyl.positions.len(), 4 * (CYLINDER_SEGMENTS + 1));
        assert_eq!(cyl.indices.len() / 3, CYLINDER_SEGMENTS * 4);
        let (mn, mx) = bounds(&cyl);
        assert!((mn[1] + 0.5).abs() < 1e-9 && (mx[1] - 0.5).abs() < 1e-9);
        assert!((mx[0] - 0.5).abs() < 1e-9);

        let cone = build_geometry(&PrimitiveSpec {
            shape: PrimitiveShape::Cylinder,
            uvs: Vec::new(),
            radius_top: Some(0.0),
            radius_bottom: Some(1.0),
        });
        consistent(&cone);
        assert_eq!(cone.indices.len() / 3, CYLINDER_SEGMENTS * 3);
        let (mn, mx) = bounds(&cone);
        assert!((mx[0] - 1.0).abs() < 1e-9 && (mn[0] + 1.0).abs() < 1e-9);

        let inverted = build_geometry(&PrimitiveSpec {
            shape: PrimitiveShape::Cylinder,
            uvs: Vec::new(),
            radius_top: Some(1.0),
            radius_bottom: Some(0.0),
        });
        consistent(&inverted);
        assert_eq!(inverted.indices.len() / 3, CYLINDER_SEGMENTS * 3);
    }

    fn bounds(g: &PrimitiveGeometry) -> ([f64; 3], [f64; 3]) {
        let mut mn = [f64::INFINITY; 3];
        let mut mx = [f64::NEG_INFINITY; 3];
        for p in &g.positions {
            for i in 0..3 {
                mn[i] = mn[i].min(p[i]);
                mx[i] = mx[i].max(p[i]);
            }
        }
        (mn, mx)
    }

    // ── decoding ─────────────────────────────────────────────────────────────

    fn varint(mut v: u64, out: &mut Vec<u8>) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return;
            }
            out.push(b | 0x80);
        }
    }

    fn field_bytes(field: u64, body: &[u8], out: &mut Vec<u8>) {
        varint((field << 3) | 2, out);
        varint(body.len() as u64, out);
        out.extend_from_slice(body);
    }

    fn field_f32(field: u64, v: f32, out: &mut Vec<u8>) {
        varint((field << 3) | 5, out);
        out.extend_from_slice(&v.to_le_bytes());
    }

    fn field_varint(field: u64, v: u64, out: &mut Vec<u8>) {
        varint(field << 3, out);
        varint(v, out);
    }

    fn color4(c: [f32; 4]) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, v) in c.iter().enumerate() {
            if *v != 0.0 {
                field_f32(i as u64 + 1, *v, &mut out);
            }
        }
        out
    }

    fn texture_union(src: &str, wrap: Option<u64>) -> Vec<u8> {
        let mut tex = Vec::new();
        field_bytes(1, src.as_bytes(), &mut tex);
        if let Some(w) = wrap {
            field_varint(2, w, &mut tex);
        }
        let mut out = Vec::new();
        field_bytes(1, &tex, &mut out);
        out
    }

    #[test]
    fn mesh_renderer_proto_decodes_every_shape() {
        assert_eq!(mesh_renderer_from_proto(&[]), None);

        let mut packed = Vec::new();
        for v in [0.1f32, 0.2, 0.3, 0.4] {
            packed.extend_from_slice(&v.to_le_bytes());
        }
        let mut box_body = Vec::new();
        field_bytes(1, &packed, &mut box_body);
        let mut msg = Vec::new();
        field_bytes(1, &box_body, &mut msg);
        let spec = mesh_renderer_from_proto(&msg).unwrap();
        assert_eq!(spec.shape, PrimitiveShape::Box);
        assert_eq!(spec.uvs, vec![0.1, 0.2, 0.3, 0.4]);

        // empty BoxMesh (SDK default `MeshRenderer.setBox`)
        assert_eq!(
            mesh_renderer_from_proto(&[0x0a, 0x00]),
            Some(PrimitiveSpec::simple(PrimitiveShape::Box))
        );
        assert_eq!(
            mesh_renderer_from_proto(&[0x12, 0x00]),
            Some(PrimitiveSpec::simple(PrimitiveShape::Sphere))
        );
        assert_eq!(
            mesh_renderer_from_proto(&[0x22, 0x00]),
            Some(PrimitiveSpec::simple(PrimitiveShape::Plane))
        );

        let mut cyl_body = Vec::new();
        field_f32(1, 0.0, &mut cyl_body);
        field_f32(2, 1.0, &mut cyl_body);
        let mut msg = Vec::new();
        field_bytes(3, &cyl_body, &mut msg);
        let spec = mesh_renderer_from_proto(&msg).unwrap();
        assert_eq!(spec.shape, PrimitiveShape::Cylinder);
        assert_eq!(spec.radius_top, Some(0.0));
        assert_eq!(spec.radius_bottom, Some(1.0));

        // unpacked floats and unknown fields are tolerated
        let mut plane_body = Vec::new();
        field_f32(1, 0.5, &mut plane_body);
        field_f32(1, 0.75, &mut plane_body);
        field_varint(9, 7, &mut plane_body);
        let mut msg = Vec::new();
        field_bytes(4, &plane_body, &mut msg);
        let spec = mesh_renderer_from_proto(&msg).unwrap();
        assert_eq!(spec.shape, PrimitiveShape::Plane);
        assert_eq!(spec.uvs, vec![0.5, 0.75]);

        // truncated length prefix
        assert_eq!(mesh_renderer_from_proto(&[0x0a, 0x05, 0x00]), None);
    }

    #[test]
    fn material_proto_decodes_pbr_and_unlit() {
        assert_eq!(material_from_proto(&[]), None);
        assert_eq!(material_from_proto(&[0x08, 0x01]), None); // field 1 as varint: not a message

        let mut pbr = Vec::new();
        field_bytes(1, &texture_union("images/a.png", Some(0)), &mut pbr);
        field_f32(2, 0.25, &mut pbr);
        field_bytes(7, &color4([1.0, 0.5, 0.25, 0.5]), &mut pbr);
        field_varint(10, u64::from(TRANSPARENCY_ALPHA_TEST), &mut pbr);
        field_f32(11, 0.9, &mut pbr); // metallic, ignored
        let mut msg = Vec::new();
        field_bytes(2, &pbr, &mut msg);
        let m = material_from_proto(&msg).unwrap();
        assert!(m.pbr);
        assert_eq!(m.alpha_test, Some(0.25));
        assert_eq!(m.transparency_mode, Some(TRANSPARENCY_ALPHA_TEST));
        assert_eq!(m.color, Some([1.0, 0.5, 0.25, 0.5]));
        let tex = m.texture.unwrap();
        assert_eq!(tex.src, "images/a.png");
        assert_eq!(tex.wrap_mode, Some(0));
        assert!(!m.has_alpha_texture);

        let mut unlit = Vec::new();
        field_bytes(4, &color4([0.0, 0.0, 1.0, 1.0]), &mut unlit);
        field_bytes(5, &texture_union("alpha.png", None), &mut unlit);
        let mut msg = Vec::new();
        field_bytes(1, &unlit, &mut msg);
        let m = material_from_proto(&msg).unwrap();
        assert!(!m.pbr);
        assert_eq!(m.color, Some([0.0, 0.0, 1.0, 1.0]));
        assert_eq!(m.transparency_mode, None);
        assert!(m.texture.is_none());
        assert!(m.has_alpha_texture);

        // video texture is not a static texture
        let mut video = Vec::new();
        field_bytes(3, &[0x08, 0x05], &mut video);
        let mut pbr = Vec::new();
        field_bytes(1, &video, &mut pbr);
        let mut msg = Vec::new();
        field_bytes(2, &pbr, &mut msg);
        assert!(material_from_proto(&msg).unwrap().texture.is_none());
    }

    #[test]
    fn json_rows_decode_like_the_manifest_builder_writes_them() {
        let box_row = serde_json::json!({"mesh": {"$case": "box", "box": {"uvs": []}}});
        assert_eq!(
            mesh_renderer_from_json(&box_row),
            Some(PrimitiveSpec::simple(PrimitiveShape::Box))
        );
        let cyl = serde_json::json!({"mesh": {"$case": "cylinder", "cylinder": {"radiusTop": 1, "radiusBottom": 0}}});
        let spec = mesh_renderer_from_json(&cyl).unwrap();
        assert_eq!(spec.radius_top, Some(1.0));
        assert_eq!(spec.radius_bottom, Some(0.0));
        let plane = serde_json::json!({"mesh": {"$case": "plane", "plane": {"uvs": [0, 1, 1, 1]}}});
        assert_eq!(
            mesh_renderer_from_json(&plane).unwrap().uvs,
            vec![0.0, 1.0, 1.0, 1.0]
        );
        assert_eq!(mesh_renderer_from_json(&serde_json::Value::Null), None);
        assert_eq!(
            mesh_renderer_from_json(&serde_json::json!({"mesh": {"$case": "gltf"}})),
            None
        );

        let mat = serde_json::json!({"material": {"$case": "pbr", "pbr": {
            "texture": {"tex": {"$case": "texture", "texture": {"src": "https://x.test/a.png", "wrapMode": 0, "filterMode": 0}}},
            "alphaTest": 0.5,
            "alphaTexture": {"tex": {"$case": "texture", "texture": {"src": "", "wrapMode": 0}}},
            "albedoColor": {"r": 1, "g": 1, "b": 1, "a": 0.3},
            "transparencyMode": 2, "metallic": 0, "roughness": 1
        }}});
        let m = material_from_json(&mat).unwrap();
        assert!(m.pbr);
        assert_eq!(m.color, Some([1.0, 1.0, 1.0, 0.3]));
        assert_eq!(m.transparency_mode, Some(2));
        assert_eq!(m.texture.as_ref().unwrap().src, "https://x.test/a.png");
        assert!(!m.has_alpha_texture);

        let unlit = serde_json::json!({"material": {"$case": "unlit", "unlit": {
            "diffuseColor": {"r": 0, "g": 1, "b": 0, "a": 1},
            "alphaTexture": {"tex": {"$case": "texture", "texture": {"src": "a.png"}}}
        }}});
        let m = material_from_json(&unlit).unwrap();
        assert!(!m.pbr);
        assert!(m.has_alpha_texture);
        assert_eq!(m.color, Some([0.0, 1.0, 0.0, 1.0]));

        let video = serde_json::json!({"material": {"$case": "pbr", "pbr": {
            "texture": {"tex": {"$case": "videoTexture", "videoTexture": {"videoPlayerEntity": 512}}}
        }}});
        assert!(material_from_json(&video).unwrap().texture.is_none());
    }

    #[test]
    fn draws_nothing_only_for_untextured_fully_transparent_blends() {
        let hash = "bafkreitex".to_string();
        let mut lowered: HashMap<String, &String> = HashMap::new();
        lowered.insert("images/a.png".to_string(), &hash);

        // The invisible volume: AUTO + alpha 0 resolves to BLEND, and nothing supplies alpha.
        let volume = MaterialFields {
            pbr: true,
            color: Some([0.0, 0.0, 0.0, 0.0]),
            ..Default::default()
        };
        let (m, _) = resolve_material(Some(&volume), &lowered);
        assert_eq!(m.class, AlphaClass::Blend);
        assert!(m.draws_nothing());

        // A resolved texture keeps it: the sampled texel carries the alpha.
        let textured = MaterialFields {
            texture: Some(TextureFields {
                src: "images/a.png".to_string(),
                wrap_mode: Some(0),
            }),
            ..volume.clone()
        };
        assert!(!resolve_material(Some(&textured), &lowered).0.draws_nothing());

        // A texture the deployment does not ship falls back to colour only, which is what
        // the explorer draws too — still nothing.
        let absent = MaterialFields {
            texture: Some(TextureFields {
                src: "images/gone.png".to_string(),
                wrap_mode: Some(0),
            }),
            ..volume.clone()
        };
        let (m, missing) = resolve_material(Some(&absent), &lowered);
        assert!(missing);
        assert!(m.draws_nothing());

        // Alpha 0 under an explicit OPAQUE mode still draws; so does a merely faint blend.
        let opaque_zero = MaterialFields {
            transparency_mode: Some(TRANSPARENCY_OPAQUE),
            ..volume.clone()
        };
        assert!(!resolve_material(Some(&opaque_zero), &lowered).0.draws_nothing());
        let faint = MaterialFields {
            color: Some([1.0, 1.0, 1.0, 0.02]),
            ..volume
        };
        assert!(!resolve_material(Some(&faint), &lowered).0.draws_nothing());
        assert!(!PrimitiveMaterial::default().draws_nothing());
    }

    #[test]
    fn material_resolution_follows_the_explorer() {
        let hash = "bafkreitex".to_string();
        let mut lowered: HashMap<String, &String> = HashMap::new();
        lowered.insert("images/a.png".to_string(), &hash);

        let (m, missing) = resolve_material(None, &lowered);
        assert_eq!(m, PrimitiveMaterial::default());
        assert!(!missing);

        // AUTO + opaque albedo → OPAQUE; AUTO + alpha < 1 → BLEND
        let auto = MaterialFields {
            pbr: true,
            color: Some([1.0, 0.0, 0.0, 1.0]),
            ..Default::default()
        };
        assert_eq!(
            resolve_material(Some(&auto), &lowered).0.class,
            AlphaClass::Opaque
        );
        let glass = MaterialFields {
            color: Some([1.0, 1.0, 1.0, 0.3]),
            ..auto.clone()
        };
        assert_eq!(
            resolve_material(Some(&glass), &lowered).0.class,
            AlphaClass::Blend
        );

        // explicit ALPHA_TEST → MASK with the scene's cutoff (default 0.5)
        let cutout = MaterialFields {
            transparency_mode: Some(TRANSPARENCY_ALPHA_TEST),
            alpha_test: Some(0.7),
            ..auto.clone()
        };
        let (m, _) = resolve_material(Some(&cutout), &lowered);
        assert_eq!(m.class, AlphaClass::Mask);
        assert_eq!(m.cutoff, 0.7);
        let cutout_default = MaterialFields {
            alpha_test: None,
            ..cutout
        };
        assert_eq!(
            resolve_material(Some(&cutout_default), &lowered).0.cutoff,
            0.5
        );

        // explicit mode overrides the alpha heuristic; unlit ignores transparency_mode
        let opaque_glass = MaterialFields {
            transparency_mode: Some(TRANSPARENCY_OPAQUE),
            ..glass.clone()
        };
        assert_eq!(
            resolve_material(Some(&opaque_glass), &lowered).0.class,
            AlphaClass::Opaque
        );
        let unlit_alpha = MaterialFields {
            pbr: false,
            color: Some([1.0; 4]),
            has_alpha_texture: true,
            transparency_mode: Some(TRANSPARENCY_OPAQUE),
            ..Default::default()
        };
        assert_eq!(
            resolve_material(Some(&unlit_alpha), &lowered).0.class,
            AlphaClass::Blend
        );
        // the pbr lane never sees an alpha texture
        let pbr_alpha = MaterialFields {
            pbr: true,
            color: Some([1.0; 4]),
            has_alpha_texture: true,
            ..Default::default()
        };
        assert_eq!(
            resolve_material(Some(&pbr_alpha), &lowered).0.class,
            AlphaClass::Opaque
        );

        // textures: scene file (case-insensitive, leading ./ stripped), URL, missing, empty
        let with = |src: &str, wrap: Option<u32>| MaterialFields {
            pbr: true,
            texture: Some(TextureFields {
                src: src.to_string(),
                wrap_mode: wrap,
            }),
            ..Default::default()
        };
        let (m, missing) = resolve_material(Some(&with("./Images/A.PNG", Some(0))), &lowered);
        assert!(!missing);
        assert_eq!(
            m.texture,
            Some(PrimitiveTexture {
                source: TextureSource::Hash(hash.clone()),
                wrap: WrapMode::Repeat,
            })
        );
        let (m, missing) = resolve_material(Some(&with("HTTPS://cdn.test/t.jpg", None)), &lowered);
        assert!(!missing);
        assert_eq!(
            m.texture,
            Some(PrimitiveTexture {
                source: TextureSource::Url("HTTPS://cdn.test/t.jpg".to_string()),
                wrap: WrapMode::Clamp,
            })
        );
        let (m, missing) = resolve_material(Some(&with("images/missing.png", Some(2))), &lowered);
        assert!(missing);
        assert!(m.texture.is_none());
        let (m, missing) = resolve_material(Some(&with("", Some(1))), &lowered);
        assert!(!missing);
        assert!(m.texture.is_none());
    }
}

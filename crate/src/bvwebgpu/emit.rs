use anyhow::{anyhow, bail, Context, Result};
use gltf_json::accessor::{ComponentType, GenericComponentType, Type};
use gltf_json::validation::{Checked, USize64};
use gltf_json::{buffer, mesh, Index, Root};
use image::RgbaImage;
use std::collections::{BTreeSet, HashSet};
use std::io::Write;

pub(crate) type ResolveUri<'a> = crate::gltf::Resolve<'a>;

pub(crate) fn transform_img(bytes: &[u8]) -> Result<Vec<u8>> {
    let img = image::load_from_memory(bytes).context("decode image")?;
    if keep_raw(img.width(), img.height(), bytes.len()) {
        return Ok(bytes.to_vec());
    }
    encode_dds_bc7(&img.to_rgba8(), true, false)
}

const DDS_HEADER_LEN: usize = 148;

fn encoded_dds_len(w: u32, h: u32) -> usize {
    let (tw, th) = crate::texprofile::bc7_target_size(w, h, super::BVW_TEXTURE_MAX);
    let mips = crate::bc7_pure::compute_default_mip_count(tw, th);
    DDS_HEADER_LEN + crate::bc7_pure::compute_mip_chain_size(tw, th, mips)
}

fn keep_raw(w: u32, h: u32, raw_len: usize) -> bool {
    w <= 2 || h <= 2 || encoded_dds_len(w, h) >= raw_len
}

fn encode_dds_bc7(rgba: &RgbaImage, srgb: bool, is_normal: bool) -> Result<Vec<u8>> {
    let (w, h) = rgba.dimensions();
    let (tw, th) = crate::texprofile::bc7_target_size(w, h, super::BVW_TEXTURE_MAX);
    let mut buf = if (tw, th) != (w, h) {
        crate::resize::box_downscale_rgba(
            rgba.as_raw(),
            w as usize,
            h as usize,
            tw as usize,
            th as usize,
            srgb,
        )
    } else {
        rgba.as_raw().clone()
    };
    if !is_normal && buf.iter().skip(3).step_by(4).any(|&a| a < 255) {
        crate::alpha_bleed::alpha_bleed_inplace(&mut buf, tw, th);
    }
    let perceptual = srgb && !is_normal;
    let (data, mips) = crate::bc7_pure::encode_bc7_mip_chain_with_profile(
        &buf,
        tw,
        th,
        None,
        false,
        srgb,
        perceptual,
        crate::bc7_pure::Bc7Profile::Basic,
    );
    let mut dds = ddsfile::Dds::new_dxgi(ddsfile::NewDxgiParams {
        height: th,
        width: tw,
        depth: None,
        format: ddsfile::DxgiFormat::BC7_UNorm,
        mipmap_levels: Some(mips as u32),
        array_layers: None,
        caps2: Some(ddsfile::Caps2::empty()),
        is_cubemap: false,
        resource_dimension: ddsfile::D3D10ResourceDimension::Texture2D,
        alpha_mode: ddsfile::AlphaMode::PreMultiplied,
    })
    .map_err(|e| anyhow!("dds header: {e}"))?;
    dds.data = data;
    let mut out = Vec::new();
    dds.write(&mut out).map_err(|e| anyhow!("dds write: {e}"))?;
    Ok(out)
}

#[derive(Clone, Copy, PartialEq)]
enum Tc {
    Srgb,
    Linear,
    Normal,
}

fn classify(bytes: &[u8], ext: &str, resolve: ResolveUri, n_images: usize) -> Vec<Tc> {
    let mut out = vec![Tc::Srgb; n_images];
    let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::gltf::parse_classify(bytes, ext, resolve)
    }));
    let Ok(Ok(scene)) = parsed else {
        return out;
    };
    let mut srgb: HashSet<usize> = HashSet::new();
    let mut normal: HashSet<usize> = HashSet::new();
    let mut linear: HashSet<usize> = HashSet::new();
    for m in &scene.materials {
        for t in [
            m.base_color_image,
            m.base_color_emit_image,
            m.emissive_image,
            m.spec_gloss_image,
            m.specular_color_image,
        ]
        .into_iter()
        .flatten()
        {
            srgb.insert(t.image);
        }
        if let Some(t) = m.normal_image {
            normal.insert(t.image);
        }
        for t in [
            m.metallic_roughness_image,
            m.metal_rough_emit_image,
            m.occlusion_image,
        ]
        .into_iter()
        .flatten()
        {
            linear.insert(t.image);
        }
    }
    for (i, tc) in out.iter_mut().enumerate() {
        *tc = if srgb.contains(&i) {
            Tc::Srgb
        } else if normal.contains(&i) {
            Tc::Normal
        } else if linear.contains(&i) {
            Tc::Linear
        } else {
            Tc::Srgb
        };
    }
    out
}

fn split_container(bytes: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
    if bytes.starts_with(b"glTF") {
        if bytes.len() < 12 {
            bail!("glb too short");
        }
        let mut pos = 12usize;
        let mut json: Option<Vec<u8>> = None;
        let mut bin: Vec<u8> = Vec::new();
        while pos + 8 <= bytes.len() {
            let clen = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
            let ctype = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap());
            let start = pos + 8;
            let end = start + clen;
            if end > bytes.len() {
                break;
            }
            if ctype == 0x4E4F_534A {
                json = Some(bytes[start..end].to_vec());
            } else if ctype == 0x004E_4942 {
                bin = bytes[start..end].to_vec();
            }
            pos = end;
        }
        Ok((json.ok_or_else(|| anyhow!("glb has no json chunk"))?, bin))
    } else if bytes.starts_with(b"{") {
        Ok((bytes.to_vec(), Vec::new()))
    } else {
        bail!("not a glb or gltf")
    }
}

pub(super) fn is_draco(prim: &mesh::Primitive) -> bool {
    prim.extensions
        .as_ref()
        .is_some_and(|e| e.others.contains_key("KHR_draco_mesh_compression"))
}

struct AccLayout {
    start: usize,
    stride: usize,
    count: usize,
}

fn acc_layout(root: &Root, ai: usize, elem: usize) -> Option<AccLayout> {
    let a = root.accessors.get(ai)?;
    if a.sparse.is_some() {
        return None;
    }
    let v = root.buffer_views.get(a.buffer_view?.value())?;
    let start =
        v.byte_offset.map_or(0, |o| o.0) as usize + a.byte_offset.map_or(0, |o| o.0) as usize;
    let stride = v.byte_stride.map_or(elem, |s| s.0);
    Some(AccLayout {
        start,
        stride,
        count: a.count.0 as usize,
    })
}

pub(super) fn acc_kind(root: &Root, ai: usize) -> Option<(ComponentType, Type, bool)> {
    let a = root.accessors.get(ai)?;
    let Checked::Valid(GenericComponentType(ct)) = a.component_type else {
        return None;
    };
    let Checked::Valid(ty) = a.type_ else {
        return None;
    };
    Some((ct, ty, a.normalized))
}

pub(super) fn read_f32_rows<const N: usize>(
    root: &Root,
    bin: &[u8],
    ai: usize,
) -> Option<Vec<[f32; N]>> {
    let (ct, ty, normalized) = acc_kind(root, ai)?;
    let ncomp = match ty {
        Type::Vec2 => 2,
        Type::Vec3 => 3,
        Type::Vec4 => 4,
        _ => return None,
    };
    if ncomp != N {
        return None;
    }
    let comp = match ct {
        ComponentType::F32 => 4,
        ComponentType::U8 | ComponentType::U16 if normalized => {
            if ct == ComponentType::U8 {
                1
            } else {
                2
            }
        }
        _ => return None,
    };
    let l = acc_layout(root, ai, comp * N)?;
    if l.count == 0 || l.start + l.stride * (l.count - 1) + comp * N > bin.len() {
        return None;
    }
    let mut out = Vec::with_capacity(l.count);
    for i in 0..l.count {
        let base = l.start + i * l.stride;
        let mut row = [0f32; N];
        for (c, slot) in row.iter_mut().enumerate() {
            let p = base + c * comp;
            *slot = match ct {
                ComponentType::F32 => f32::from_le_bytes(bin[p..p + 4].try_into().unwrap()),
                ComponentType::U8 => bin[p] as f32 / 255.0,
                ComponentType::U16 => {
                    u16::from_le_bytes(bin[p..p + 2].try_into().unwrap()) as f32 / 65535.0
                }
                _ => unreachable!(),
            };
        }
        out.push(row);
    }
    Some(out)
}

pub(super) fn read_indices(root: &Root, bin: &[u8], ai: usize) -> Option<Vec<u32>> {
    let (ct, ty, _) = acc_kind(root, ai)?;
    if ty != Type::Scalar {
        return None;
    }
    let comp = match ct {
        ComponentType::U8 => 1,
        ComponentType::U16 => 2,
        ComponentType::U32 => 4,
        _ => return None,
    };
    let l = acc_layout(root, ai, comp)?;
    if l.count == 0 || l.start + l.stride * (l.count - 1) + comp > bin.len() {
        return None;
    }
    let mut out = Vec::with_capacity(l.count);
    for i in 0..l.count {
        let p = l.start + i * l.stride;
        out.push(match ct {
            ComponentType::U8 => bin[p] as u32,
            ComponentType::U16 => u16::from_le_bytes(bin[p..p + 2].try_into().unwrap()) as u32,
            ComponentType::U32 => u32::from_le_bytes(bin[p..p + 4].try_into().unwrap()),
            _ => unreachable!(),
        });
    }
    Some(out)
}

pub(super) fn read_tight(root: &Root, bin: &[u8], ai: usize) -> Option<(Vec<u8>, usize)> {
    let (ct, ty, _) = acc_kind(root, ai)?;
    let comp = match ct {
        ComponentType::I8 | ComponentType::U8 => 1,
        ComponentType::I16 | ComponentType::U16 => 2,
        ComponentType::U32 | ComponentType::F32 => 4,
    };
    let ncomp = match ty {
        Type::Scalar => 1,
        Type::Vec2 => 2,
        Type::Vec3 => 3,
        Type::Vec4 => 4,
        Type::Mat2 => 4,
        Type::Mat3 => 9,
        Type::Mat4 => 16,
    };
    let elem = comp * ncomp;
    let l = acc_layout(root, ai, elem)?;
    if l.count == 0 || l.start + l.stride * (l.count - 1) + elem > bin.len() {
        return None;
    }
    let mut out = Vec::with_capacity(l.count * elem);
    for i in 0..l.count {
        let s = l.start + i * l.stride;
        out.extend_from_slice(&bin[s..s + elem]);
    }
    Some((out, elem))
}

fn renorm_weights_f32(root: &Root, bin: &mut [u8], ai: usize) {
    let Some((ComponentType::F32, Type::Vec4, _)) = acc_kind(root, ai) else {
        return;
    };
    let Some(l) = acc_layout(root, ai, 16) else {
        return;
    };
    if l.count == 0 || l.start + l.stride * (l.count - 1) + 16 > bin.len() {
        return;
    }
    for i in 0..l.count {
        let base = l.start + i * l.stride;
        let mut w = [0f32; 4];
        for (c, slot) in w.iter_mut().enumerate() {
            *slot = f32::from_le_bytes(bin[base + c * 4..base + c * 4 + 4].try_into().unwrap());
        }
        w.iter_mut().for_each(|x| *x = x.max(0.0));
        let sum: f32 = w.iter().sum();
        if sum == 0.0 {
            w[0] = 1.0;
        } else {
            let recip = sum.recip();
            for x in w.iter_mut() {
                *x *= recip;
            }
        }
        for (c, x) in w.iter().enumerate() {
            bin[base + c * 4..base + c * 4 + 4].copy_from_slice(&x.to_le_bytes());
        }
    }
}

struct MikktspaceGeometry<'a> {
    indices: Option<&'a [u32]>,
    positions: &'a [[f32; 3]],
    normals: &'a [[f32; 3]],
    uvs: &'a [[f32; 2]],
    tangents: Vec<[f32; 4]>,
}

impl MikktspaceGeometry<'_> {
    fn index(&self, face: usize, vert: usize) -> usize {
        let ii = face * 3 + vert;
        match self.indices {
            Some(ix) => ix[ii] as usize,
            None => ii,
        }
    }
}

impl bevy_mikktspace::Geometry for MikktspaceGeometry<'_> {
    fn num_faces(&self) -> usize {
        self.indices.map_or(self.positions.len(), <[u32]>::len) / 3
    }

    fn num_vertices_of_face(&self, _: usize) -> usize {
        3
    }

    fn position(&self, face: usize, vert: usize) -> [f32; 3] {
        self.positions[self.index(face, vert)]
    }

    fn normal(&self, face: usize, vert: usize) -> [f32; 3] {
        self.normals[self.index(face, vert)]
    }

    fn tex_coord(&self, face: usize, vert: usize) -> [f32; 2] {
        self.uvs[self.index(face, vert)]
    }

    fn set_tangent(
        &mut self,
        tangent_space: Option<bevy_mikktspace::TangentSpace>,
        face: usize,
        vert: usize,
    ) {
        let idx = self.index(face, vert);
        self.tangents[idx] = tangent_space.unwrap_or_default().tangent_encoded();
    }
}

fn mikktspace_tangents(
    positions: &[[f32; 3]],
    normals: &[[f32; 3]],
    uvs: &[[f32; 2]],
    indices: Option<&[u32]>,
) -> Option<Vec<[f32; 4]>> {
    if normals.len() != positions.len() || uvs.len() != positions.len() || positions.is_empty() {
        return None;
    }
    if let Some(ix) = indices {
        if ix.iter().any(|&i| i as usize >= positions.len()) {
            return None;
        }
    }
    let mut g = MikktspaceGeometry {
        indices,
        positions,
        normals,
        uvs,
        tangents: vec![[0.0; 4]; positions.len()],
    };
    bevy_mikktspace::generate_tangents(&mut g).ok()?;
    for t in &mut g.tangents {
        t[3] = -t[3];
    }
    Some(g.tangents)
}

fn attr(prim: &mesh::Primitive, sem: mesh::Semantic) -> Option<usize> {
    prim.attributes.get(&Checked::Valid(sem)).map(Index::value)
}

fn wants_tangents(root: &Root, prim: &mesh::Primitive) -> bool {
    if !matches!(prim.mode, Checked::Valid(mesh::Mode::Triangles)) || is_draco(prim) {
        return false;
    }
    let normal_mapped = prim
        .material
        .and_then(|m| root.materials.get(m.value()))
        .is_some_and(|m| m.normal_texture.is_some());
    normal_mapped
        && attr(prim, mesh::Semantic::Positions).is_some()
        && attr(prim, mesh::Semantic::Normals).is_some()
        && attr(prim, mesh::Semantic::TexCoords(0)).is_some()
        && attr(prim, mesh::Semantic::Tangents).is_none()
}

fn extract_image_bytes(
    img: &gltf_json::Image,
    views: &[buffer::View],
    bin: &[u8],
) -> Option<Vec<u8>> {
    if let Some(uri) = &img.uri {
        if uri.starts_with("data:") {
            return crate::gltf::decode_data_uri(uri);
        }
        return None;
    }
    let view = views.get(img.buffer_view?.value())?;
    let start = view.byte_offset.map_or(0, |o| o.0) as usize;
    let end = start + view.byte_length.0 as usize;
    (end <= bin.len()).then(|| bin[start..end].to_vec())
}

fn write_glb(root: &Root, binary_payload: &[u8]) -> Result<Vec<u8>> {
    let json_string =
        gltf_json::serialize::to_string(root).map_err(|e| anyhow!("gltf serialize: {e}"))?;
    let mut json_bytes = json_string.into_bytes();
    while !json_bytes.len().is_multiple_of(4) {
        json_bytes.push(0x20);
    }
    let mut bin_data = binary_payload.to_vec();
    while !bin_data.len().is_multiple_of(4) {
        bin_data.push(0x00);
    }
    let json_len = json_bytes.len() as u32;
    let bin_len = bin_data.len() as u32;
    let total_len = 12 + 8 + json_len + 8 + bin_len;
    let mut out = Vec::with_capacity(total_len as usize);
    out.write_all(b"glTF")?;
    out.write_all(&2u32.to_le_bytes())?;
    out.write_all(&total_len.to_le_bytes())?;
    out.write_all(&json_len.to_le_bytes())?;
    out.write_all(b"JSON")?;
    out.write_all(&json_bytes)?;
    out.write_all(&bin_len.to_le_bytes())?;
    out.write_all(b"BIN\0")?;
    out.write_all(&bin_data)?;
    Ok(out)
}

pub(crate) fn transform_glb(
    bytes: &[u8],
    ext: &str,
    resolve: ResolveUri,
    mesh_compress: bool,
) -> Result<Vec<u8>> {
    let (json_bytes, glb_bin) = split_container(bytes)?;
    let mut root: Root =
        gltf_json::deserialize::from_slice(&json_bytes).context("gltf json parse")?;
    let classes = classify(bytes, ext, resolve, root.images.len());

    let mut merged: Vec<u8> = Vec::new();
    let mut bases: Vec<u64> = Vec::with_capacity(root.buffers.len());
    for (bi, buf) in root.buffers.iter().enumerate() {
        let chunk: Vec<u8> = match &buf.uri {
            None => glb_bin.clone(),
            Some(uri) if uri.starts_with("data:") => crate::gltf::decode_data_uri(uri)
                .ok_or_else(|| anyhow!("buffer[{bi}] bad data uri"))?,
            Some(uri) => resolve
                .and_then(|f| f(uri))
                .ok_or_else(|| anyhow!("buffer[{bi}] external uri {uri:?} unresolved"))?,
        };
        while !merged.len().is_multiple_of(4) {
            merged.push(0);
        }
        bases.push(merged.len() as u64);
        merged.extend_from_slice(&chunk);
    }
    for view in &mut root.buffer_views {
        let base = *bases
            .get(view.buffer.value())
            .ok_or_else(|| anyhow!("view buffer {} out of range", view.buffer.value()))?;
        view.byte_offset = Some(USize64(base + view.byte_offset.map_or(0, |o| o.0)));
        view.buffer = Index::new(0);
    }
    if root.buffers.is_empty() {
        root.buffers.push(buffer::Buffer {
            byte_length: USize64(0),
            name: None,
            uri: None,
            extensions: None,
            extras: Default::default(),
        });
    }
    root.buffers.truncate(1);
    root.buffers[0].uri = None;

    let mut weight_accessors: BTreeSet<usize> = BTreeSet::new();
    let mut tangent_jobs: Vec<(usize, usize, Vec<[f32; 4]>)> = Vec::new();
    for (mi, m) in root.meshes.iter().enumerate() {
        for (pi, prim) in m.primitives.iter().enumerate() {
            if is_draco(prim) {
                continue;
            }
            for (sem, idx) in &prim.attributes {
                if let Checked::Valid(mesh::Semantic::Weights(_)) = sem {
                    weight_accessors.insert(idx.value());
                }
            }
            if !wants_tangents(&root, prim) {
                continue;
            }
            let read = || -> Option<Vec<[f32; 4]>> {
                let positions =
                    read_f32_rows::<3>(&root, &merged, attr(prim, mesh::Semantic::Positions)?)?;
                let normals =
                    read_f32_rows::<3>(&root, &merged, attr(prim, mesh::Semantic::Normals)?)?;
                let uvs =
                    read_f32_rows::<2>(&root, &merged, attr(prim, mesh::Semantic::TexCoords(0))?)?;
                let indices = match prim.indices {
                    Some(a) => Some(read_indices(&root, &merged, a.value())?),
                    None => None,
                };
                mikktspace_tangents(&positions, &normals, &uvs, indices.as_deref())
            };
            if let Some(tangents) = read() {
                tangent_jobs.push((mi, pi, tangents));
            }
        }
    }
    for ai in &weight_accessors {
        renorm_weights_f32(&root, &mut merged, *ai);
    }

    let mut zombie = vec![false; root.buffer_views.len()];
    let mut plans: Vec<Option<(RgbaImage, bool, bool)>> = Vec::with_capacity(root.images.len());
    for (i, image) in root.images.iter().enumerate() {
        match extract_image_bytes(image, &root.buffer_views, &merged) {
            None => plans.push(None),
            Some(src) => {
                let img =
                    image::load_from_memory(&src).map_err(|e| anyhow!("decode image[{i}]: {e}"))?;
                if keep_raw(img.width(), img.height(), src.len()) {
                    plans.push(None);
                } else {
                    let class = classes.get(i).copied().unwrap_or(Tc::Srgb);
                    plans.push(Some((
                        img.to_rgba8(),
                        class == Tc::Srgb,
                        class == Tc::Normal,
                    )));
                    if let Some(v) = image.buffer_view {
                        zombie[v.value()] = true;
                    }
                }
            }
        }
    }

    let mesh_plan = if mesh_compress {
        super::meshcomp::plan(&root, &merged)
    } else {
        None
    };
    if let Some(p) = &mesh_plan {
        for &vi in &p.zombies {
            zombie[vi] = true;
        }
    }

    let mut new_bin: Vec<u8> = Vec::new();
    for (i, view) in root.buffer_views.iter_mut().enumerate() {
        if zombie[i] {
            view.byte_length = USize64(0);
            view.byte_offset = Some(USize64(0));
        } else {
            let start = view.byte_offset.map_or(0, |o| o.0) as usize;
            let len = view.byte_length.0 as usize;
            while !new_bin.len().is_multiple_of(4) {
                new_bin.push(0);
            }
            let offset = new_bin.len() as u64;
            if merged.len() >= start + len {
                new_bin.extend_from_slice(&merged[start..start + len]);
            } else {
                new_bin.extend(std::iter::repeat_n(0u8, len));
            }
            view.byte_offset = Some(USize64(offset));
        }
    }

    for (i, plan) in plans.into_iter().enumerate() {
        let Some((rgba, srgb, is_normal)) = plan else {
            continue;
        };
        let dds =
            encode_dds_bc7(&rgba, srgb, is_normal).with_context(|| format!("encode image[{i}]"))?;
        debug_assert_eq!(dds.len(), encoded_dds_len(rgba.width(), rgba.height()));
        while !new_bin.len().is_multiple_of(4) {
            new_bin.push(0);
        }
        let offset = new_bin.len() as u64;
        new_bin.extend_from_slice(&dds);
        let vidx = root.buffer_views.len() as u32;
        root.buffer_views.push(buffer::View {
            buffer: Index::new(0),
            byte_length: USize64(dds.len() as u64),
            byte_offset: Some(USize64(offset)),
            byte_stride: None,
            name: Some("BC7_Data".into()),
            target: None,
            extensions: None,
            extras: Default::default(),
        });
        root.images[i].buffer_view = Some(Index::new(vidx));
        root.images[i].uri = None;
        root.images[i].mime_type = Some(gltf_json::image::MimeType("image/vnd-ms.dds".into()));
    }

    if let Some(p) = &mesh_plan {
        super::meshcomp::apply(&mut root, &mut new_bin, &p.streams);
    }

    for (mi, pi, tangents) in tangent_jobs {
        let comp = mesh_plan
            .as_ref()
            .filter(|p| p.eligible.contains(&(mi, pi)))
            .and_then(|_| super::meshcomp::tangent_stream(&tangents));
        let aidx = root.accessors.len() as u32;
        if let Some((cbytes, count)) = comp {
            let s = super::meshcomp::Stream {
                accessor: 0,
                bytes: cbytes,
                mode: "ATTRIBUTES",
                filter: Some("OCTAHEDRAL"),
                stride: 4,
                count,
                component_type: ComponentType::I8,
                type_: Type::Vec4,
                normalized: true,
            };
            let vidx = super::meshcomp::append_stream_view(&mut root, &mut new_bin, &s);
            root.accessors.push(gltf_json::Accessor {
                buffer_view: Some(Index::new(vidx)),
                byte_offset: Some(USize64(0)),
                count: USize64(count as u64),
                component_type: Checked::Valid(GenericComponentType(ComponentType::I8)),
                extensions: None,
                extras: Default::default(),
                type_: Checked::Valid(Type::Vec4),
                min: None,
                max: None,
                name: None,
                normalized: true,
                sparse: None,
            });
        } else {
            while !new_bin.len().is_multiple_of(4) {
                new_bin.push(0);
            }
            let offset = new_bin.len() as u64;
            for t in &tangents {
                for c in t {
                    new_bin.extend_from_slice(&c.to_le_bytes());
                }
            }
            let vidx = root.buffer_views.len() as u32;
            root.buffer_views.push(buffer::View {
                buffer: Index::new(0),
                byte_length: USize64(tangents.len() as u64 * 16),
                byte_offset: Some(USize64(offset)),
                byte_stride: None,
                name: None,
                target: None,
                extensions: None,
                extras: Default::default(),
            });
            root.accessors.push(gltf_json::Accessor {
                buffer_view: Some(Index::new(vidx)),
                byte_offset: Some(USize64(0)),
                count: USize64(tangents.len() as u64),
                component_type: Checked::Valid(GenericComponentType(ComponentType::F32)),
                extensions: None,
                extras: Default::default(),
                type_: Checked::Valid(Type::Vec4),
                min: None,
                max: None,
                name: None,
                normalized: false,
                sparse: None,
            });
        }
        root.meshes[mi].primitives[pi]
            .attributes
            .insert(Checked::Valid(mesh::Semantic::Tangents), Index::new(aidx));
    }

    root.buffers[0].byte_length = USize64(new_bin.len() as u64);
    write_glb(&root, &new_bin)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) fn png_bytes(w: u32, h: u32, px: [u8; 4]) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba(px));
        let mut out = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    pub(crate) fn noise_png_bytes(w: u32, h: u32) -> Vec<u8> {
        let mut s = 0x2545_f491u32;
        let mut img = image::RgbaImage::new(w, h);
        for p in img.pixels_mut() {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let b = s.to_le_bytes();
            *p = image::Rgba([b[0], b[1], b[2], 255]);
        }
        let mut out = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    pub(crate) fn mk_glb(mut gltf: serde_json::Value, bin: &[u8]) -> Vec<u8> {
        gltf["asset"] = json!({"version": "2.0"});
        if !bin.is_empty() {
            gltf["buffers"] = json!([{ "byteLength": bin.len() }]);
        }
        let mut json_chunk = serde_json::to_vec(&gltf).unwrap();
        while !json_chunk.len().is_multiple_of(4) {
            json_chunk.push(b' ');
        }
        let mut bin_chunk = bin.to_vec();
        while !bin_chunk.len().is_multiple_of(4) {
            bin_chunk.push(0);
        }
        let total = 12 + 8 + json_chunk.len() + 8 + bin_chunk.len();
        let mut glb: Vec<u8> = Vec::new();
        glb.extend_from_slice(b"glTF");
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(&(total as u32).to_le_bytes());
        glb.extend_from_slice(&(json_chunk.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"JSON");
        glb.extend_from_slice(&json_chunk);
        glb.extend_from_slice(&(bin_chunk.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"BIN\0");
        glb.extend_from_slice(&bin_chunk);
        glb
    }

    pub(crate) fn out_root_and_bin(out: &[u8]) -> (Root, Vec<u8>) {
        assert_eq!(&out[..4], b"glTF");
        assert_eq!(u32::from_le_bytes(out[4..8].try_into().unwrap()), 2);
        assert_eq!(
            u32::from_le_bytes(out[8..12].try_into().unwrap()) as usize,
            out.len()
        );
        let (json_bytes, bin) = split_container(out).unwrap();
        (
            gltf_json::deserialize::from_slice(&json_bytes).unwrap(),
            bin,
        )
    }

    fn dds_of(root: &Root, bin: &[u8], image_idx: usize) -> ddsfile::Dds {
        let img = &root.images[image_idx];
        assert_eq!(
            img.mime_type.as_ref().map(|m| m.0.as_str()),
            Some("image/vnd-ms.dds")
        );
        assert!(img.uri.is_none());
        let view = &root.buffer_views[img.buffer_view.unwrap().value()];
        assert_eq!(view.name.as_deref(), Some("BC7_Data"));
        let start = view.byte_offset.unwrap().0 as usize;
        let end = start + view.byte_length.0 as usize;
        ddsfile::Dds::read(std::io::Cursor::new(&bin[start..end])).unwrap()
    }

    #[test]
    fn embedded_png_becomes_dds_bc7_with_full_mips() {
        let png = noise_png_bytes(16, 16);
        let gltf = json!({
            "images": [{"bufferView": 0, "mimeType": "image/png"}],
            "textures": [{"source": 0}],
            "materials": [{"pbrMetallicRoughness": {"baseColorTexture": {"index": 0}}}],
            "bufferViews": [{"buffer": 0, "byteOffset": 0, "byteLength": png.len()}]
        });
        let glb = mk_glb(gltf, &png);
        let out = transform_glb(&glb, ".glb", None, true).unwrap();
        let (root, bin) = out_root_and_bin(&out);
        let dds = dds_of(&root, &bin, 0);
        assert_eq!((dds.get_width(), dds.get_height()), (16, 16));
        assert_eq!(dds.get_num_mipmap_levels(), 5);
        assert_eq!(dds.get_dxgi_format(), Some(ddsfile::DxgiFormat::BC7_UNorm));
        assert_eq!(
            dds.data.len(),
            crate::bc7_pure::compute_mip_chain_size(16, 16, 5)
        );
        let zombie = &root.buffer_views[0];
        assert_eq!(zombie.byte_length.0, 0);
        assert_eq!(zombie.byte_offset.map(|o| o.0), Some(0));
    }

    #[test]
    fn compressible_image_stays_raw_when_encode_is_larger() {
        let flat = png_bytes(64, 64, [90, 120, 30, 255]);
        assert_eq!(transform_img(&flat).unwrap(), flat);
        let noisy = noise_png_bytes(16, 16);
        let mut bin = flat.clone();
        while !bin.len().is_multiple_of(4) {
            bin.push(0);
        }
        let noisy_off = bin.len();
        bin.extend_from_slice(&noisy);
        let gltf = json!({
            "images": [
                {"bufferView": 0, "mimeType": "image/png"},
                {"bufferView": 1, "mimeType": "image/png"}
            ],
            "textures": [{"source": 0}, {"source": 1}],
            "materials": [
                {"pbrMetallicRoughness": {"baseColorTexture": {"index": 0}}},
                {"pbrMetallicRoughness": {"baseColorTexture": {"index": 1}}}
            ],
            "bufferViews": [
                {"buffer": 0, "byteOffset": 0, "byteLength": flat.len()},
                {"buffer": 0, "byteOffset": noisy_off, "byteLength": noisy.len()}
            ]
        });
        let out = transform_glb(&mk_glb(gltf, &bin), ".glb", None, true).unwrap();
        let (root, obin) = out_root_and_bin(&out);
        assert_eq!(
            root.images[0].mime_type.as_ref().map(|m| m.0.as_str()),
            Some("image/png")
        );
        let view = &root.buffer_views[root.images[0].buffer_view.unwrap().value()];
        let start = view.byte_offset.unwrap().0 as usize;
        assert_eq!(&obin[start..start + flat.len()], &flat[..]);
        let dds = dds_of(&root, &obin, 1);
        assert_eq!((dds.get_width(), dds.get_height()), (16, 16));
    }

    #[test]
    fn encoded_dds_len_matches_encoder_output() {
        for (w, h) in [(8u32, 8u32), (16, 8)] {
            let img = image::load_from_memory(&noise_png_bytes(w, h)).unwrap();
            let dds = encode_dds_bc7(&img.to_rgba8(), true, false).unwrap();
            assert_eq!(dds.len(), encoded_dds_len(w, h));
        }
        assert_eq!(
            encoded_dds_len(1500, 500),
            DDS_HEADER_LEN + crate::bc7_pure::compute_mip_chain_size(1024, 256, 11)
        );
    }

    #[test]
    fn oversize_png_is_capped_to_1024_with_npot_snap() {
        let png = noise_png_bytes(1500, 500);
        let gltf = json!({
            "images": [{"bufferView": 0, "mimeType": "image/png"}],
            "bufferViews": [{"buffer": 0, "byteOffset": 0, "byteLength": png.len()}]
        });
        let out = transform_glb(&mk_glb(gltf, &png), ".glb", None, true).unwrap();
        let (root, bin) = out_root_and_bin(&out);
        let dds = dds_of(&root, &bin, 0);
        assert_eq!((dds.get_width(), dds.get_height()), (1024, 256));
        assert_eq!(dds.get_num_mipmap_levels(), 11);
    }

    #[test]
    fn tiny_embedded_image_passes_through_untouched() {
        let png = png_bytes(2, 2, [1, 2, 3, 255]);
        let gltf = json!({
            "images": [{"bufferView": 0, "mimeType": "image/png"}],
            "bufferViews": [{"buffer": 0, "byteOffset": 0, "byteLength": png.len()}]
        });
        let out = transform_glb(&mk_glb(gltf, &png), ".glb", None, true).unwrap();
        let (root, bin) = out_root_and_bin(&out);
        let img = &root.images[0];
        assert_eq!(
            img.mime_type.as_ref().map(|m| m.0.as_str()),
            Some("image/png")
        );
        let view = &root.buffer_views[img.buffer_view.unwrap().value()];
        let start = view.byte_offset.unwrap().0 as usize;
        assert_eq!(&bin[start..start + png.len()], &png[..]);
    }

    #[test]
    fn corrupt_embedded_image_fails_the_transform() {
        let bad = b"not a png at all".to_vec();
        let gltf = json!({
            "images": [{"bufferView": 0, "mimeType": "image/png"}],
            "bufferViews": [{"buffer": 0, "byteOffset": 0, "byteLength": bad.len()}]
        });
        assert!(transform_glb(&mk_glb(gltf, &bad), ".glb", None, true).is_err());
    }

    pub(crate) fn quad_bin_and_views(
        png: &[u8],
    ) -> (Vec<u8>, serde_json::Value, serde_json::Value) {
        let mut bin: Vec<u8> = Vec::new();
        let positions: [[f32; 3]; 4] = [
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [1.0, 1.0, 0.0],
            [0.0, 1.0, 0.0],
        ];
        let normals = [[0.0f32, 0.0, 1.0]; 4];
        let uvs: [[f32; 2]; 4] = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
        let weights: [[f32; 4]; 4] = [
            [2.0, 2.0, 0.0, 0.0],
            [0.5, 0.25, 0.0, 0.0],
            [-1.0, -2.0, 0.0, 0.0],
            [0.3, 0.3, 0.3, 0.3],
        ];
        let joints: [[u16; 4]; 4] = [[0, 1, 0, 0]; 4];
        let indices: [u16; 6] = [0, 1, 2, 0, 2, 3];
        let pos_off = bin.len();
        for v in positions.iter().flatten() {
            bin.extend_from_slice(&v.to_le_bytes());
        }
        let nrm_off = bin.len();
        for v in normals.iter().flatten() {
            bin.extend_from_slice(&v.to_le_bytes());
        }
        let uv_off = bin.len();
        for v in uvs.iter().flatten() {
            bin.extend_from_slice(&v.to_le_bytes());
        }
        let w_off = bin.len();
        for v in weights.iter().flatten() {
            bin.extend_from_slice(&v.to_le_bytes());
        }
        let j_off = bin.len();
        for v in joints.iter().flatten() {
            bin.extend_from_slice(&v.to_le_bytes());
        }
        let idx_off = bin.len();
        for v in indices.iter() {
            bin.extend_from_slice(&v.to_le_bytes());
        }
        let png_off = bin.len();
        bin.extend_from_slice(png);
        let views = json!([
            {"buffer": 0, "byteOffset": pos_off, "byteLength": 48},
            {"buffer": 0, "byteOffset": nrm_off, "byteLength": 48},
            {"buffer": 0, "byteOffset": uv_off, "byteLength": 32},
            {"buffer": 0, "byteOffset": w_off, "byteLength": 64},
            {"buffer": 0, "byteOffset": j_off, "byteLength": 32},
            {"buffer": 0, "byteOffset": idx_off, "byteLength": 12},
            {"buffer": 0, "byteOffset": png_off, "byteLength": png.len()}
        ]);
        let accessors = json!([
            {"bufferView": 0, "componentType": 5126, "count": 4, "type": "VEC3"},
            {"bufferView": 1, "componentType": 5126, "count": 4, "type": "VEC3"},
            {"bufferView": 2, "componentType": 5126, "count": 4, "type": "VEC2"},
            {"bufferView": 3, "componentType": 5126, "count": 4, "type": "VEC4"},
            {"bufferView": 4, "componentType": 5123, "count": 4, "type": "VEC4"},
            {"bufferView": 5, "componentType": 5123, "count": 6, "type": "SCALAR"}
        ]);
        (bin, views, accessors)
    }

    #[test]
    fn tangents_and_weight_renorm_on_normal_mapped_skinned_quad() {
        let png = noise_png_bytes(16, 16);
        let (bin, views, accessors) = quad_bin_and_views(&png);
        let gltf = json!({
            "scene": 0,
            "scenes": [{"nodes": [0]}],
            "nodes": [{"mesh": 0}],
            "meshes": [{"primitives": [{
                "attributes": {"POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2,
                               "WEIGHTS_0": 3, "JOINTS_0": 4},
                "indices": 5, "material": 0
            }]}],
            "materials": [{"normalTexture": {"index": 0}}],
            "textures": [{"source": 0}],
            "images": [{"bufferView": 6, "mimeType": "image/png"}],
            "bufferViews": views,
            "accessors": accessors
        });
        let out = transform_glb(&mk_glb(gltf, &bin), ".glb", None, false).unwrap();
        let (root, obin) = out_root_and_bin(&out);
        let prim = &root.meshes[0].primitives[0];
        let tai = prim
            .attributes
            .get(&Checked::Valid(mesh::Semantic::Tangents))
            .expect("tangent attribute added")
            .value();
        let tangents = read_f32_rows::<4>(&root, &obin, tai).unwrap();
        assert_eq!(tangents.len(), 4);
        for t in &tangents {
            let len = (t[0] * t[0] + t[1] * t[1] + t[2] * t[2]).sqrt();
            assert!((len - 1.0).abs() < 1e-3, "{t:?}");
            assert!(t[3].abs() == 1.0, "{t:?}");
        }
        let wai = prim
            .attributes
            .get(&Checked::Valid(mesh::Semantic::Weights(0)))
            .unwrap()
            .value();
        let weights = read_f32_rows::<4>(&root, &obin, wai).unwrap();
        for row in &weights {
            let sum: f32 = row.iter().sum();
            assert!((sum - 1.0).abs() < 1e-4, "{row:?}");
            assert!(row.iter().all(|w| *w >= 0.0), "{row:?}");
        }
        assert_eq!(weights[2][0], 1.0);
        let normal_dds = dds_of(&root, &obin, 0);
        assert_eq!(
            normal_dds.get_dxgi_format(),
            Some(ddsfile::DxgiFormat::BC7_UNorm)
        );
    }

    #[test]
    fn draco_primitive_keeps_extension_and_gets_no_mesh_passes() {
        let png = noise_png_bytes(16, 16);
        let (bin, views, accessors) = quad_bin_and_views(&png);
        let gltf = json!({
            "extensionsUsed": ["KHR_draco_mesh_compression"],
            "meshes": [{"primitives": [{
                "attributes": {"POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2,
                               "WEIGHTS_0": 3, "JOINTS_0": 4},
                "indices": 5, "material": 0,
                "extensions": {"KHR_draco_mesh_compression":
                    {"bufferView": 5, "attributes": {"POSITION": 0}}}
            }]}],
            "materials": [{"normalTexture": {"index": 0}}],
            "textures": [{"source": 0}],
            "images": [{"bufferView": 6, "mimeType": "image/png"}],
            "bufferViews": views,
            "accessors": accessors
        });
        let out = transform_glb(&mk_glb(gltf, &bin), ".glb", None, true).unwrap();
        let (root, obin) = out_root_and_bin(&out);
        let prim = &root.meshes[0].primitives[0];
        assert!(!prim
            .attributes
            .contains_key(&Checked::Valid(mesh::Semantic::Tangents)));
        let ext = prim.extensions.as_ref().unwrap();
        assert_eq!(
            ext.others.get("KHR_draco_mesh_compression"),
            Some(&json!({"bufferView": 5, "attributes": {"POSITION": 0}}))
        );
        let wai = prim
            .attributes
            .get(&Checked::Valid(mesh::Semantic::Weights(0)))
            .unwrap()
            .value();
        let weights = read_f32_rows::<4>(&root, &obin, wai).unwrap();
        assert_eq!(weights[0], [2.0, 2.0, 0.0, 0.0]);
        dds_of(&root, &obin, 0);
    }

    #[test]
    fn gltf_external_image_uri_is_left_referenced() {
        let ext_bin: Vec<u8> = vec![7u8; 42];
        let gltf = json!({
            "asset": {"version": "2.0"},
            "images": [{"uri": "textures/wall.png"}],
            "buffers": [{"byteLength": 42, "uri": "geo.bin"}],
            "bufferViews": [{"buffer": 0, "byteOffset": 0, "byteLength": 42}]
        });
        let bytes = serde_json::to_vec(&gltf).unwrap();
        let resolve =
            |uri: &str| -> Option<Vec<u8>> { (uri == "geo.bin").then(|| ext_bin.clone()) };
        let out = transform_glb(&bytes, ".gltf", Some(&resolve), true).unwrap();
        let (root, obin) = out_root_and_bin(&out);
        assert_eq!(root.images[0].uri.as_deref(), Some("textures/wall.png"));
        assert!(root.images[0].buffer_view.is_none());
        assert_eq!(root.buffers.len(), 1);
        assert!(root.buffers[0].uri.is_none());
        let view = &root.buffer_views[0];
        let start = view.byte_offset.unwrap().0 as usize;
        assert_eq!(&obin[start..start + 42], &ext_bin[..]);
    }

    #[test]
    fn transform_img_tiny_and_corrupt_and_normal_paths() {
        let tiny = png_bytes(2, 2, [9, 9, 9, 9]);
        assert_eq!(transform_img(&tiny).unwrap(), tiny);
        assert!(transform_img(b"garbage").is_err());
        let big = noise_png_bytes(16, 8);
        assert!(encoded_dds_len(16, 8) < big.len());
        let dds_bytes = transform_img(&big).unwrap();
        let dds = ddsfile::Dds::read(std::io::Cursor::new(&dds_bytes[..])).unwrap();
        assert_eq!((dds.get_width(), dds.get_height()), (16, 8));
        assert_eq!(dds.get_num_mipmap_levels(), 5);
        assert_eq!(dds.get_dxgi_format(), Some(ddsfile::DxgiFormat::BC7_UNorm));
    }
}

use super::pack::canvas_size;
use super::tile::{srgb_decode, srgb_encode};
use super::*;
use crate::bc7_pure::{linear_to_srgb_u8, srgb_to_linear_u8};

fn png_bytes(img: &RgbaImage) -> Vec<u8> {
    let mut cur = std::io::Cursor::new(Vec::new());
    img.write_to(&mut cur, image::ImageFormat::Png).unwrap();
    cur.into_inner()
}

fn flat_image(w: u32, h: u32, color: [u8; 4]) -> Vec<u8> {
    let mut img = RgbaImage::new(w, h);
    for p in img.pixels_mut() {
        *p = image::Rgba(color);
    }
    png_bytes(&img)
}

fn quad_image() -> (Vec<u8>, [[u8; 4]; 4]) {
    let colors = [
        [255, 0, 0, 255],
        [0, 255, 0, 255],
        [0, 0, 255, 255],
        [255, 255, 0, 255],
    ];
    let mut img = RgbaImage::new(2, 2);
    img.put_pixel(0, 0, image::Rgba(colors[0]));
    img.put_pixel(1, 0, image::Rgba(colors[1]));
    img.put_pixel(0, 1, image::Rgba(colors[2]));
    img.put_pixel(1, 1, image::Rgba(colors[3]));
    (png_bytes(&img), colors)
}

fn mat(name: &str, class: AlphaClass, base_color: [f64; 4], image: Option<usize>) -> LodMaterial {
    LodMaterial {
        name: name.to_string(),
        class,
        base_color,
        cutoff: 0.5,
        image,
        double_sided: false,
    }
}

fn prim(material: usize, uvs: Vec<[f32; 2]>) -> LodPrimitive {
    let n = uvs.len();
    let positions = (0..n).map(|i| [i as f32, 0.0, 0.0]).collect();
    let normals = vec![[0.0, 0.0, 1.0]; n];
    let mut indices = Vec::new();
    for i in 2..n {
        indices.extend_from_slice(&[0, (i - 1) as u32, i as u32]);
    }
    LodPrimitive {
        positions,
        normals,
        uvs,
        indices,
        material,
        ..Default::default()
    }
}

fn model_of(
    materials: Vec<LodMaterial>,
    primitives: Vec<LodPrimitive>,
    images: Vec<Vec<u8>>,
) -> LodModel {
    LodModel {
        root_name: "root".to_string(),
        primitives,
        materials,
        images: images
            .into_iter()
            .map(|bytes| LodImage {
                bytes,
                mime: "image/png".to_string(),
            })
            .collect(),
        log: Vec::new(),
    }
}

fn decode(img: &LodImage) -> RgbaImage {
    image::load_from_memory(&img.bytes).unwrap().to_rgba8()
}

fn tri_uvs() -> Vec<[f32; 2]> {
    vec![[0.0, 0.0], [1.0, 1.0], [0.0, 1.0]]
}

fn log_line<'a>(m: &'a LodModel, tag: &str) -> &'a str {
    m.log
        .iter()
        .find(|l| l.contains(tag))
        .unwrap_or_else(|| panic!("no log line containing {tag:?}: {:?}", m.log))
}

#[test]
fn skyline_non_overlap_with_padding() {
    let padding = 2u32;
    let dims: Vec<(u32, u32)> = vec![
        (30, 40),
        (30, 40),
        (64, 10),
        (10, 64),
        (25, 25),
        (25, 25),
        (25, 25),
        (7, 50),
        (50, 7),
        (12, 12),
    ];
    let sizes: Vec<(u32, u32)> = dims
        .iter()
        .map(|&(w, h)| (w + 2 * padding, h + 2 * padding))
        .collect();
    let order: Vec<usize> = (0..sizes.len()).collect();
    let canvas = 128u32;
    let pos = pack_skyline(&sizes, &order, canvas).unwrap();
    for (i, (&(x, y), &(w, h))) in pos.iter().zip(sizes.iter()).enumerate() {
        assert!(x + w <= canvas && y + h <= canvas, "tile {i} out of bounds");
    }
    for a in 0..sizes.len() {
        for b in (a + 1)..sizes.len() {
            let (ax, ay) = pos[a];
            let (aw, ah) = sizes[a];
            let (bx, by) = pos[b];
            let (bw, bh) = sizes[b];
            let disjoint = ax + aw <= bx || bx + bw <= ax || ay + ah <= by || by + bh <= ay;
            assert!(disjoint, "tiles {a} and {b} overlap incl padding");
        }
    }
}

#[test]
fn canvas_square_pot_within_max() {
    let png = flat_image(16, 16, [10, 20, 30, 255]);
    let m = model_of(
        vec![mat("a", AlphaClass::Mask, [1.0; 4], Some(0))],
        vec![prim(0, tri_uvs())],
        vec![png],
    );
    for max in [64u32, 256, 1024] {
        let out = atlas(&m, max, 2).unwrap();
        let img = decode(&out.images[0]);
        assert_eq!(img.width(), img.height());
        assert!(img.width().is_power_of_two());
        assert!(img.width() <= max);
    }
}

#[test]
fn end_to_end_padded_rects_disjoint() {
    let mut mats = Vec::new();
    let mut prims = Vec::new();
    let mut images = Vec::new();
    for i in 0..6u32 {
        images.push(flat_image(10 + i * 3, 16, [i as u8 * 40, 10, 200, 255]));
        mats.push(mat(
            &format!("m{i}"),
            AlphaClass::Mask,
            [1.0; 4],
            Some(i as usize),
        ));
        prims.push(prim(i as usize, tri_uvs()));
    }
    let m = model_of(mats, prims, images);
    let out = atlas(&m, 256, 2).unwrap();
    let s = decode(&out.images[0]).width() as f64;
    let merged = &out.primitives[0];
    let mut rects = Vec::new();
    for k in 0..6usize {
        let uv0 = merged.uvs[k * 3];
        let uv1 = merged.uvs[k * 3 + 1];
        let x = (uv0[0] as f64 * s).round() as i64;
        let y = (uv0[1] as f64 * s).round() as i64;
        let w = (uv1[0] as f64 * s).round() as i64 - x;
        let h = (uv1[1] as f64 * s).round() as i64 - y;
        assert!(w > 0 && h > 0);
        rects.push((x - 2, y - 2, w + 4, h + 4));
    }
    for &(x, y, w, h) in &rects {
        assert!(x >= 0 && y >= 0 && x + w <= s as i64 && y + h <= s as i64);
    }
    for a in 0..rects.len() {
        for b in (a + 1)..rects.len() {
            let (ax, ay, aw, ah) = rects[a];
            let (bx, by, bw, bh) = rects[b];
            let disjoint = ax + aw <= bx || bx + bw <= ax || ay + ah <= by || by + bh <= ay;
            assert!(disjoint, "padded rects {a} and {b} overlap");
        }
    }
}

#[test]
fn non_tiled_uv_remap_exact() {
    let png = flat_image(16, 16, [10, 20, 30, 255]);
    let m = model_of(
        vec![mat("a", AlphaClass::Opaque, [1.0; 4], Some(0))],
        vec![prim(0, vec![[0.0, 0.0], [1.0, 1.0], [0.5, 0.25]])],
        vec![png],
    );
    let out = atlas(&m, 1024, 2).unwrap();
    assert_eq!(decode(&out.images[0]).width(), 16);
    let uvs = &out.primitives[0].uvs;
    let expect = [[0.0, 0.0], [1.0, 1.0], [0.5, 0.25]];
    for (got, want) in uvs.iter().zip(expect.iter()) {
        for a in 0..2 {
            assert!(
                (got[a] as f64 - want[a]).abs() < 1e-12,
                "got {got:?} want {want:?}"
            );
        }
    }
}

#[test]
fn repeat_bake_2x2_pixels() {
    let (png, colors) = quad_image();
    let m = model_of(
        vec![mat("a", AlphaClass::Mask, [1.0; 4], Some(0))],
        vec![prim(
            0,
            vec![[0.0, 0.0], [2.0, 0.0], [0.0, 2.0], [2.0, 2.0]],
        )],
        vec![png],
    );
    let out = atlas(&m, 1024, 2).unwrap();
    let canvas = decode(&out.images[0]);
    let uv0 = out.primitives[0].uvs[0];
    let s = canvas.width() as f64;
    let rx = (uv0[0] as f64 * s).round() as u32;
    let ry = (uv0[1] as f64 * s).round() as u32;
    for py in 0..4u32 {
        for px in 0..4u32 {
            let want = colors[((py % 2) * 2 + px % 2) as usize];
            let got = canvas.get_pixel(rx + px, ry + py).0;
            assert_eq!(got, want, "baked pixel ({px},{py})");
        }
    }
    let uv3 = out.primitives[0].uvs[3];
    assert!(((uv3[0] as f64 * s).round() as u32) - rx == 4);
    assert!(((uv3[1] as f64 * s).round() as u32) - ry == 4);
}

#[test]
fn span_three_bakes_three_repeats() {
    let (png, colors) = quad_image();
    let m = model_of(
        vec![mat("a", AlphaClass::Mask, [1.0; 4], Some(0))],
        vec![prim(0, vec![[0.0, 0.0], [3.0, 0.0], [0.0, 3.0]])],
        vec![png],
    );
    let out = atlas_with(&m, 64, 2, AtlasMode::Native).unwrap();
    let line = log_line(&out, "class=mask");
    assert!(line.contains("fallbacks=0"), "{line}");
    assert!(line.contains("unique=1"), "{line}");
    let canvas = decode(&out.images[0]);
    let s = canvas.width() as f64;
    let uv0 = out.primitives[0].uvs[0];
    let rx = (uv0[0] as f64 * s).round() as u32;
    let ry = (uv0[1] as f64 * s).round() as u32;
    for py in 0..6u32 {
        for px in 0..6u32 {
            let want = colors[((py % 2) * 2 + px % 2) as usize];
            let got = canvas.get_pixel(rx + px, ry + py).0;
            assert_eq!(got, want, "baked pixel ({px},{py})");
        }
    }
}

fn fallback_uvs() -> Vec<[f32; 2]> {
    vec![[f32::NAN, f32::NAN]; 3]
}

#[test]
fn non_finite_uvs_fall_back_to_average_solid() {
    let (png, colors) = quad_image();
    let m = model_of(
        vec![mat("a", AlphaClass::Mask, [1.0; 4], Some(0))],
        vec![prim(0, fallback_uvs())],
        vec![png],
    );
    let out = atlas(&m, 64, 2).unwrap();
    let uvs = &out.primitives[0].uvs;
    assert_eq!(uvs[0], uvs[1]);
    assert_eq!(uvs[0], uvs[2]);
    let line = log_line(&out, "class=mask");
    assert!(line.contains("fallbacks=1"), "{line}");
    assert!(log_line(&out, "WARN fallback").contains("non-finite"));
    let canvas = decode(&out.images[0]);
    let s = canvas.width() as f64;
    let cx = (uvs[0][0] as f64 * s) as u32;
    let cy = (uvs[0][1] as f64 * s) as u32;
    let mut avg = [0f64; 3];
    for c in colors {
        for ch in 0..3 {
            avg[ch] += srgb_to_linear_u8(c[ch]) as f64 / 4.0;
        }
    }
    let mut want = [255u8; 4];
    for ch in 0..3 {
        want[ch] = linear_to_srgb_u8(avg[ch] as f32);
    }
    assert_eq!(canvas.get_pixel(cx, cy).0, want);
}

#[test]
fn fallback_average_ignores_invisible_texels() {
    let mut img = RgbaImage::new(16, 16);
    for p in img.pixels_mut() {
        *p = image::Rgba([0, 0, 0, 0]);
    }
    img.put_pixel(3, 3, image::Rgba([255, 255, 255, 255]));
    let m = model_of(
        vec![mat("a", AlphaClass::Blend, [1.0; 4], Some(0))],
        vec![prim(0, fallback_uvs())],
        vec![png_bytes(&img)],
    );
    let out = atlas(&m, 64, 2).unwrap();
    let canvas = decode(&out.images[0]);
    let s = canvas.width() as f64;
    let uv0 = out.primitives[0].uvs[0];
    let px = canvas
        .get_pixel((uv0[0] as f64 * s) as u32, (uv0[1] as f64 * s) as u32)
        .0;
    assert_eq!(&px[0..3], &[255, 255, 255], "ghost pane colour: {px:?}");
    assert_eq!(px[3], 1);
}

#[test]
fn fully_transparent_fallback_deterministic() {
    let build = || {
        let mut img = RgbaImage::new(8, 8);
        for (x, _y, p) in img.enumerate_pixels_mut() {
            let v = if x < 4 { 0 } else { 255 };
            *p = image::Rgba([v, v, v, 0]);
        }
        model_of(
            vec![mat("a", AlphaClass::Blend, [1.0; 4], Some(0))],
            vec![prim(0, fallback_uvs())],
            vec![png_bytes(&img)],
        )
    };
    let out1 = atlas(&build(), 64, 2).unwrap();
    let out2 = atlas(&build(), 64, 2).unwrap();
    assert_eq!(out1.images[0].bytes, out2.images[0].bytes);
    let canvas = decode(&out1.images[0]);
    let g = linear_to_srgb_u8(0.5);
    assert_eq!(canvas.get_pixel(1, 1).0, [g, g, g, 0]);
}

#[test]
fn blend_downscale_does_not_drag_hidden_rgb() {
    let mut img = RgbaImage::new(128, 64);
    for (x, _y, p) in img.enumerate_pixels_mut() {
        *p = if x < 64 {
            image::Rgba([255, 0, 0, 255])
        } else {
            image::Rgba([0, 255, 0, 0])
        };
    }
    let m = model_of(
        vec![mat("a", AlphaClass::Blend, [1.0; 4], Some(0))],
        vec![prim(0, tri_uvs())],
        vec![png_bytes(&img)],
    );
    let out = atlas(&m, 64, 2).unwrap();
    let canvas = decode(&out.images[0]);
    for p in canvas.pixels() {
        assert_eq!(p.0[1], 0, "hidden green leaked: {:?}", p.0);
        if p.0[3] > 0 {
            assert_eq!(p.0[0], 255, "visible red thinned: {:?}", p.0);
        }
    }
}

#[test]
fn srgb_roundtrip_all_bytes() {
    for v in 0u16..=255 {
        let enc = (srgb_encode(srgb_decode(v as f64 / 255.0)) * 255.0).round() as u16;
        assert_eq!(enc, v);
    }
}

#[test]
fn dedupe_same_bytes_and_tint() {
    let png = flat_image(8, 8, [1, 2, 3, 255]);
    let m = model_of(
        vec![
            mat("a", AlphaClass::Mask, [1.0; 4], Some(0)),
            mat("b", AlphaClass::Mask, [1.0; 4], Some(1)),
        ],
        vec![prim(0, tri_uvs()), prim(1, tri_uvs())],
        vec![png.clone(), png],
    );
    let out = atlas(&m, 1024, 2).unwrap();
    let line = log_line(&out, "class=mask");
    assert!(line.contains("refs=2"), "{line}");
    assert!(line.contains("unique=1"), "{line}");
    let p = &out.primitives[0];
    assert_eq!(p.positions.len(), 3);
    assert_eq!(p.indices, vec![0, 1, 2, 0, 1, 2]);
}

#[test]
fn distinct_tint_gets_distinct_tile() {
    let png = flat_image(8, 8, [255, 255, 255, 255]);
    let m = model_of(
        vec![
            mat("a", AlphaClass::Mask, [1.0; 4], Some(0)),
            mat("b", AlphaClass::Mask, [1.0, 0.0, 0.0, 1.0], Some(1)),
        ],
        vec![prim(0, tri_uvs()), prim(1, tri_uvs())],
        vec![png.clone(), png],
    );
    let out = atlas(&m, 64, 2).unwrap();
    let line = log_line(&out, "class=mask");
    assert!(line.contains("refs=2"), "{line}");
    assert!(line.contains("unique=2"), "{line}");
    assert_ne!(out.primitives[0].uvs[0], out.primitives[0].uvs[3]);
}

#[test]
fn tint_bake_red_on_white() {
    let png = flat_image(8, 8, [255, 255, 255, 255]);
    let m = model_of(
        vec![mat("a", AlphaClass::Mask, [1.0, 0.0, 0.0, 1.0], Some(0))],
        vec![prim(0, tri_uvs())],
        vec![png],
    );
    let out = atlas(&m, 1024, 2).unwrap();
    assert_eq!(out.materials[0].base_color, [1.0, 1.0, 1.0, 1.0]);
    let canvas = decode(&out.images[0]);
    let s = canvas.width() as f64;
    let uv0 = out.primitives[0].uvs[0];
    let x = (uv0[0] as f64 * s).round() as u32;
    let y = (uv0[1] as f64 * s).round() as u32;
    assert_eq!(canvas.get_pixel(x + 1, y + 1).0, [255, 0, 0, 255]);
}

#[test]
fn untextured_solid_tile_color() {
    let m = model_of(
        vec![mat("a", AlphaClass::Blend, [0.0, 0.5, 1.0, 1.0], None)],
        vec![prim(0, tri_uvs())],
        vec![],
    );
    let out = atlas(&m, 64, 2).unwrap();
    let canvas = decode(&out.images[0]);
    let s = canvas.width() as f64;
    let uv0 = out.primitives[0].uvs[0];
    let x = (uv0[0] as f64 * s) as u32;
    let y = (uv0[1] as f64 * s) as u32;
    assert_eq!(canvas.get_pixel(x, y).0, [0, 188, 255, 255]);
    assert_eq!(out.primitives[0].uvs[0], out.primitives[0].uvs[1]);
}

#[test]
fn class_split_names_and_counts() {
    let png = flat_image(8, 8, [9, 9, 9, 255]);
    let m = model_of(
        vec![
            mat("o", AlphaClass::Opaque, [1.0; 4], Some(0)),
            mat("c", AlphaClass::Mask, [1.0; 4], Some(0)),
            mat("t", AlphaClass::Blend, [1.0; 4], None),
        ],
        vec![prim(0, tri_uvs()), prim(1, tri_uvs()), prim(2, tri_uvs())],
        vec![png],
    );
    let out = atlas(&m, 64, 2).unwrap();
    assert_eq!(out.primitives.len(), 3);
    assert_eq!(out.materials.len(), 3);
    assert_eq!(out.images.len(), 3);
    assert_eq!(out.materials[0].name, "TextureBakeResult-mat");
    assert_eq!(out.materials[0].class, AlphaClass::Opaque);
    assert_eq!(out.materials[1].name, "TextureBakeResult-mat-cutout");
    assert_eq!(out.materials[1].class, AlphaClass::Mask);
    assert_eq!(out.materials[1].cutoff, 0.5);
    assert_eq!(out.materials[2].name, "TextureBakeResult-mat-transparent");
    assert_eq!(out.materials[2].class, AlphaClass::Blend);
    for (i, om) in out.materials.iter().enumerate() {
        assert_eq!(om.image, Some(i));
        assert_eq!(om.base_color, [1.0, 1.0, 1.0, 1.0]);
        assert!(!om.double_sided);
    }
    assert_eq!(out.images[0].mime, "image/jpeg");
    assert_eq!(out.images[1].mime, "image/png");
    assert_eq!(out.images[2].mime, "image/png");
    let opaque_only = model_of(
        vec![mat("o", AlphaClass::Opaque, [1.0; 4], None)],
        vec![prim(0, tri_uvs())],
        vec![],
    );
    let out1 = atlas(&opaque_only, 64, 2).unwrap();
    assert_eq!(out1.materials.len(), 1);
    assert_eq!(out1.images.len(), 1);
    assert_eq!(out1.materials[0].name, "TextureBakeResult-mat");
}

#[test]
fn mimes_decode_back() {
    let png = flat_image(8, 8, [50, 100, 150, 255]);
    let m = model_of(
        vec![
            mat("o", AlphaClass::Opaque, [1.0; 4], Some(0)),
            mat("c", AlphaClass::Mask, [1.0; 4], Some(0)),
        ],
        vec![prim(0, tri_uvs()), prim(1, tri_uvs())],
        vec![png],
    );
    let out = atlas(&m, 1024, 2).unwrap();
    assert_eq!(out.images[0].mime, "image/png");
    assert_eq!(out.images[1].mime, "image/png");
    assert_eq!(&out.images[0].bytes[0..4], &[0x89, b'P', b'N', b'G']);
    let out = atlas(&m, 256, 2).unwrap();
    assert_eq!(out.images[0].mime, "image/jpeg");
    assert_eq!(out.images[1].mime, "image/png");
    assert_eq!(&out.images[0].bytes[0..3], &[0xFF, 0xD8, 0xFF]);
    assert_eq!(&out.images[1].bytes[0..4], &[0x89, b'P', b'N', b'G']);
    for img in &out.images {
        let d = decode(img);
        assert!(d.width() > 0);
    }
}

#[test]
fn per_class_merge_conserves_tris() {
    let png = flat_image(8, 8, [1, 1, 1, 255]);
    let m = model_of(
        vec![
            mat("o", AlphaClass::Opaque, [1.0; 4], Some(0)),
            mat("c", AlphaClass::Mask, [1.0; 4], Some(0)),
            mat("t", AlphaClass::Blend, [1.0; 4], None),
        ],
        vec![
            prim(0, tri_uvs()),
            prim(0, vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]]),
            prim(1, tri_uvs()),
            prim(2, tri_uvs()),
            prim(2, vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]]),
        ],
        vec![png],
    );
    let out = atlas(&m, 64, 2).unwrap();
    assert_eq!(out.primitives.len(), 3);
    assert_eq!(out.primitives[0].indices.len() / 3, 3);
    assert_eq!(out.primitives[1].indices.len() / 3, 1);
    assert_eq!(out.primitives[2].indices.len() / 3, 3);
    assert_eq!(out.total_tris(), m.total_tris());
    for p in &out.primitives {
        let max = *p.indices.iter().max().unwrap() as usize;
        assert!(max < p.positions.len());
        assert_eq!(p.positions.len(), p.normals.len());
        assert_eq!(p.positions.len(), p.uvs.len());
    }
}

#[test]
fn deterministic_output() {
    let build = || {
        let (png, _) = quad_image();
        let png2 = flat_image(20, 12, [3, 200, 3, 255]);
        model_of(
            vec![
                mat("o", AlphaClass::Opaque, [1.0; 4], Some(0)),
                mat("c", AlphaClass::Mask, [0.5, 1.0, 1.0, 1.0], Some(1)),
                mat("t", AlphaClass::Blend, [1.0, 1.0, 0.2, 0.7], None),
            ],
            vec![prim(0, tri_uvs()), prim(1, tri_uvs()), prim(2, tri_uvs())],
            vec![png, png2],
        )
    };
    let a = crate::lodgen::emit::emit_glb(&atlas(&build(), 128, 2).unwrap()).unwrap();
    let b = crate::lodgen::emit::emit_glb(&atlas(&build(), 128, 2).unwrap()).unwrap();
    assert_eq!(a, b);
}

#[test]
fn opaque_background_bleeds_tile_color() {
    let png = flat_image(8, 8, [87, 0, 204, 255]);
    let solid = flat_image(4, 4, [80, 10, 190, 255]);
    let m = model_of(
        vec![
            mat("a", AlphaClass::Opaque, [1.0; 4], Some(0)),
            mat("b", AlphaClass::Opaque, [1.0; 4], Some(1)),
        ],
        vec![prim(0, tri_uvs()), prim(1, tri_uvs())],
        vec![png, solid],
    );
    let out = atlas(&m, 32, 2).unwrap();
    let canvas = decode(&out.images[0]);
    assert_eq!(canvas.width(), 32);
    let mut acc = [0f64; 3];
    for p in canvas.pixels() {
        for ch in 0..3 {
            acc[ch] += p.0[ch] as f64;
        }
    }
    let n = (canvas.width() * canvas.height()) as f64;
    let avg = acc.map(|v| v / n);
    assert!(
        avg[2] > 150.0 && avg[0] > 40.0,
        "canvas average {avg:?} still dark"
    );
    for p in canvas.pixels() {
        let px = p.0;
        assert!(
            px[0] as u32 + px[1] as u32 + px[2] as u32 > 30,
            "near-black background pixel {px:?} survived bleed"
        );
    }
}

#[test]
fn full_bleed_solid_fills_canvas() {
    let m = model_of(
        vec![mat("a", AlphaClass::Blend, [0.2, 0.4, 0.6, 1.0], None)],
        vec![prim(0, tri_uvs())],
        vec![],
    );
    let out = atlas(&m, 64, 2).unwrap();
    let canvas = decode(&out.images[0]);
    assert_eq!(canvas.width(), 64);
    assert_eq!(canvas.height(), 64);
    for &(x, y) in &[(0u32, 0u32), (63, 0), (0, 63), (63, 63), (31, 31)] {
        let px = canvas.get_pixel(x, y).0;
        assert_eq!(&px[0..3], &[124, 170, 203], "pixel ({x},{y})");
    }
}

#[test]
fn full_bleed_single_image_tile() {
    let png = flat_image(32, 16, [10, 200, 30, 255]);
    let m = model_of(
        vec![mat("a", AlphaClass::Mask, [1.0; 4], Some(0))],
        vec![prim(0, tri_uvs())],
        vec![png],
    );
    let out = atlas(&m, 1024, 2).unwrap();
    let canvas = decode(&out.images[0]);
    assert_eq!(canvas.width(), 32);
    assert_eq!(canvas.height(), 32);
    let uvs = &out.primitives[0].uvs;
    assert_eq!(uvs[0], [0.0, 0.0]);
    assert_eq!(uvs[1], [1.0, 1.0]);
    assert_eq!(canvas.get_pixel(0, 0).0, [10, 200, 30, 255]);
    assert_eq!(canvas.get_pixel(31, 31).0, [10, 200, 30, 255]);
}

#[test]
fn pot_tile_does_not_double_canvas() {
    let png = flat_image(512, 512, [40, 80, 120, 255]);
    let m = model_of(
        vec![mat("a", AlphaClass::Mask, [1.0; 4], Some(0))],
        vec![prim(0, tri_uvs())],
        vec![png],
    );
    let out = atlas(&m, 512, 2).unwrap();
    let canvas = decode(&out.images[0]);
    assert_eq!(canvas.width(), 512);
    assert_eq!(canvas.height(), 512);
    let line = log_line(&out, "class=mask");
    assert!(line.contains("occupancy=100.0%"), "{line}");
}

#[test]
fn overflow_scales_to_fit() {
    let mut mats = Vec::new();
    let mut prims = Vec::new();
    let mut images = Vec::new();
    for i in 0..5u32 {
        images.push(flat_image(200, 200, [i as u8 * 30 + 10, 0, 100, 255]));
        mats.push(mat(
            &format!("m{i}"),
            AlphaClass::Mask,
            [1.0; 4],
            Some(i as usize),
        ));
        prims.push(prim(i as usize, tri_uvs()));
    }
    let m = model_of(mats, prims, images);
    let out = atlas(&m, 256, 2).unwrap();
    let canvas = decode(&out.images[0]);
    assert_eq!(canvas.width(), 256);
    assert_eq!(canvas.height(), 256);
    let line = log_line(&out, "class=mask");
    assert!(line.contains("size=256"), "{line}");
    let occ: f64 = line
        .split("occupancy=")
        .nth(1)
        .and_then(|s| s.split('%').next())
        .unwrap()
        .parse()
        .unwrap();
    assert!(occ >= 40.0, "occupancy {occ} below band: {line}");
    assert_eq!(out.total_tris(), m.total_tris());
}

#[test]
fn adaptive_single_solid_tile_is_8x8() {
    let m = model_of(
        vec![mat("a", AlphaClass::Blend, [0.2, 0.4, 0.6, 1.0], None)],
        vec![prim(0, tri_uvs())],
        vec![],
    );
    let out = atlas_with(&m, 512, 0, AtlasMode::Adaptive).unwrap();
    let canvas = decode(&out.images[0]);
    assert_eq!(canvas.width(), 8);
    assert_eq!(canvas.height(), 8);
    assert_eq!(&canvas.get_pixel(4, 4).0[0..3], &[124, 170, 203]);
    let fixed = atlas_with(&m, 512, 0, AtlasMode::FullBleed).unwrap();
    assert_eq!(decode(&fixed.images[0]).width(), 512);
}

#[test]
fn canvas_size_policy() {
    for extent in [1u32, 8, 40, 256, 600] {
        assert_eq!(canvas_size(AtlasMode::Native, 256, extent), 256);
        assert_eq!(canvas_size(AtlasMode::FullBleed, 512, extent), 512);
    }
    assert_eq!(canvas_size(AtlasMode::Adaptive, 512, 1), 8);
    assert_eq!(canvas_size(AtlasMode::Adaptive, 512, 40), 64);
    assert_eq!(canvas_size(AtlasMode::Adaptive, 512, 256), 256);
    assert_eq!(canvas_size(AtlasMode::Adaptive, 512, 600), 512);
    assert_eq!(budget_pot(256), 256);
    assert_eq!(budget_pot(300), 256);
    assert_eq!(budget_pot(512), 512);
    assert_eq!(budget_pot(1), 1);
}

#[test]
fn native_single_solid_fills_budget() {
    let m = model_of(
        vec![mat("a", AlphaClass::Blend, [0.2, 0.4, 0.6, 1.0], None)],
        vec![prim(0, tri_uvs())],
        vec![],
    );
    let out = atlas_with(&m, 256, 0, AtlasMode::Native).unwrap();
    let canvas = decode(&out.images[0]);
    assert_eq!(canvas.width(), 256);
    assert_eq!(canvas.height(), 256);
    for &(x, y) in &[(0u32, 0u32), (255, 0), (0, 255), (255, 255), (128, 128)] {
        assert_eq!(&canvas.get_pixel(x, y).0[0..3], &[124, 170, 203]);
    }
}

#[test]
fn native_small_image_pads_canvas_to_budget() {
    let png = flat_image(16, 16, [10, 200, 30, 255]);
    let m = model_of(
        vec![mat("a", AlphaClass::Mask, [1.0; 4], Some(0))],
        vec![prim(0, vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]])],
        vec![png],
    );
    let out = atlas_with(&m, 256, 0, AtlasMode::Native).unwrap();
    let canvas = decode(&out.images[0]);
    assert_eq!(canvas.width(), 256);
    assert_eq!(canvas.height(), 256);
    assert_eq!(canvas.get_pixel(0, 0).0, [10, 200, 30, 255]);
    let uvs = &out.primitives[0].uvs;
    assert!(uvs.iter().all(|uv| uv[0] <= 16.0 / 256.0 + 1e-6));
    assert!(uvs.iter().all(|uv| uv[1] <= 16.0 / 256.0 + 1e-6));
}

#[test]
fn native_all_classes_share_budget_size() {
    let png = flat_image(16, 16, [9, 9, 9, 255]);
    let m = model_of(
        vec![
            mat("o", AlphaClass::Opaque, [1.0; 4], Some(0)),
            mat("c", AlphaClass::Mask, [1.0; 4], Some(0)),
            mat("t", AlphaClass::Blend, [1.0; 4], None),
        ],
        vec![prim(0, tri_uvs()), prim(1, tri_uvs()), prim(2, tri_uvs())],
        vec![png],
    );
    let out = atlas_with(&m, 256, 0, AtlasMode::Native).unwrap();
    assert_eq!(out.images.len(), 3);
    for img in &out.images {
        let d = decode(img);
        assert_eq!((d.width(), d.height()), (256, 256));
    }
    let adaptive = atlas_with(&m, 256, 0, AtlasMode::Adaptive).unwrap();
    let dims: Vec<u32> = adaptive.images.iter().map(|i| decode(i).width()).collect();
    assert!(dims.iter().any(|&w| w < 256), "{dims:?}");
}

#[test]
fn native_multi_tile_canvas_stays_budget() {
    let mut mats = Vec::new();
    let mut prims = Vec::new();
    let mut images = Vec::new();
    for i in 0..3u32 {
        images.push(flat_image(16, 16, [i as u8 * 60 + 10, 20, 200, 255]));
        mats.push(mat(
            &format!("m{i}"),
            AlphaClass::Mask,
            [1.0; 4],
            Some(i as usize),
        ));
        prims.push(prim(i as usize, vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]]));
    }
    let m = model_of(mats, prims, images);
    let out = atlas_with(&m, 256, 0, AtlasMode::Native).unwrap();
    let canvas = decode(&out.images[0]);
    assert_eq!(canvas.width(), 256);
    assert_eq!(canvas.height(), 256);
}

#[test]
fn adaptive_crops_single_image_tile_to_uv_window() {
    let mut img = RgbaImage::new(64, 64);
    for (x, y, p) in img.enumerate_pixels_mut() {
        *p = image::Rgba([(x * 4) as u8, (y * 4) as u8, 7, 255]);
    }
    let png = png_bytes(&img);
    let m = model_of(
        vec![mat("a", AlphaClass::Mask, [1.0; 4], Some(0))],
        vec![prim(0, vec![[0.25, 0.25], [0.3, 0.25], [0.25, 0.3]])],
        vec![png],
    );
    let out = atlas_with(&m, 512, 0, AtlasMode::Adaptive).unwrap();
    let canvas = decode(&out.images[0]);
    assert_eq!(canvas.width(), 8);
    assert_eq!(canvas.height(), 8);
    assert_eq!(canvas.get_pixel(0, 0).0, [64, 64, 7, 255]);
    assert_eq!(canvas.get_pixel(7, 7).0, [92, 92, 7, 255]);
    let uv0 = out.primitives[0].uvs[0];
    assert!(
        (uv0[0] - 0.0).abs() < 1e-6 && (uv0[1] - 0.0).abs() < 1e-6,
        "{uv0:?}"
    );
    let uv1 = out.primitives[0].uvs[1];
    assert!((uv1[0] - 0.4).abs() < 1e-6, "{uv1:?}");
}

#[test]
fn adaptive_full_span_single_image_keeps_native_size() {
    let png = flat_image(32, 32, [10, 200, 30, 255]);
    let m = model_of(
        vec![mat("a", AlphaClass::Mask, [1.0; 4], Some(0))],
        vec![prim(0, vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]])],
        vec![png],
    );
    let out = atlas_with(&m, 512, 0, AtlasMode::Adaptive).unwrap();
    let canvas = decode(&out.images[0]);
    assert_eq!(canvas.width(), 32);
    assert_eq!(canvas.get_pixel(0, 0).0, [10, 200, 30, 255]);
}

#[test]
fn adaptive_canvas_shrinks_to_packed_extent() {
    let mut mats = Vec::new();
    let mut prims = Vec::new();
    let mut images = Vec::new();
    for i in 0..3u32 {
        images.push(flat_image(16, 16, [i as u8 * 60 + 10, 20, 200, 255]));
        mats.push(mat(
            &format!("m{i}"),
            AlphaClass::Mask,
            [1.0; 4],
            Some(i as usize),
        ));
        prims.push(prim(i as usize, vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]]));
    }
    let m = model_of(mats, prims, images);
    let out = atlas_with(&m, 512, 0, AtlasMode::Adaptive).unwrap();
    let canvas = decode(&out.images[0]);
    assert!(
        canvas.width() <= 64,
        "canvas {} did not shrink",
        canvas.width()
    );
    assert!(canvas.width() >= 16);
    assert!(canvas.width().is_power_of_two());
}

#[test]
fn weld_merges_one_ulp_normal_duplicates_only() {
    let mut p = LodPrimitive {
        positions: vec![
            [1.0, 2.0, 3.0],
            [1.0, 2.0, 3.0],
            [1.0, 2.0, 3.0],
            [4.0, 5.0, 6.0],
        ],
        normals: vec![
            [-1.0, 0.0, 4.371_138_5e-8],
            [-1.0, 0.0, 4.371_139e-8],
            [-1.0, 0.0, 0.5],
            [0.0, 1.0, 0.0],
        ],
        uvs: vec![[0.5, 0.5], [0.5, 0.5], [0.5, 0.5], [0.1, 0.2]],
        indices: vec![0, 1, 2, 1, 2, 3],
        material: 0,
        ..Default::default()
    };
    weld_primitive(&mut p);
    assert_eq!(p.positions.len(), 3);
    assert_eq!(p.indices, vec![0, 0, 1, 0, 1, 2]);
    assert_eq!(p.normals[0], [-1.0, 0.0, 4.371_138_5e-8]);
}

fn area_quad(material: usize, size: f32) -> LodPrimitive {
    LodPrimitive {
        positions: vec![
            [0.0, 0.0, 0.0],
            [size, 0.0, 0.0],
            [size, 0.0, size],
            [0.0, 0.0, size],
        ],
        normals: vec![[0.0, 1.0, 0.0]; 4],
        uvs: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
        indices: vec![0, 1, 2, 0, 2, 3],
        material,
        ..Default::default()
    }
}

#[test]
fn area_weighted_rects_follow_world_area_with_floor() {
    let heavy_png = flat_image(128, 128, [200, 40, 40, 255]);
    let light_png = flat_image(128, 128, [40, 200, 40, 255]);
    let m = model_of(
        vec![
            mat("heavy", AlphaClass::Opaque, [1.0; 4], Some(0)),
            mat("light", AlphaClass::Opaque, [1.0; 4], Some(1)),
        ],
        vec![area_quad(0, 100.0), area_quad(1, 1.0)],
        vec![heavy_png, light_png],
    );
    let (_, tables) = atlas_with_rects(&m, 256, 2, AtlasMode::Native).unwrap();
    let rects = &tables[0].rects;
    assert_eq!(rects.len(), 2);
    let heavy = rects[0];
    let light = rects[1];
    assert_eq!((heavy[2], heavy[3]), (128, 128), "{rects:?}");
    assert_eq!((light[2], light[3]), (16, 16), "{rects:?}");
}

#[test]
fn degenerate_weight_pool_keeps_legacy_scaling() {
    let a_png = flat_image(8, 8, [200, 40, 40, 255]);
    let b_png = flat_image(8, 8, [40, 200, 40, 255]);
    let build = || {
        model_of(
            vec![
                mat("a", AlphaClass::Opaque, [1.0; 4], Some(0)),
                mat("b", AlphaClass::Opaque, [1.0; 4], Some(1)),
            ],
            vec![prim(0, tri_uvs()), prim(1, tri_uvs())],
            vec![a_png.clone(), b_png.clone()],
        )
    };
    let (out1, tables) = atlas_with_rects(&build(), 256, 2, AtlasMode::Native).unwrap();
    let (out2, _) = atlas_with_rects(&build(), 256, 2, AtlasMode::Native).unwrap();
    assert_eq!(out1.images[0].bytes, out2.images[0].bytes);
    for r in &tables[0].rects {
        assert_eq!((r[2], r[3]), (8, 8), "{:?}", tables[0].rects);
    }
}

#[test]
fn uncapped_reps_bake_bounded_and_deterministic() {
    let (png, colors) = quad_image();
    let build = || {
        model_of(
            vec![mat("a", AlphaClass::Mask, [1.0; 4], Some(0))],
            vec![prim(0, vec![[0.0, 0.0], [300.0, 0.0], [0.0, 300.0]])],
            vec![png.clone()],
        )
    };
    let out = atlas_with(&build(), 64, 2, AtlasMode::Native).unwrap();
    let line = log_line(&out, "class=mask");
    assert!(line.contains("fallbacks=0"), "{line}");
    let out2 = atlas_with(&build(), 64, 2, AtlasMode::Native).unwrap();
    assert_eq!(out.images[0].bytes, out2.images[0].bytes);
    let canvas = decode(&out.images[0]);
    let mut avg = [0f64; 3];
    for c in colors {
        for ch in 0..3 {
            avg[ch] += srgb_to_linear_u8(c[ch]) as f64 / 4.0;
        }
    }
    let mut want = [255u8; 4];
    for ch in 0..3 {
        want[ch] = linear_to_srgb_u8(avg[ch] as f32);
    }
    assert_eq!(canvas.get_pixel(5, 5).0, want);
}

#[test]
fn atlased_model_emits_and_reparses() {
    let (png, _) = quad_image();
    let m = model_of(
        vec![
            mat("o", AlphaClass::Opaque, [1.0; 4], Some(0)),
            mat("c", AlphaClass::Mask, [1.0; 4], Some(0)),
        ],
        vec![prim(0, tri_uvs()), prim(1, tri_uvs())],
        vec![png],
    );
    let out = atlas(&m, 256, 2).unwrap();
    let glb = crate::lodgen::emit::emit_glb(&out).unwrap();
    let back = super::super::model::from_glb_bytes(&glb, "root").unwrap();
    assert_eq!(back.total_tris(), out.total_tris());
    assert_eq!(back.materials.len(), 2);
    assert_eq!(back.images.len(), 2);
    assert_eq!(back.images[0].mime, "image/jpeg");
    assert_eq!(back.materials[0].name, "TextureBakeResult-mat");
}

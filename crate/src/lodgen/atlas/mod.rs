use anyhow::{anyhow, bail, Result};
use image::RgbaImage;
use rayon::prelude::*;

use super::model::{AlphaClass, LodImage, LodMaterial, LodModel, LodPrimitive};

mod compose;
mod pack;
#[cfg(test)]
mod tests;
mod tile;

use compose::{compose, encode_atlas, native_crops, weld_primitive};
#[cfg(test)]
use pack::pack_skyline;
use pack::{pack_bucket, Packed};
use tile::{
    average_color, emissive_pixels, emissive_solid, emissive_tile, fused_repeat_bake, glows,
    intern_tile, mr_average, mr_pixels, mr_solid_bytes, mr_solid_tile, mr_tile,
    premultiplied_filtering, prim_area, solid_color, solid_tile, tint_bits, tinted_pixels, uv_plan,
    Bucket, EmisKey, MrKey, Tile, TileKey, UvMap, UvPlan,
};

const MIN_TILE_DIM: u32 = 4;
const UV_EPS: f64 = 1e-4;
const JPEG_QUALITY: u8 = 85;
const TARGET_OCCUPANCY: f64 = 0.75;
const SHRINK_STEP: f64 = 0.95;
const MAX_PACK_TRIES: u32 = 200;
const GROW_TRIES: u32 = 12;
const TILE_FLOOR_DIV: u32 = 16;
const LOSSLESS_OPAQUE_MIN_BUDGET: u32 = 512;

const NATIVE_SOLID_DIM: u32 = 8;
const NATIVE_MIN_CANVAS: u32 = 8;

const BUCKET_SPECS: [(AlphaClass, &str, &str); 4] = [
    (AlphaClass::Opaque, "TextureBakeResult-mat", "opaque"),
    (AlphaClass::Mask, "TextureBakeResult-mat-cutout", "mask"),
    (
        AlphaClass::Blend,
        "TextureBakeResult-mat-transparent",
        "blend",
    ),
    (AlphaClass::Opaque, "TextureBakeResult-mat-metal", "metal"),
];
const METAL_BUCKET: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtlasMode {
    FullBleed,
    Native,
    Adaptive,
}

pub fn budget_pot(max_size: u32) -> u32 {
    let mut pot = 1u32;
    while pot * 2 <= max_size {
        pot *= 2;
    }
    pot
}

pub fn class_material_name(class: AlphaClass) -> &'static str {
    BUCKET_SPECS[class_index(class)].1
}

fn bucket_index(mat: &LodMaterial, fidelity: bool) -> usize {
    if fidelity && super::model::is_metal(mat) {
        METAL_BUCKET
    } else {
        class_index(mat.class)
    }
}

fn class_index(class: AlphaClass) -> usize {
    match class {
        AlphaClass::Opaque => 0,
        AlphaClass::Mask => 1,
        AlphaClass::Blend => 2,
    }
}

pub fn atlas(model: &LodModel, max_size: u32, padding: u32) -> Result<LodModel> {
    atlas_with(model, max_size, padding, AtlasMode::FullBleed, false)
}

pub fn atlas_with(
    model: &LodModel,
    max_size: u32,
    padding: u32,
    mode: AtlasMode,
    fidelity: bool,
) -> Result<LodModel> {
    Ok(atlas_with_rects(model, max_size, padding, mode, fidelity)?.0)
}

pub fn atlas_with_rects(
    model: &LodModel,
    max_size: u32,
    padding: u32,
    mode: AtlasMode,
    fidelity: bool,
) -> Result<(LodModel, Vec<super::reclamp::ClassRects>)> {
    if model.primitives.is_empty() {
        bail!("atlas: model has no primitives");
    }
    let max_pot = budget_pot(max_size);
    if max_pot <= 2 * padding + MIN_TILE_DIM {
        bail!("atlas: max size {max_size} too small for padding {padding}");
    }

    #[cfg(not(target_arch = "wasm32"))]
    let t0 = std::time::Instant::now();
    #[cfg(target_arch = "wasm32")]
    let atlas_ms: u128 = 0;
    let mut log = model.log.clone();
    let mut needed = vec![false; model.images.len()];
    let mut any_glow = false;
    for prim in &model.primitives {
        if prim.positions.is_empty() || prim.indices.len() < 3 {
            continue;
        }
        let Some(mat) = model.materials.get(prim.material) else {
            continue;
        };
        if let Some(idx) = mat.image.filter(|&i| i < needed.len()) {
            needed[idx] = true;
        }
        if glows(mat) {
            any_glow = true;
            if let Some(idx) = mat.emissive_image.filter(|&i| i < needed.len()) {
                needed[idx] = true;
            }
        }
        if fidelity && super::model::is_metal(mat) {
            if let Some(idx) = mat.mr_image.filter(|&i| i < needed.len()) {
                needed[idx] = true;
            }
        }
    }
    let decoded: Vec<Option<(RgbaImage, String)>> = needed
        .par_iter()
        .enumerate()
        .map(|(idx, &need)| {
            if !need {
                return None;
            }
            image::load_from_memory(&model.images[idx].bytes)
                .ok()
                .map(|d| {
                    (
                        d.to_rgba8(),
                        crate::hashes::sha256_hex(&model.images[idx].bytes),
                    )
                })
        })
        .collect();
    for idx in 0..decoded.len() {
        if needed[idx] && decoded[idx].is_none() {
            log.push(format!(
                "atlas: image {idx} failed to decode, using solid base-color tile"
            ));
        }
    }
    let mut buckets: [Bucket; 4] = std::array::from_fn(|_| Bucket::default());
    let mut class_double_sided = [false; 4];

    for (pi, prim) in model.primitives.iter().enumerate() {
        if prim.positions.is_empty() || prim.indices.len() < 3 {
            continue;
        }
        let mat = model
            .materials
            .get(prim.material)
            .ok_or_else(|| anyhow!("atlas: primitive {pi} references missing material"))?;
        let bi = bucket_index(mat, fidelity);
        let bucket = &mut buckets[bi];
        bucket.refs += 1;
        if bi == METAL_BUCKET {
            let tris = (prim.indices.len() / 3) as f64;
            bucket.met_sum += mat.metallic * tris;
            bucket.rough_sum += mat.roughness * tris;
            bucket.met_tris += tris;
        }
        if fidelity && mat.class != AlphaClass::Opaque && mat.double_sided {
            class_double_sided[bi] = true;
        }
        let img_ref: Option<&(RgbaImage, String)> = match mat.image {
            Some(idx) if idx < model.images.len() => decoded[idx].as_ref(),
            _ => None,
        };
        let emis_ref: Option<&(RgbaImage, String)> = match mat.emissive_image {
            Some(idx) if glows(mat) && idx < model.images.len() => decoded[idx].as_ref(),
            _ => None,
        };
        let mr_ref: Option<&(RgbaImage, String)> = match mat.mr_image {
            Some(idx) if bi == METAL_BUCKET && idx < model.images.len() => decoded[idx].as_ref(),
            _ => None,
        };
        let mrkey_solid = if bi == METAL_BUCKET {
            MrKey::Solid(mr_solid_bytes(mat.metallic, mat.roughness))
        } else {
            MrKey::None
        };
        let mr_from_key = |mk: &MrKey| match mk {
            MrKey::Solid(ms) => mr_solid_tile(*ms),
            _ => solid_tile([0, 0, 0, 0]),
        };
        match img_ref {
            None => {
                let ekey = if glows(mat) {
                    EmisKey::Solid(emissive_solid(mat.emissive))
                } else {
                    EmisKey::Dark
                };
                let mkey = mrkey_solid;
                let color = solid_color(mat.base_color);
                let ti = intern_tile(
                    bucket,
                    TileKey::Solid(color, ekey.clone(), mkey.clone()),
                    || {
                        let e = match &ekey {
                            EmisKey::Solid(c) => solid_tile(*c),
                            _ => solid_tile([0, 0, 0, 255]),
                        };
                        (solid_tile(color), e, mr_from_key(&mkey))
                    },
                );
                bucket.prims.push((pi, ti, UvMap::Center));
            }
            Some((img, img_hash)) => match uv_plan(&prim.uvs) {
                UvPlan::Rect { shift, reps } => {
                    let ekey = if !glows(mat) {
                        EmisKey::Dark
                    } else if let Some((_, ehash)) = emis_ref {
                        EmisKey::Image {
                            hash: ehash.clone(),
                            factor: mat.emissive.map(|v| v.to_bits()),
                        }
                    } else {
                        EmisKey::Solid(emissive_solid(mat.emissive))
                    };
                    let mkey = if let Some((_, mhash)) = mr_ref {
                        bucket.has_mr_tex = true;
                        MrKey::Image {
                            hash: mhash.clone(),
                            factors: [mat.metallic.to_bits(), mat.roughness.to_bits()],
                        }
                    } else {
                        mrkey_solid
                    };
                    let key = TileKey::Image {
                        hash: img_hash.clone(),
                        tint: tint_bits(mat.base_color),
                        reps,
                        emis: ekey.clone(),
                        mr: mkey.clone(),
                    };
                    let ti = intern_tile(bucket, key, || {
                        let tinted = tinted_pixels(img, mat.base_color);
                        let (px, w, h) = fused_repeat_bake(
                            tinted,
                            img.width(),
                            img.height(),
                            reps,
                            max_pot,
                            premultiplied_filtering(mat.class),
                            true,
                        );
                        let e = match (&ekey, emis_ref) {
                            (EmisKey::Image { .. }, Some((eimg, _))) => emissive_tile(
                                eimg,
                                mat.emissive,
                                (img.width(), img.height()),
                                reps,
                                max_pot,
                            ),
                            (EmisKey::Solid(c), _) => solid_tile(*c),
                            _ => solid_tile([0, 0, 0, 255]),
                        };
                        let mrt = match (&mkey, mr_ref) {
                            (MrKey::Image { .. }, Some((mimg, _))) => mr_tile(
                                mimg,
                                mat.metallic,
                                mat.roughness,
                                (img.width(), img.height()),
                                reps,
                                max_pot,
                            ),
                            _ => mr_from_key(&mkey),
                        };
                        (Tile::from_pixels(px, w, h), e, mrt)
                    });
                    bucket.weights[ti] += prim_area(prim);
                    bucket.prims.push((
                        pi,
                        ti,
                        UvMap::Rect {
                            shift,
                            reps: [reps.0 as f64, reps.1 as f64],
                        },
                    ));
                }
                UvPlan::Fallback => {
                    bucket.fallbacks += 1;
                    log.push(format!(
                        "atlas: WARN fallback prim {pi} material {:?}: non-finite uvs, collapsed to average-color tile",
                        mat.name
                    ));
                    let tinted = tinted_pixels(img, mat.base_color);
                    let color = average_color(mat.class, &tinted);
                    let ekey = if !glows(mat) {
                        EmisKey::Dark
                    } else if let Some((eimg, _)) = emis_ref {
                        let mut avg =
                            average_color(mat.class, &emissive_pixels(eimg, mat.emissive));
                        avg[3] = 255;
                        EmisKey::Solid(avg)
                    } else {
                        EmisKey::Solid(emissive_solid(mat.emissive))
                    };
                    let mkey = if let Some((mimg, _)) = mr_ref {
                        MrKey::Solid(mr_average(&mr_pixels(mimg, mat.metallic, mat.roughness)))
                    } else {
                        mrkey_solid
                    };
                    let ti = intern_tile(
                        bucket,
                        TileKey::Solid(color, ekey.clone(), mkey.clone()),
                        || {
                            let e = match &ekey {
                                EmisKey::Solid(c) => solid_tile(*c),
                                _ => solid_tile([0, 0, 0, 255]),
                            };
                            (solid_tile(color), e, mr_from_key(&mkey))
                        },
                    );
                    bucket.prims.push((pi, ti, UvMap::Center));
                }
            },
        }
    }

    let mut out = LodModel {
        root_name: model.root_name.clone(),
        ..Default::default()
    };
    type HeavyOut = (
        Packed,
        LodImage,
        Option<LodImage>,
        Option<LodImage>,
        Vec<Option<[u32; 4]>>,
    );
    let heavy: Vec<Option<Result<HeavyOut>>> = BUCKET_SPECS
        .par_iter()
        .zip(buckets.par_iter_mut())
        .map(|(&(class, _, _), bucket)| {
            if bucket.prims.is_empty() {
                return None;
            }
            Some((|| {
                let mut crops = match mode {
                    AtlasMode::Native | AtlasMode::Adaptive => native_crops(bucket, model),
                    AtlasMode::FullBleed => vec![None; bucket.tiles.len()],
                };
                let packed = pack_bucket(
                    &mut bucket.tiles,
                    &mut crops,
                    &bucket.weights,
                    mode,
                    max_pot,
                    padding,
                )?;
                let canvas_px = compose(
                    &bucket.tiles,
                    &crops,
                    &packed.rects,
                    packed.canvas,
                    padding,
                    premultiplied_filtering(class),
                    true,
                );
                let img = encode_atlas(class, canvas_px, packed.canvas, max_pot)?;
                let emis_img = if any_glow {
                    let emis_px = compose(
                        &bucket.emis,
                        &crops,
                        &packed.rects,
                        packed.canvas,
                        padding,
                        false,
                        true,
                    );
                    Some(encode_atlas(class, emis_px, packed.canvas, max_pot)?)
                } else {
                    None
                };
                let mr_img = if bucket.has_mr_tex {
                    let mr_px = compose(
                        &bucket.mr,
                        &crops,
                        &packed.rects,
                        packed.canvas,
                        padding,
                        false,
                        false,
                    );
                    let img = RgbaImage::from_raw(packed.canvas, packed.canvas, mr_px)
                        .ok_or_else(|| anyhow!("atlas mr buffer"))?;
                    let mut cur = std::io::Cursor::new(Vec::new());
                    img.write_to(&mut cur, image::ImageFormat::Png)?;
                    Some(LodImage {
                        bytes: cur.into_inner(),
                        mime: "image/png".to_string(),
                    })
                } else {
                    None
                };
                Ok((packed, img, emis_img, mr_img, crops))
            })())
        })
        .collect();
    let mut total_fallbacks = 0usize;
    let mut rect_tables = Vec::new();
    for (ci, item) in heavy.into_iter().enumerate() {
        let Some(res) = item else {
            continue;
        };
        let (packed, img, emis_img, mr_img, crops) = res?;
        let (class, mat_name, tag) = BUCKET_SPECS[ci];
        let bucket = &buckets[ci];
        let mime = img.mime.clone();
        let img_idx = out.images.len();
        out.images.push(img);
        let emis_idx = emis_img.map(|e| {
            let i = out.images.len();
            out.images.push(e);
            i
        });
        let mr_idx = mr_img.map(|e| {
            let i = out.images.len();
            out.images.push(e);
            i
        });
        let mat_idx = out.materials.len();
        let (metallic, roughness) = if mr_idx.is_some() {
            (1.0, 0.0)
        } else if ci == METAL_BUCKET && bucket.met_tris > 0.0 {
            (
                bucket.met_sum / bucket.met_tris,
                bucket.rough_sum / bucket.met_tris,
            )
        } else {
            (0.0, 1.0)
        };
        out.materials.push(LodMaterial {
            name: mat_name.to_string(),
            class,
            base_color: [1.0, 1.0, 1.0, 1.0],
            cutoff: 0.5,
            image: Some(img_idx),
            double_sided: class_double_sided[ci],
            emissive: if emis_idx.is_some() {
                [1.0; 3]
            } else {
                [0.0; 3]
            },
            emissive_image: emis_idx,
            metallic,
            roughness,
            mr_image: mr_idx,
            ..Default::default()
        });
        let s = packed.canvas as f64;
        let mut merged = LodPrimitive {
            material: mat_idx,
            ..Default::default()
        };
        for (pi, ti, uvmap) in &bucket.prims {
            let prim = &model.primitives[*pi];
            let base = merged.positions.len() as u32;
            merged.positions.extend_from_slice(&prim.positions);
            merged.normals.extend_from_slice(&prim.normals);
            let (rx, ry, rw, rh) = packed.rects[*ti];
            let tile = &bucket.tiles[*ti];
            let crop = crops[*ti];
            for uv in &prim.uvs {
                let (lu, lv) = match uvmap {
                    UvMap::Center => (0.5, 0.5),
                    UvMap::Rect { shift, reps } => {
                        let mut lu = ((uv[0] as f64 - shift[0]) / reps[0]).clamp(0.0, 1.0);
                        let mut lv = ((uv[1] as f64 - shift[1]) / reps[1]).clamp(0.0, 1.0);
                        if let Some([cx, cy, cw, ch]) = crop {
                            lu = ((lu * tile.src_w as f64 - cx as f64) / cw as f64).clamp(0.0, 1.0);
                            lv = ((lv * tile.src_h as f64 - cy as f64) / ch as f64).clamp(0.0, 1.0);
                        }
                        (lu, lv)
                    }
                };
                merged.uvs.push([
                    ((rx as f64 + lu * rw as f64) / s) as f32,
                    ((ry as f64 + lv * rh as f64) / s) as f32,
                ]);
            }
            for &i in &prim.indices {
                merged.indices.push(base + i);
            }
        }
        weld_primitive(&mut merged);
        let occupancy = packed
            .rects
            .iter()
            .map(|r| r.2 as f64 * r.3 as f64)
            .sum::<f64>()
            / (s * s)
            * 100.0;
        let emis_note = match emis_idx {
            Some(i) => format!(" emissive_image={} ({})", i, out.images[i].mime),
            None => String::new(),
        };
        let mr_note = match mr_idx {
            Some(i) => format!(" metal_rough_image={} ({})", i, out.images[i].mime),
            None => String::new(),
        };
        log.push(format!(
            "atlas: class={} material={} size={} refs={} unique={} occupancy={:.1}% fallbacks={} scale={:.3} mime={}{}{}",
            tag,
            mat_name,
            packed.canvas,
            bucket.refs,
            bucket.tiles.len(),
            occupancy,
            bucket.fallbacks,
            packed.scale,
            mime,
            emis_note,
            mr_note
        ));
        total_fallbacks += bucket.fallbacks;
        rect_tables.push(super::reclamp::ClassRects {
            material: mat_name.to_string(),
            canvas: packed.canvas,
            rects: packed
                .rects
                .iter()
                .map(|&(x, y, w, h)| [x, y, w, h])
                .collect(),
        });
        out.primitives.push(merged);
    }
    if out.primitives.is_empty() {
        bail!("atlas: no non-empty primitives");
    }
    log.push(format!(
        "atlas: classes={} prims_in={} prims_out={} images_in={} images_out={} fallbacks={} tris_in={} tris_out={} elapsed_ms={}",
        out.primitives.len(),
        model.primitives.len(),
        out.primitives.len(),
        model.images.len(),
        out.images.len(),
        total_fallbacks,
        model.total_tris(),
        out.primitives.iter().map(|p| p.indices.len() / 3).sum::<usize>(),
        {
            #[cfg(not(target_arch = "wasm32"))]
            let atlas_ms = t0.elapsed().as_millis();
            atlas_ms
        }
    ));
    out.log = log;
    Ok((out, rect_tables))
}

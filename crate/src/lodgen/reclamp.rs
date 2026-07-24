use super::model::LodModel;

#[derive(Clone, Debug)]
pub struct ClassRects {
    pub material: String,
    pub canvas: u32,
    pub rects: Vec<[u32; 4]>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ReclampReport {
    pub scanned: usize,
    pub reclamped: usize,
}

pub fn reclamp_model(model: &mut LodModel, tables: &[ClassRects]) -> ReclampReport {
    let mut report = ReclampReport::default();
    for prim_idx in 0..model.primitives.len() {
        let mat_name = match model.materials.get(model.primitives[prim_idx].material) {
            Some(m) => m.name.clone(),
            None => continue,
        };
        let Some(table) = tables.iter().find(|t| t.material == mat_name) else {
            continue;
        };
        let s = table.canvas as f32;
        let half_texel = 0.5 / s;
        let eps = half_texel;
        let rects: Vec<[f32; 4]> = table
            .rects
            .iter()
            .map(|&[x, y, w, h]| {
                [
                    x as f32 / s,
                    y as f32 / s,
                    (x + w) as f32 / s,
                    (y + h) as f32 / s,
                ]
            })
            .collect();
        let contains = |r: &[f32; 4], u: f32, v: f32| {
            u >= r[0] - eps && u <= r[2] + eps && v >= r[1] - eps && v <= r[3] + eps
        };
        let prim = &mut model.primitives[prim_idx];
        let ntris = prim.indices.len() / 3;
        report.scanned += ntris;
        for t in 0..ntris {
            let tri = [
                prim.indices[3 * t] as usize,
                prim.indices[3 * t + 1] as usize,
                prim.indices[3 * t + 2] as usize,
            ];
            let uvs = [prim.uvs[tri[0]], prim.uvs[tri[1]], prim.uvs[tri[2]]];
            let bb = uvs
                .iter()
                .fold([f32::MAX, f32::MAX, f32::MIN, f32::MIN], |a, uv| {
                    [
                        a[0].min(uv[0]),
                        a[1].min(uv[1]),
                        a[2].max(uv[0]),
                        a[3].max(uv[1]),
                    ]
                });
            if rects.iter().any(|r| {
                bb[0] >= r[0] - eps
                    && bb[2] <= r[2] + eps
                    && bb[1] >= r[1] - eps
                    && bb[3] <= r[3] + eps
            }) {
                continue;
            }
            let homes: Vec<Option<usize>> = uvs
                .iter()
                .map(|uv| rects.iter().position(|r| contains(r, uv[0], uv[1])))
                .collect();
            let mut dominant: Option<usize> = None;
            for h in homes.iter().flatten() {
                if homes.iter().flatten().filter(|x| *x == h).count() >= 2 {
                    dominant = Some(*h);
                    break;
                }
            }
            let dominant = dominant.or_else(|| homes.iter().flatten().next().copied());
            let Some(ri) = dominant else { continue };
            let r = rects[ri];
            let lo_u = r[0] + half_texel;
            let hi_u = (r[2] - half_texel).max(lo_u);
            let lo_v = r[1] + half_texel;
            let hi_v = (r[3] - half_texel).max(lo_v);
            for (k, &vi) in tri.iter().enumerate() {
                let uv = uvs[k];
                let cu = uv[0].clamp(lo_u, hi_u);
                let cv = uv[1].clamp(lo_v, hi_v);
                if cu == uv[0] && cv == uv[1] {
                    continue;
                }
                let new_idx = prim.positions.len() as u32;
                prim.positions.push(prim.positions[vi]);
                prim.normals.push(prim.normals[vi]);
                prim.uvs.push([cu, cv]);
                prim.indices[3 * t + k] = new_idx;
            }
            report.reclamped += 1;
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lodgen::model::{AlphaClass, LodMaterial, LodPrimitive};

    fn model_with_uvs(uvs: Vec<[f32; 2]>, indices: Vec<u32>) -> LodModel {
        let n = uvs.len();
        LodModel {
            root_name: "t".to_string(),
            primitives: vec![LodPrimitive {
                positions: vec![[0.0, 0.0, 0.0]; n],
                normals: vec![[0.0, 1.0, 0.0]; n],
                uvs,
                indices,
                material: 0,
                ..Default::default()
            }],
            materials: vec![LodMaterial {
                name: "opaque".to_string(),
                class: AlphaClass::Opaque,
                base_color: [1.0, 1.0, 1.0, 1.0],
                cutoff: 0.5,
                image: None,
                double_sided: false,
                ..Default::default()
            }],
            images: Vec::new(),
            log: Vec::new(),
        }
    }

    fn table() -> Vec<ClassRects> {
        vec![ClassRects {
            material: "opaque".to_string(),
            canvas: 256,
            rects: vec![[0, 0, 64, 64], [64, 0, 64, 64], [128, 0, 128, 128]],
        }]
    }

    #[test]
    fn in_rect_triangle_is_untouched() {
        let mut m = model_with_uvs(vec![[0.05, 0.05], [0.2, 0.05], [0.05, 0.2]], vec![0, 1, 2]);
        let before = m.primitives[0].uvs.clone();
        let rep = reclamp_model(&mut m, &table());
        assert_eq!(rep.scanned, 1);
        assert_eq!(rep.reclamped, 0);
        assert_eq!(m.primitives[0].uvs, before);
    }

    #[test]
    fn cross_rect_triangle_snaps_to_majority_rect() {
        let mut m = model_with_uvs(vec![[0.05, 0.05], [0.2, 0.1], [0.9, 0.3]], vec![0, 1, 2]);
        let rep = reclamp_model(&mut m, &table());
        assert_eq!(rep.reclamped, 1);
        let p = &m.primitives[0];
        assert_eq!(p.positions.len(), 4);
        let moved = p.uvs[p.indices[2] as usize];
        let hi = 0.25 - 0.5 / 256.0;
        assert!((moved[0] - hi).abs() < 1e-6, "{moved:?}");
        assert!(moved[1] <= hi + 1e-6, "{moved:?}");
        assert_eq!(p.uvs[p.indices[0] as usize], [0.05, 0.05]);
    }

    #[test]
    fn shared_vertex_of_good_triangle_is_not_mutated() {
        let mut m = model_with_uvs(
            vec![[0.05, 0.05], [0.2, 0.1], [0.05, 0.2], [0.9, 0.3]],
            vec![0, 1, 2, 1, 3, 0],
        );
        let rep = reclamp_model(&mut m, &table());
        assert_eq!(rep.reclamped, 1);
        let p = &m.primitives[0];
        assert_eq!(p.uvs[1], [0.2, 0.1]);
        assert_eq!(&p.indices[..3], &[0, 1, 2]);
    }
}

//! Compares `fontgen::bake` against a font asset Unity baked itself: glyph metrics, and each
//! glyph's distance field texel by texel (region-relative, since packing differs).
//!
//! usage: fontcal <font.ttf> <tmp.json> <atlas0.raw>
//!
//! The Unity side is a dynamic `TMP_FontAsset` created with the explorer's settings
//! (`CreateFontAsset(font, 90, 9, SDFAA, 1024, 1024)`) and pre-filled with
//! `TryAddCharacters`: `tmp.json` is `EditorJsonUtility.ToJson(asset)` and `atlas0.raw` is
//! `asset.atlasTextures[0].GetRawTextureData()`. `CAL_GLYPH=<glyph index>` prints that glyph's
//! field next to Unity's.
use abgen::fontgen;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let font = std::fs::read(&a[0]).unwrap();
    let unity: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&a[1]).unwrap()).unwrap();
    let uatlas = std::fs::read(&a[2]).unwrap();
    let t = std::time::Instant::now();
    let baked = fontgen::bake(&font).unwrap();
    println!(
        "bake {:?}: {} glyphs {} chars used={} free={} render_work={} ({:.1}% of the budget)",
        t.elapsed(),
        baked.glyphs.len(),
        baked.characters.len(),
        baked.used_rects.len(),
        baked.free_rects.len(),
        baked.render_work,
        baked.render_work as f64 * 100.0 / fontgen::MAX_RENDER_WORK as f64
    );
    println!("face {:?}", baked.face);
    let mb = &unity["MonoBehaviour"];
    println!("unity face {}", mb["m_FaceInfo"]);
    let ug: std::collections::HashMap<u64, &serde_json::Value> = mb["m_GlyphTable"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| (g["m_Index"].as_u64().unwrap(), g))
        .collect();
    let (mut metric_bad, mut n, mut hist, mut band) = (0, 0, [0usize; 6], [0usize; 6]);
    let pad = fontgen::ATLAS_PADDING as i64;
    for g in &baked.glyphs {
        let Some(u) = ug.get(&(g.index as u64)) else {
            continue;
        };
        if u["m_AtlasIndex"].as_u64() != Some(0) {
            continue;
        }
        let m = &u["m_Metrics"];
        let d = [
            (g.width, "m_Width"),
            (g.height, "m_Height"),
            (g.bearing_x, "m_HorizontalBearingX"),
            (g.bearing_y, "m_HorizontalBearingY"),
            (g.advance, "m_HorizontalAdvance"),
        ];
        let bad: Vec<_> = d
            .iter()
            .filter(|(v, k)| (v - m[*k].as_f64().unwrap()).abs() > 1e-3)
            .collect();
        let ur = &u["m_GlyphRect"];
        let (ux, uy, uw, uh) = (
            ur["m_X"].as_i64().unwrap(),
            ur["m_Y"].as_i64().unwrap(),
            ur["m_Width"].as_i64().unwrap(),
            ur["m_Height"].as_i64().unwrap(),
        );
        if !bad.is_empty() || uw != g.rect.width as i64 || uh != g.rect.height as i64 {
            metric_bad += 1;
            if metric_bad <= 5 {
                println!(
                    "metric diff glyph {} ours w{} h{} bx{} by{} rect {}x{} unity {} rect {}x{}",
                    g.index,
                    g.width,
                    g.height,
                    g.bearing_x,
                    g.bearing_y,
                    g.rect.width,
                    g.rect.height,
                    m,
                    uw,
                    uh
                );
            }
            continue;
        }
        if uw == 0 {
            continue;
        }
        if std::env::var("CAL_GLYPH")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            == Some(g.index)
        {
            side_by_side(
                &baked.atlas,
                g.rect.x as i64,
                g.rect.y as i64,
                &uatlas,
                ux,
                uy,
                uw,
                &[-9, -5, -1, 0, 5, 20, 40, uh - 1, uh + 3, uh + 8],
            );
        }
        for j in -pad..uh + pad {
            for i in -pad..uw + pad {
                let o = baked.atlas[((g.rect.y as i64 + j) * 1024 + g.rect.x as i64 + i) as usize]
                    as i64;
                let r = uatlas[((uy + j) * 1024 + ux + i) as usize] as i64;
                let diff = (o - r).unsigned_abs() as usize;
                hist[diff.min(5)] += 1;
                n += 1;
                if (r - 127).abs() <= 38 {
                    band[diff.min(5)] += 1;
                }
            }
        }
    }
    println!("metric mismatches {metric_bad}; texels {n}; |diff| histogram 0..=5+: {hist:?}; within 3px of the edge: {band:?}");
}

#[allow(clippy::too_many_arguments)]
fn side_by_side(
    ours: &[u8],
    ox: i64,
    oy: i64,
    unity: &[u8],
    ux: i64,
    uy: i64,
    w: i64,
    rows: &[i64],
) {
    for &j in rows {
        let a: Vec<String> = (-9..w + 9)
            .map(|i| format!("{:3}", ours[((oy + j) * 1024 + ox + i) as usize]))
            .collect();
        let b: Vec<String> = (-9..w + 9)
            .map(|i| format!("{:3}", unity[((uy + j) * 1024 + ux + i) as usize]))
            .collect();
        println!("row {j:3} ours  {}", a.join(""));
        println!("row {j:3} unity {}", b.join(""));
    }
}


use abgen::bc7_pure;
use abgen::gpu::{build_engine, encode_bc7_mip_chain_on, init_gpu, Bc7Profile, Engine, Gpu};
use std::cell::RefCell;
use std::rc::Rc;
use wasm_bindgen::prelude::*;

thread_local! {
    static STATE: RefCell<Option<Rc<(Gpu, Engine)>>> = const { RefCell::new(None) };
}

fn cpu_profile(p: Bc7Profile) -> bc7_pure::Bc7Profile {
    match p {
        Bc7Profile::Slow => bc7_pure::Bc7Profile::Slow,
        Bc7Profile::Basic => bc7_pure::Bc7Profile::Basic,
    }
}

fn gen_texture(seed: u64, w: u32, h: u32) -> Vec<u8> {
    let mut s = seed.wrapping_mul(0x9e3779b97f4a7c15).wrapping_add(1);
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..(w * h) {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let v = s >> 32;
        out.extend_from_slice(&[
            (v & 0xff) as u8,
            ((v >> 8) & 0xff) as u8,
            ((v >> 16) & 0xff) as u8,
            ((v >> 24) & 0xff) as u8,
        ]);
    }
    out
}

async fn qualify(g: &Gpu, eng: &Engine) -> Result<(), String> {
    for &(w, h) in &[(64u32, 64u32), (128, 32), (37, 53)] {
        let tex = gen_texture(1, w, h);
        for srgb in [false, true] {
            for perceptual in [false, true] {
                for profile in [Bc7Profile::Slow, Bc7Profile::Basic] {
                    let (want, want_mips) = bc7_pure::encode_bc7_mip_chain_with_profile(
                        &tex,
                        w,
                        h,
                        None,
                        true,
                        srgb,
                        perceptual,
                        cpu_profile(profile),
                    );
                    let (got, got_mips) = encode_bc7_mip_chain_on(
                        g, eng, &tex, w, h, None, true, srgb, perceptual, profile,
                    )
                    .await
                    .map_err(|e| format!("qualification encode failed: {e:#}"))?;
                    if got != want || got_mips != want_mips {
                        let diff = got
                            .iter()
                            .zip(want.iter())
                            .position(|(a, b)| a != b)
                            .unwrap_or(got.len().min(want.len()));
                        let ctx = |v: &[u8]| {
                            v.iter()
                                .skip(diff & !15)
                                .take(16)
                                .map(|b| format!("{b:02x}"))
                                .collect::<String>()
                        };
                        return Err(format!(
                            "not bit-exact vs CPU oracle at {w}x{h} srgb={srgb} \
                             perceptual={perceptual} profile={profile:?}: first diff \
                             byte {diff} (block {} byte-in-block {}; lens {}/{} mips {got_mips}/{want_mips}) \
                             got[{}..]={} want={}",
                            diff / 16,
                            diff % 16,
                            got.len(),
                            want.len(),
                            diff & !15,
                            ctx(&got),
                            ctx(&want),
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

#[wasm_bindgen]
pub async fn gpu_init() -> Result<String, JsValue> {
    let g = init_gpu().await.map_err(|e| JsValue::from_str(&e))?;
    let eng = build_engine(&g);
    qualify(&g, &eng)
        .await
        .map_err(|e| JsValue::from_str(&format!("{}: {e}", g.adapter_summary())))?;
    let summary = g.adapter_summary();
    STATE.with(|s| *s.borrow_mut() = Some(Rc::new((g, eng))));
    Ok(summary)
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[wasm_bindgen]
pub async fn gpu_bisect() -> Result<String, JsValue> {
    let state = STATE.with(|s| s.borrow().clone());
    let owned: Option<Gpu> = if state.is_some() {
        None
    } else {
        Some(init_gpu().await.map_err(|e| JsValue::from_str(&e))?)
    };
    let g: &Gpu = match &state {
        Some(st) => &st.0,
        None => owned.as_ref().unwrap(),
    };
    let results = abgen::gpu::bisect::run_bisect(g).await;
    let mut json = String::from("{\"adapter\":\"");
    json.push_str(&json_escape(&g.adapter_summary()));
    json.push_str("\",\"results\":[");
    for (i, r) in results.iter().enumerate() {
        if i > 0 {
            json.push(',');
        }
        json.push_str(&format!(
            "{{\"entry\":\"{}\",\"cases\":{},\"pass\":{}",
            json_escape(&r.entry),
            r.cases,
            r.pass
        ));
        if let Some(d) = &r.first_diff {
            json.push_str(&format!(
                ",\"first_diff\":{{\"byte_offset\":{},\"got_word\":{},\"want_word\":{},\"case_index\":{}}}",
                d.byte_offset, d.got_word, d.want_word, d.case_index
            ));
        } else {
            json.push_str(",\"first_diff\":null");
        }
        json.push('}');
    }
    json.push_str("]}");
    Ok(json)
}

#[wasm_bindgen]
pub async fn gpu_encode(req: &[u8]) -> Result<js_sys::Uint8Array, JsValue> {
    if req.len() < 16 {
        return Err(JsValue::from_str("request too short"));
    }
    let u32_at = |i: usize| u32::from_le_bytes(req[i..i + 4].try_into().unwrap());
    let (w, h) = (u32_at(0), u32_at(4));
    let mips = u32_at(8) as i32;
    let flags = u32_at(12);
    let rgba = &req[16..];
    if rgba.len() != (w as usize) * (h as usize) * 4 {
        return Err(JsValue::from_str("rgba length mismatch"));
    }
    let profile = if flags & 8 != 0 {
        Bc7Profile::Basic
    } else {
        Bc7Profile::Slow
    };
    let state = STATE.with(|s| s.borrow().clone());
    let Some(state) = state else {
        return Err(JsValue::from_str("gpu_init has not succeeded"));
    };
    let (g, eng) = (&state.0, &state.1);
    let (out, _mips) = encode_bc7_mip_chain_on(
        g,
        eng,
        rgba,
        w,
        h,
        Some(mips),
        flags & 1 != 0,
        flags & 2 != 0,
        flags & 4 != 0,
        profile,
    )
    .await
    .map_err(|e| JsValue::from_str(&format!("{e:#}")))?;
    Ok(js_sys::Uint8Array::from(out.as_slice()))
}

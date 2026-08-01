//! abgen as a wasm module — the browser host of [`abgen::export`].
//!
//! Only the wasm-shaped parts live here: the `env` host imports, the linear-
//! memory alloc/free pair, and `poc_init`.
//!
//! This crate is excluded from the root workspace. While it carried its own
//! copy of the pipeline nothing compiled it, the copy drifted out of sync with
//! `abgen`'s API and stopped building unnoticed; sharing the core plus the
//! `excluded-hosts` CI job is what stops that recurring.

use abgen::export::{self, HostInfo, Sink};

#[link(wasm_import_module = "env")]
unsafe extern "C" {
    fn host_emit(kind: u32, ptr: *const u8, len: usize);
    fn host_encode_bc7(req_ptr: *const u8, req_len: usize, out_ptr: *mut u8, out_cap: usize)
        -> i32;
}

const HOST: HostInfo = HostInfo::new("v-abgen-wasm", "wasm://in-browser");

struct WasmSink;

impl Sink for WasmSink {
    fn emit(&self, kind: export::Kind, bytes: &[u8]) {
        unsafe { host_emit(kind as u32, bytes.as_ptr(), bytes.len()) }
    }
}

/// bc7_pure wasm hook: serialize one encode request for the host.
/// Layout (LE): u32 width, u32 height, i32 mips, u32 flags
/// (bit0 flip, bit1 srgb, bit2 perceptual, bit3 profile-basic), rgba bytes.
fn gpu_host_encode(
    rgba: &[u8],
    width: u32,
    height: u32,
    mip_count: Option<i32>,
    flip: bool,
    srgb: bool,
    perceptual: bool,
    profile: abgen::bc7_pure::Bc7Profile,
) -> Option<(Vec<u8>, i32)> {
    use abgen::bc7_pure::{compute_default_mip_count, compute_mip_chain_size, Bc7Profile};
    let mips = mip_count.unwrap_or_else(|| compute_default_mip_count(width, height));
    let flags: u32 = (flip as u32)
        | ((srgb as u32) << 1)
        | ((perceptual as u32) << 2)
        | ((matches!(profile, Bc7Profile::Basic) as u32) << 3);
    let mut req = Vec::with_capacity(16 + rgba.len());
    req.extend_from_slice(&width.to_le_bytes());
    req.extend_from_slice(&height.to_le_bytes());
    req.extend_from_slice(&mips.to_le_bytes());
    req.extend_from_slice(&flags.to_le_bytes());
    req.extend_from_slice(rgba);
    let mut out = vec![0u8; compute_mip_chain_size(width, height, mips)];
    let n = unsafe { host_encode_bc7(req.as_ptr(), req.len(), out.as_mut_ptr(), out.len()) };
    if n <= 0 || n as usize != out.len() {
        return None;
    }
    Some((out, mips))
}

#[unsafe(no_mangle)]
pub extern "C" fn poc_alloc(len: usize) -> *mut u8 {
    let layout = std::alloc::Layout::array::<u8>(len.max(1)).unwrap();
    unsafe { std::alloc::alloc(layout) }
}

#[unsafe(no_mangle)]
pub extern "C" fn poc_free(ptr: *mut u8, len: usize) {
    let layout = std::alloc::Layout::array::<u8>(len.max(1)).unwrap();
    unsafe { std::alloc::dealloc(ptr, layout) }
}

#[unsafe(no_mangle)]
pub extern "C" fn poc_init() {
    unsafe extern "C" {
        fn __wasm_call_ctors();
    }
    unsafe { __wasm_call_ctors() };
    std::panic::set_hook(Box::new(|info| {
        let msg = format!("panic: {info}");
        WasmSink.emit(export::Kind::Error, msg.as_bytes());
    }));
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .use_current_thread()
        .build_global();
    abgen::bc7_pure::set_encode_hook(gpu_host_encode);
}

/// Converts one request blob out of linear memory; layout in
/// `abgen::export::wire`.
#[unsafe(no_mangle)]
pub extern "C" fn poc_convert(ptr: *const u8, len: usize) -> i32 {
    let buf = unsafe { std::slice::from_raw_parts(ptr, len) };
    export::run(buf, &WasmSink, HOST)
}

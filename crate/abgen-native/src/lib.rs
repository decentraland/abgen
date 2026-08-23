#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::todo,
    clippy::dbg_macro
)]
#![allow(clippy::missing_safety_doc)]

use std::os::raw::{c_char, c_void};
use std::panic::{catch_unwind, AssertUnwindSafe};

use abgen_core::export::{self, HostInfo, Kind, Sink};
use abgen_core::texencode_cache::{self, CacheProfile};

pub const ABI_VERSION: u32 = 1;

const HOST: HostInfo = HostInfo::new("v-abgen-native", "native://embedded");

pub const ABGEN_OK: i32 = 0;
pub const ABGEN_ERR_MALFORMED_INPUT: i32 = 1;
pub const ABGEN_ERR_CONVERT_FAILED: i32 = 2;
pub const ABGEN_ERR_NULL_ARG: i32 = 3;
pub const ABGEN_ERR_PANIC: i32 = 4;
pub const ABGEN_ERR_ALREADY_CONFIGURED: i32 = 5;

pub type AbgenEmitFn =
    unsafe extern "C" fn(user_data: *mut c_void, kind: u32, ptr: *const u8, len: usize);

struct CallbackSink {
    emit: AbgenEmitFn,
    user_data: *mut c_void,
}

impl Sink for CallbackSink {
    fn emit(&self, kind: Kind, bytes: &[u8]) {
        // SAFETY: non-null when accepted, slice outlives the call; that it
        unsafe { (self.emit)(self.user_data, kind as u32, bytes.as_ptr(), bytes.len()) }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn abgen_abi_version() -> u32 {
    ABI_VERSION
}

#[unsafe(no_mangle)]
pub const extern "C" fn abgen_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), '\0').as_ptr().cast()
}

#[unsafe(no_mangle)]
pub extern "C" fn abgen_set_max_threads(threads: u32) -> i32 {
    let n = (threads as usize).max(1);
    match rayon::ThreadPoolBuilder::new()
        .num_threads(n)
        .build_global()
    {
        Ok(()) => ABGEN_OK,
        Err(_) => ABGEN_ERR_ALREADY_CONFIGURED,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn abgen_alloc(len: usize) -> *mut u8 {
    if len == 0 {
        return std::ptr::null_mut();
    }
    match std::alloc::Layout::array::<u8>(len) {
        // SAFETY: len != 0, so the layout is non-zero-sized.
        Ok(layout) => unsafe { std::alloc::alloc(layout) },
        Err(_) => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn abgen_free(ptr: *mut u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    if let Ok(layout) = std::alloc::Layout::array::<u8>(len) {
        // SAFETY: caller states this came from `abgen_alloc` with this `len`.
        unsafe { std::alloc::dealloc(ptr, layout) }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn abgen_convert(
    request: *const u8,
    request_len: usize,
    emit: Option<AbgenEmitFn>,
    user_data: *mut c_void,
) -> i32 {
    let Some(emit) = emit else {
        return ABGEN_ERR_NULL_ARG;
    };
    if request.is_null() || request_len == 0 {
        return ABGEN_ERR_NULL_ARG;
    }

    let sink = CallbackSink { emit, user_data };

    // Idempotent, so declaring per call is fine.
    texencode_cache::enable_with_profile(CacheProfile::Client);

    // SAFETY: non-null and non-zero, checked above; caller keeps it valid.
    let bytes = unsafe { std::slice::from_raw_parts(request, request_len) };

    match catch_unwind(AssertUnwindSafe(|| export::run(bytes, &sink, HOST))) {
        Ok(code) => code,
        Err(payload) => {
            let msg = panic_message(&payload);
            let _ = catch_unwind(AssertUnwindSafe(|| {
                sink.emit_error(&format!("abgen panicked: {msg}"))
            }));
            ABGEN_ERR_PANIC
        }
    }
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use abgen_core::export::InputBuilder;

    #[derive(Default)]
    struct Captured {
        kinds: Vec<u32>,
        errors: Vec<String>,
        outputs: usize,
    }

    unsafe extern "C" fn capture(ud: *mut c_void, kind: u32, ptr: *const u8, len: usize) {
        let bytes = if len == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(ptr, len) }
        };
        let Some(c) = (unsafe { ud.cast::<Captured>().as_mut() }) else {
            return;
        };
        c.kinds.push(kind);
        if kind == Kind::Error as u32 {
            c.errors.push(String::from_utf8_lossy(bytes).into_owned());
        }
        if kind == Kind::Output as u32 {
            c.outputs = c.outputs.saturating_add(1);
        }
    }

    fn run_capture(blob: &[u8]) -> (i32, Captured) {
        let mut captured = Captured::default();
        let rc = unsafe {
            abgen_convert(
                blob.as_ptr(),
                blob.len(),
                Some(capture),
                std::ptr::addr_of_mut!(captured).cast(),
            )
        };
        (rc, captured)
    }

    #[test]
    fn abi_version_is_stable() {
        assert_eq!(abgen_abi_version(), 1);
    }

    #[test]
    fn version_string_is_nul_terminated() {
        let p = abgen_version();
        assert!(!p.is_null());
        let s = unsafe { std::ffi::CStr::from_ptr(p) };
        assert_eq!(s.to_string_lossy(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn null_request_is_rejected_without_calling_back() {
        let rc = unsafe { abgen_convert(std::ptr::null(), 0, Some(capture), std::ptr::null_mut()) };
        assert_eq!(rc, ABGEN_ERR_NULL_ARG);
    }

    #[test]
    fn null_callback_is_rejected() {
        let blob = InputBuilder::new().file("a.glb", vec![1u8]).build();
        let rc = unsafe { abgen_convert(blob.as_ptr(), blob.len(), None, std::ptr::null_mut()) };
        assert_eq!(rc, ABGEN_ERR_NULL_ARG);
    }

    #[test]
    fn zero_length_request_is_rejected() {
        let blob = [0u8; 4];
        let rc = unsafe { abgen_convert(blob.as_ptr(), 0, Some(capture), std::ptr::null_mut()) };
        assert_eq!(rc, ABGEN_ERR_NULL_ARG);
    }

    #[test]
    fn malformed_request_reports_through_the_callback() {
        let (rc, cap) = run_capture(&[0xff, 0xff, 0xff, 0xff, 0x00]);
        assert_eq!(rc, ABGEN_ERR_MALFORMED_INPUT);
        assert_eq!(cap.errors, vec!["malformed input blob".to_string()]);
        assert_eq!(cap.outputs, 0);
    }

    #[test]
    fn a_corrupt_model_fails_the_file_not_the_process() {
        let blob = InputBuilder::new()
            .file("evil.glb", vec![0xde, 0xad, 0xbe, 0xef, 0x00, 0x01])
            .platform("windows")
            .build();
        let (rc, cap) = run_capture(&blob);
        assert_eq!(rc, ABGEN_OK, "a bad asset is a file error, not a run error");
        assert_eq!(
            cap.outputs, 0,
            "nothing should be emitted for a corrupt glb"
        );
        assert!(
            cap.kinds.contains(&(Kind::Manifest as u32)),
            "the run should still finish with a manifest"
        );
    }

    #[test]
    fn alloc_free_roundtrip() {
        let p = abgen_alloc(64);
        assert!(!p.is_null());
        unsafe { abgen_free(p, 64) };
        assert!(abgen_alloc(0).is_null());
        unsafe { abgen_free(std::ptr::null_mut(), 0) };
    }
}

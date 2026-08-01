#![allow(
    clippy::type_complexity,
    clippy::inherent_to_string,
    clippy::too_many_arguments,
    clippy::needless_range_loop
)]

#[cfg(all(not(target_os = "windows"), not(target_arch = "wasm32")))]
#[global_allocator]
static GLOBAL_ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[macro_use]
pub mod value;
pub mod gpu;
#[cfg(not(target_arch = "wasm32"))]
pub mod gpu_dispatch;
#[cfg(not(target_arch = "wasm32"))]
pub mod gpuhost;
pub mod scene;

pub mod alpha_bleed;
pub mod animation;
pub mod animation_mecanim;
pub mod cabname;
#[cfg(not(target_arch = "wasm32"))]
pub mod catalyst;
pub mod clihelp;
pub mod dates;
pub mod detmath;
pub mod draco;
#[cfg(not(target_arch = "wasm32"))]
pub mod glbscan;
pub mod gltf;
pub mod hashes;
#[cfg(not(target_arch = "wasm32"))]
pub mod live;
#[cfg(not(target_arch = "wasm32"))]
pub mod local_store;
pub mod lodgen;
pub mod lz4;
pub mod manifest;
pub mod materials;
pub mod mesh_layout;
pub mod naming;
pub mod normals;
pub mod pathids;
pub mod placeholder;
pub mod png;
pub mod resize;
pub mod ress;
pub mod sbp_order;
pub mod skeleton;
#[cfg(not(target_arch = "wasm32"))]
pub mod space;
pub mod tangents;
pub mod texprofile;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod tmppath;
#[cfg(not(target_arch = "wasm32"))]
pub mod worlds;

pub mod bc5_pure;
pub mod bc7_mode_tree;
pub mod bc7_pure;
pub mod dxt1_pure;
pub mod dxt_unity;
#[cfg(not(target_arch = "wasm32"))]
pub mod ffi;
#[cfg(target_arch = "wasm32")]
#[path = "ffi_wasm.rs"]
pub mod ffi;

pub mod unity;

pub mod builder;
pub mod bundle;
#[cfg(not(target_arch = "wasm32"))]
pub mod bvwebgpu;

pub mod compress;
/// The embeddable conversion export shared by the wasm bridge, the C-ABI
/// cdylib (`crate/abgen-native`) and the node addon (`crate/abgen-node`).
pub mod export;
pub mod lods;
#[cfg(not(target_arch = "wasm32"))]
pub mod regen;
pub mod shader;
pub mod validate;
#[cfg(not(target_arch = "wasm32"))]
pub mod wearables;

#[cfg(not(target_arch = "wasm32"))]
pub mod abcdn;

#[cfg(not(target_arch = "wasm32"))]
pub mod registry;

pub use anyhow::{anyhow, bail, Context, Result};

#[cfg(not(target_arch = "wasm32"))]
pub fn enable_gpu() -> std::result::Result<(), String> {
    gpu_dispatch::enable()
}

#[cfg(target_arch = "wasm32")]
pub fn enable_gpu() -> std::result::Result<(), String> {
    gpu::enable_wgpu_wasm()
}

pub struct GpuStatus {
    pub backend: &'static str,
    pub qualified: bool,
    pub reason: Option<String>,
}

#[cfg(not(target_arch = "wasm32"))]
pub fn gpu_status() -> Option<GpuStatus> {
    gpu::gpu_status()
}

#[cfg(target_arch = "wasm32")]
pub fn gpu_status() -> Option<GpuStatus> {
    gpu::gpu_status()
}

#[cfg(not(target_arch = "wasm32"))]
pub fn maybe_enable_gpu_from_env() {
    if gpu::backend_is_off() {
        return;
    }
    let explicit = clihelp::env_bool("ABGEN_GPU", false);
    if !explicit && gpu::auto_defaults_to_cpu() {
        eprintln!(
            "abgen-gpu: macOS default is CPU (integrated Metal is slower than the CPU for BC7); set ABGEN_GPU=1 or ABGEN_GPU_BACKEND=wgpu to force the GPU"
        );
        return;
    }
    if let Err(e) = enable_gpu() {
        if explicit {
            eprintln!("error: ABGEN_GPU set but no GPU available: {e}");
            std::process::exit(2);
        }
        eprintln!("warning: no GPU available ({e}); continuing on CPU");
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn arm_gpu_explicit() {
    if gpu::backend_is_off() {
        return;
    }
    if let Err(e) = enable_gpu() {
        eprintln!("error: --gpu: {e}");
        std::process::exit(2);
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn arm_gpu_default() {
    if gpu::backend_is_off() {
        return;
    }
    if gpu::auto_defaults_to_cpu() {
        eprintln!(
            "abgen-gpu: macOS default is CPU (integrated Metal is slower than the CPU for BC7); pass --gpu or set ABGEN_GPU_BACKEND=wgpu to force the GPU"
        );
        return;
    }
    if let Err(e) = enable_gpu() {
        eprintln!("warning: no GPU available ({e}); continuing on CPU");
    }
}

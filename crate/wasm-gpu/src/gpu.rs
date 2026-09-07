#[path = "corelib.rs"]
#[allow(dead_code, unused_imports, clippy::all)]
pub mod corelib;
#[allow(dead_code, unused_imports, clippy::all)]
#[path = "../../src/gpu/wgpu.rs"]
pub mod wgpu;
#[allow(dead_code, unused_imports, clippy::all)]
#[path = "../../src/gpu/wgpu_bc7/mod.rs"]
pub mod wgpu_bc7;

pub use corelib::bc7::Bc7Profile;
pub use wgpu::{init_gpu, Gpu};
pub use wgpu_bc7::{build_engine, encode_bc7_mip_chain_on, Engine};

//! abgen as a Node native module — the third host of `abgen::export`.
//!
//! `sdk-commands` otherwise spawns abgen as a sidecar and proxies to it
//! (js-sdk-toolchain#1504, abgen#21); requiring this makes conversion a
//! function call. The sidecar stays the right shape when the converter should
//! scale independently, or for a hard boundary against hostile content.
//! Surface modelled on `decentraland/asset-bundle-encoder`'s napi module.
//!
//! ```js
//! const { convert } = require('@dcl/abgen-node')
//!
//! const { bundles, events, manifest } = await convert({
//!   files: [{ name: 'model.glb', data: glbBuffer }],
//!   platform: 'windows',
//! })
//! ```

use abgen::export::{self, CollectingSink, HostInfo, InputBuilder};
use napi::bindgen_prelude::*;
use napi_derive::napi;
use std::sync::Once;

const HOST: HostInfo = HostInfo::new("v-abgen-node", "node://embedded");

/// One content file. `name` is the entity's content path.
#[napi(object)]
pub struct AbgenFile {
    pub name: String,
    pub data: Buffer,
}

#[napi(object)]
pub struct AbgenContentEntry {
    pub file: String,
    pub hash: String,
}

/// One produced bundle, named `<hash>_<deps>_<platform>`.
#[napi(object)]
pub struct AbgenBundle {
    pub name: String,
    pub data: Buffer,
}

/// A conversion job.
#[napi(object)]
pub struct AbgenConvertOptions {
    pub files: Vec<AbgenFile>,
    /// `"windows"` (default), `"mac"`, `"linux"` or `"webgl"`.
    pub platform: Option<String>,
    /// Omit to detect from the files.
    pub entity_type: Option<String>,
    /// 0 convert (default), 1 scan, 2 convert one model, 3 LOD only.
    pub mode: Option<u32>,
    pub magenta_missing: Option<bool>,
    pub bake_lod: Option<bool>,
    pub crop: Option<bool>,
    /// 0 leaves the LOD uncapped.
    pub triangle_cap: Option<u32>,
    /// Names the LOD (mode 3). Omit to derive from the content table.
    pub entity_hash: Option<String>,
    /// In mode 2, which file to convert.
    pub only_glb: Option<String>,
    /// The entity's full table, so cross-file dependency hashes resolve even
    /// when `files` carries only a shard.
    pub content_table: Option<Vec<AbgenContentEntry>>,
}

/// Everything one conversion produced.
#[napi(object)]
pub struct AbgenConvertResult {
    /// 0 on success; 1 malformed request; 2 conversion failed.
    pub code: i32,
    pub bundles: Vec<AbgenBundle>,
    /// Raw JSON progress events, in order.
    pub events: Vec<String>,
    pub errors: Vec<String>,
    pub manifest: Option<String>,
}

#[napi]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

static POOL: Once = Once::new();

/// Half the cores, because this host is the developer's own CLI.
///
/// The other two hosts want every core: the server converts as its whole job,
/// and Unity calls `abgen_set_max_threads` itself. Here conversion runs inside
/// `sdk-commands start`, so taking the machine for the duration interferes with
/// the editing that preview exists to serve.
fn default_pool() {
    POOL.call_once(|| {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads((cores / 2).max(1))
            .build_global();
    });
}

/// Overrides the pool size. Process-wide, effective once, and only before the
/// first [`convert`] — after that the default is already installed and this
/// returns false rather than pretending to have applied.
#[napi]
pub fn set_max_threads(threads: u32) -> bool {
    let mut applied = false;
    POOL.call_once(|| {
        applied = rayon::ThreadPoolBuilder::new()
            .num_threads((threads as usize).max(1))
            .build_global()
            .is_ok();
    });
    applied
}

/// Converts one entity on the blocking pool, not the libuv event loop:
/// conversion is CPU-bound for seconds at a time.
///
/// A model that fails is **not** a rejected promise — it comes back as a
/// `file-error` in `events` and a non-zero `exitCode` in `manifest`, `code`
/// still 0.
#[napi]
pub async fn convert(options: AbgenConvertOptions) -> Result<AbgenConvertResult> {
    let mut builder = InputBuilder::new()
        .platform(options.platform.unwrap_or_else(|| "windows".to_string()))
        .entity_type(options.entity_type.unwrap_or_default())
        .mode(options.mode.unwrap_or(0).min(u8::MAX as u32) as u8)
        .magenta(options.magenta_missing.unwrap_or(false))
        .lod(options.bake_lod.unwrap_or(false))
        .crop(options.crop.unwrap_or(false))
        .tri_cap(options.triangle_cap.unwrap_or(0))
        .entity_hash(options.entity_hash.unwrap_or_default())
        .only_glb(options.only_glb.unwrap_or_default());

    for f in options.files {
        builder = builder.file(f.name, f.data.to_vec());
    }
    for e in options.content_table.unwrap_or_default() {
        builder = builder.content_entry(e.file, e.hash);
    }
    let request = builder.build();
    default_pool();

    let collected = tokio::task::spawn_blocking(move || {
        let sink = CollectingSink::new();
        let code = export::run(&request, &sink, HOST);
        (code, sink.take())
    })
    .await
    .map_err(|e| Error::from_reason(format!("abgen conversion task failed: {e}")))?;

    let (code, out) = collected;
    Ok(AbgenConvertResult {
        code,
        bundles: out
            .outputs
            .into_iter()
            .map(|(name, data)| AbgenBundle {
                name,
                data: Buffer::from(data),
            })
            .collect(),
        events: out.events,
        errors: out.errors,
        manifest: out.manifest,
    })
}

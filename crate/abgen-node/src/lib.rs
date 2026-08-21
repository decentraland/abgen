use abgen::export::{self, CollectingSink, HostInfo, InputBuilder};
use napi::bindgen_prelude::*;
use napi_derive::napi;
use std::sync::Once;

const HOST: HostInfo = HostInfo::new("v-abgen-node", "node://embedded");

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

#[napi(object)]
pub struct AbgenBundle {
    pub name: String,
    pub data: Buffer,
}

#[napi(object)]
pub struct AbgenConvertOptions {
    pub files: Vec<AbgenFile>,
    pub platform: Option<String>,
    pub entity_type: Option<String>,
    pub mode: Option<u32>,
    pub magenta_missing: Option<bool>,
    pub bake_lod: Option<bool>,
    pub crop: Option<bool>,
    pub triangle_cap: Option<u32>,
    pub entity_hash: Option<String>,
    pub only_glb: Option<String>,
    pub content_table: Option<Vec<AbgenContentEntry>>,
}

#[napi(object)]
pub struct AbgenConvertResult {
    pub code: i32,
    pub bundles: Vec<AbgenBundle>,
    pub events: Vec<String>,
    pub errors: Vec<String>,
    pub manifest: Option<String>,
}

#[napi]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

static POOL: Once = Once::new();

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

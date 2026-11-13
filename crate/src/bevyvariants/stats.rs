//! Per-stage timings and outcome counts of a variants build. Cheap (`Instant::now` around each
//! stage), always on, and not part of any output that is hashed: the manifest and variant bytes
//! do not depend on it.
//!
//! Stage times are measured per file on the thread that builds it and summed over files, so in a
//! parallel run they add up to more than the wall time (and an encoder's own inner parallelism
//! is counted once, as the wall time of its call). With one thread they add up to about the
//! build's wall time.

use serde::Serialize;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// Time spent per stage, in nanoseconds.
#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
pub struct StageTimes {
    /// Probe of the variant key's record (the output directory, then the lane's probe).
    pub probe_ns: u64,
    /// Content fetch (local store read or network).
    pub fetch_ns: u64,
    /// PNG/JPEG decode to RGBA8.
    pub decode_ns: u64,
    /// Downscale to the variant size (only when the source is larger or not a power of two).
    pub resize_ns: u64,
    /// BC encode of the single level (no mip chain), alpha bleed included for BC7.
    pub bc1_ns: u64,
    /// BC1 quality check of an opaque sRGB image: level 0 decoded and compared with the source
    /// (`texture::BC1_MIN_PSNR_DB`); a rejected BC1 is followed by a BC7 encode (`bc7_ns`).
    pub bc1_check_ns: u64,
    pub bc5_ns: u64,
    pub bc7_ns: u64,
    /// UASTC -> BC7 block transcode (creator KTX2).
    pub transcode_ns: u64,
    /// Zstandard compression of every level plus KTX2 assembly.
    pub zstd_ns: u64,
    /// Brotli of a `glb-br` model variant or a GLB variant (its `.br` sidecar).
    pub brotli_ns: u64,
    /// CID (sha256) plus writing the variant file (and its `.br` sidecar).
    pub hash_write_ns: u64,
}

impl StageTimes {
    pub fn add(&mut self, o: &StageTimes) {
        self.probe_ns += o.probe_ns;
        self.fetch_ns += o.fetch_ns;
        self.decode_ns += o.decode_ns;
        self.resize_ns += o.resize_ns;
        self.bc1_ns += o.bc1_ns;
        self.bc1_check_ns += o.bc1_check_ns;
        self.bc5_ns += o.bc5_ns;
        self.bc7_ns += o.bc7_ns;
        self.transcode_ns += o.transcode_ns;
        self.zstd_ns += o.zstd_ns;
        self.brotli_ns += o.brotli_ns;
        self.hash_write_ns += o.hash_write_ns;
    }

    pub fn total_ns(&self) -> u64 {
        self.probe_ns
            + self.fetch_ns
            + self.decode_ns
            + self.resize_ns
            + self.bc1_ns
            + self.bc1_check_ns
            + self.bc5_ns
            + self.bc7_ns
            + self.transcode_ns
            + self.zstd_ns
            + self.brotli_ns
            + self.hash_write_ns
    }
}

/// Run `f`, adding its wall time to `slot`.
pub fn timed<T>(slot: &mut u64, f: impl FnOnce() -> T) -> T {
    let t = Instant::now();
    let out = f();
    *slot += elapsed_ns(t);
    out
}

pub fn elapsed_ns(t: Instant) -> u64 {
    u64::try_from(t.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// Why an original got no variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NoVariant {
    /// Content not available.
    Missing,
    /// Not a PNG, JPEG or KTX2 by its bytes.
    NotAnImage,
    /// 2 px or less on a side, or under 4 px on a side at the variant size (less than one
    /// 4x4 BC block).
    Tiny,
    /// PNG/JPEG that does not decode.
    DecodeError,
    /// KTX2 the UASTC header gate turns away (not UASTC, not 2D, ...).
    Ktx2HeaderGate,
    /// UASTC KTX2 without a level of at most 1024 on a side.
    Ktx2NoLevelInRange,
    /// The chosen level is not whole 4x4 blocks.
    Ktx2NotWholeBlocks,
    /// The transcoder refused or failed.
    Ktx2TranscodeFailed,
    /// Encode or write failed.
    EncodeError,
}

/// Where a texture variant came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    Png,
    Jpeg,
    Ktx2,
    Other,
}

/// One standalone texture's outcome.
#[derive(Clone, Debug, Serialize)]
pub struct FileStat {
    pub hash: String,
    pub file: String,
    /// `other` and `original_size` 0 when the variant was reused: the source was not fetched.
    pub source: SourceKind,
    pub original_size: u64,
    /// `ktx2-bc1` / `ktx2-bc5` / `ktx2-bc7`, or `None` with a `reason`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<&'static str>,
    /// The variant's key (`bevy/{prefix}/{name}`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant_cid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mips: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<NoVariant>,
    /// Opaque sRGB images only: PSNR (dB, RGB) of the BC1 encode's level 0 against the source
    /// at the variant size, and whether it fell below `texture::BC1_MIN_PSNR_DB` (then the
    /// variant is BC7).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bc1_psnr_db: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bc1_rejected: Option<bool>,
    /// The outcome came from the key's record (probe hit): nothing was fetched or encoded.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub reused: bool,
    pub times: StageTimes,
}

/// One model's `glb-br` outcome.
#[derive(Clone, Debug, Serialize)]
pub struct ModelStat {
    pub hash: String,
    pub file: String,
    /// 0 when the variant was reused: the source was not fetched.
    pub original_size: u64,
    /// `glb-br`, or `None` with a `reason`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<&'static str>,
    /// The variant's key (`bevy/{prefix}/{name}`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant_cid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant_size: Option<u64>,
    /// Size of the `.br` sidecar.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub br_size: Option<u64>,
    /// The `.br` sidecar's size when the size gate dropped it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gated_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<super::glbbr::NoModelVariant>,
    /// The outcome came from the key's record (probe hit): nothing was fetched or encoded.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub reused: bool,
    pub times: StageTimes,
}

/// One model's GLB variant (`g` class) outcome.
#[derive(Clone, Debug, Serialize)]
pub struct GlbStat {
    pub hash: String,
    pub file: String,
    /// 0 when the outcome was reused: the source was not fetched.
    pub original_size: u64,
    /// The variant's key (`bevy/{prefix}/{name}`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant_size: Option<u64>,
    /// Size of the `.br` sidecar (none when it was not smaller than the variant).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub br_size: Option<u64>,
    /// Why there is no variant: a `model::NoGlbVariant` name, or `error` / `missing` (not final).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The outcome came from the key's record (probe hit): nothing was fetched or encoded, and
    /// `images` is empty.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub reused: bool,
    /// Every embedded image's outcome (built outcomes only).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<super::model::EmbeddedImage>,
    pub times: StageTimes,
}

/// The GLB variants' (`g` class) totals. Byte totals cover the models built here (reused
/// outcomes carry no sizes of their originals or images).
#[derive(Clone, Debug, Default, Serialize)]
pub struct GlbSummary {
    /// Models considered.
    pub considered: u64,
    /// Outcomes taken from an existing record.
    pub reused: u64,
    /// Models that got a variant (built or reused).
    pub variants: u64,
    /// Built variants: their originals' bytes, the variants' bytes, and the `.br` sidecars'
    /// bytes (`br_count` of them).
    pub variants_original_bytes: u64,
    pub variant_bytes: u64,
    pub br_count: u64,
    pub br_bytes: u64,
    /// No-variant reason -> count.
    pub no_variant: BTreeMap<String, u64>,
    /// Embedded images converted, `"<format> from <source>"` -> count, original bytes and
    /// KTX2 bytes.
    pub images_converted: BTreeMap<String, CountBytes>,
    /// Embedded images left as their original bytes, by reason -> count and bytes.
    pub images_kept: BTreeMap<String, CountBytes>,
    /// GPU bytes of the embedded images of the models built here: as the explorer holds the
    /// originals, and as it holds what the variants carry (only images whose size is known).
    pub gpu_original_bytes: u64,
    pub gpu_variant_bytes: u64,
}

/// The model variants' (`glb-br`) totals.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ModelSummary {
    /// Models considered and their bytes.
    pub considered: u64,
    pub original_bytes: u64,
    /// Models that got a variant: count, their originals' bytes, the variants' bytes (equal
    /// to the originals') and the `.br` sidecars' bytes.
    pub variants: u64,
    pub variants_original_bytes: u64,
    pub variant_bytes: u64,
    pub br_bytes: u64,
    /// Size-gated attempts: count, original bytes, the dropped `.br` sidecars' bytes.
    pub size_gated: CountBytes,
    /// Other no-variant reasons -> count.
    pub no_variant: BTreeMap<String, u64>,
}

/// Count and bytes of one group.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct CountBytes {
    pub count: u64,
    pub original_bytes: u64,
    pub variant_bytes: u64,
}

/// A build's summary: stage times summed over files, outcomes grouped.
#[derive(Clone, Debug, Default, Serialize)]
pub struct BuildStats {
    pub zstd_level: i32,
    pub threads: usize,
    pub wall_ms: u64,
    pub textures: u64,
    pub models: u64,
    pub stages: StageTimes,
    /// `"<format> from <source>"` -> count and bytes (one row per original, so originals that
    /// share a variant count once each).
    pub variants: BTreeMap<String, CountBytes>,
    /// No-variant reason -> count and original bytes.
    pub no_variant: BTreeMap<String, CountBytes>,
    /// Opaque sRGB images whose BC1 fell below the PSNR floor and became BC7.
    pub bc1_rejected: u64,
    /// Outcomes taken from an existing record (probe hits): standalone textures and models
    /// that were neither fetched nor encoded.
    pub reused_textures: u64,
    pub reused_models: u64,
    /// Variant files written by this build (reused ones excluded) and their bytes.
    pub unique_variants: u64,
    pub unique_variant_bytes: u64,
    /// Originals considered (standalone textures) and their bytes.
    pub original_bytes: u64,
    pub files: Vec<FileStat>,
    /// The model variants (`glb-br`): totals and one row per model.
    pub model_variants: ModelSummary,
    pub model_files: Vec<ModelStat>,
    /// The GLB variants (`g`): totals and one row per model.
    pub glb_variants: GlbSummary,
    pub glb_files: Vec<GlbStat>,
}

impl BuildStats {
    pub fn from_files(files: Vec<FileStat>, zstd_level: i32, wall: Duration) -> Self {
        let mut s = BuildStats {
            zstd_level,
            threads: rayon::current_num_threads(),
            wall_ms: u64::try_from(wall.as_millis()).unwrap_or(u64::MAX),
            ..Default::default()
        };
        for f in &files {
            s.textures += 1;
            s.original_bytes += f.original_size;
            s.stages.add(&f.times);
            let src = serde_json::to_value(f.source)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default();
            let (key, map) = match (f.format, f.reason) {
                (Some(fmt), _) => (format!("{fmt} from {src}"), &mut s.variants),
                (None, Some(r)) => (
                    serde_json::to_value(r)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_string))
                        .unwrap_or_default(),
                    &mut s.no_variant,
                ),
                (None, None) => ("unknown".to_string(), &mut s.no_variant),
            };
            let e = map.entry(key).or_default();
            e.count += 1;
            e.original_bytes += f.original_size;
            e.variant_bytes += f.variant_size.unwrap_or(0);
            if f.bc1_rejected == Some(true) {
                s.bc1_rejected += 1;
            }
            if f.reused {
                s.reused_textures += 1;
            }
        }
        s.files = files;
        s
    }

    /// Add the model rows: totals and stage times.
    pub fn add_models(&mut self, rows: Vec<ModelStat>) {
        let g = &mut self.model_variants;
        for m in &rows {
            g.considered += 1;
            g.original_bytes += m.original_size;
            self.stages.add(&m.times);
            if m.reused {
                self.reused_models += 1;
            }
            match (&m.variant_size, m.reason) {
                (Some(n), _) => {
                    g.variants += 1;
                    g.variants_original_bytes += m.original_size;
                    g.variant_bytes += n;
                    g.br_bytes += m.br_size.unwrap_or(0);
                }
                (None, Some(super::glbbr::NoModelVariant::SizeGate)) => {
                    g.size_gated.count += 1;
                    g.size_gated.original_bytes += m.original_size;
                    g.size_gated.variant_bytes += m.gated_size.unwrap_or(0);
                }
                (None, r) => {
                    let key = r
                        .and_then(|r| serde_json::to_value(r).ok())
                        .and_then(|v| v.as_str().map(str::to_string))
                        .unwrap_or_else(|| "unknown".to_string());
                    *g.no_variant.entry(key).or_default() += 1;
                }
            }
        }
        self.model_files = rows;
    }

    /// Add the GLB variant rows: totals and stage times.
    pub fn add_glbs(&mut self, rows: Vec<GlbStat>) {
        let g = &mut self.glb_variants;
        let name = |v: serde_json::Result<serde_json::Value>| {
            v.ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_else(|| "unknown".to_string())
        };
        for m in &rows {
            g.considered += 1;
            self.stages.add(&m.times);
            if m.reused {
                g.reused += 1;
            }
            match (m.variant_size, &m.reason) {
                (Some(n), _) => {
                    g.variants += 1;
                    if !m.reused {
                        g.variants_original_bytes += m.original_size;
                        g.variant_bytes += n;
                    }
                    if let Some(b) = m.br_size {
                        g.br_count += 1;
                        g.br_bytes += b;
                    }
                }
                (None, r) => {
                    let key = r.clone().unwrap_or_else(|| "unknown".to_string());
                    *g.no_variant.entry(key).or_default() += 1;
                }
            }
            for i in &m.images {
                let src = name(serde_json::to_value(i.source));
                let e = match (i.format, i.reason) {
                    (Some(fmt), _) => g.images_converted.entry(format!("{fmt} from {src}")),
                    (None, r) => g.images_kept.entry(
                        r.map_or_else(|| "unknown".to_string(), |r| name(serde_json::to_value(r))),
                    ),
                }
                .or_default();
                e.count += 1;
                e.original_bytes += i.original_size;
                e.variant_bytes += i.variant_size.unwrap_or(i.original_size);
                if let (Some(a), Some(b)) = (i.original_gpu_bytes, i.variant_gpu_bytes) {
                    g.gpu_original_bytes += a;
                    g.gpu_variant_bytes += b;
                }
            }
        }
        self.glb_files = rows;
    }

    /// The summary as JSON without the per-file rows.
    pub fn summary_json(&self) -> serde_json::Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        if let Some(o) = v.as_object_mut() {
            o.remove("files");
            o.remove("model_files");
            o.remove("glb_files");
            let ms = |ns: u64| (ns as f64 / 1e6 * 10.0).round() / 10.0;
            let st = &self.stages;
            o.insert(
                "stages_ms".into(),
                serde_json::json!({
                    "probe": ms(st.probe_ns),
                    "fetch": ms(st.fetch_ns),
                    "decode": ms(st.decode_ns),
                    "resize": ms(st.resize_ns),
                    "bc1": ms(st.bc1_ns),
                    "bc1_check": ms(st.bc1_check_ns),
                    "bc5": ms(st.bc5_ns),
                    "bc7": ms(st.bc7_ns),
                    "transcode_uastc_bc7": ms(st.transcode_ns),
                    "zstd": ms(st.zstd_ns),
                    "brotli": ms(st.brotli_ns),
                    "hash_write": ms(st.hash_write_ns),
                    "total": ms(st.total_ns()),
                }),
            );
            o.remove("stages");
        }
        v
    }
}

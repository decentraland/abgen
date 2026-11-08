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
    /// Content fetch (local store read or network).
    pub fetch_ns: u64,
    /// PNG/JPEG decode to RGBA8.
    pub decode_ns: u64,
    /// Downscale to the variant size (only when the source is larger or not a power of two).
    pub resize_ns: u64,
    /// BC encode of the full chain, mip generation included (the encoders build each mip
    /// while encoding), alpha bleed included for BC7.
    pub bc1_ns: u64,
    pub bc5_ns: u64,
    pub bc7_ns: u64,
    /// UASTC -> BC7 block transcode (creator KTX2).
    pub transcode_ns: u64,
    /// Zstandard compression of every level plus KTX2 assembly.
    pub zstd_ns: u64,
    /// CID (sha256) plus writing the variant file.
    pub hash_write_ns: u64,
}

impl StageTimes {
    pub fn add(&mut self, o: &StageTimes) {
        self.fetch_ns += o.fetch_ns;
        self.decode_ns += o.decode_ns;
        self.resize_ns += o.resize_ns;
        self.bc1_ns += o.bc1_ns;
        self.bc5_ns += o.bc5_ns;
        self.bc7_ns += o.bc7_ns;
        self.transcode_ns += o.transcode_ns;
        self.zstd_ns += o.zstd_ns;
        self.hash_write_ns += o.hash_write_ns;
    }

    pub fn total_ns(&self) -> u64 {
        self.fetch_ns
            + self.decode_ns
            + self.resize_ns
            + self.bc1_ns
            + self.bc5_ns
            + self.bc7_ns
            + self.transcode_ns
            + self.zstd_ns
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NoVariant {
    /// Content not available.
    Missing,
    /// Not a PNG, JPEG or KTX2 by its bytes.
    NotAnImage,
    /// 2 px or less on a side.
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
    /// The BC7 variant is larger than the original KTX2.
    Ktx2SizeGate,
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
    pub source: SourceKind,
    pub original_size: u64,
    /// `ktx2-bc1` / `ktx2-bc5` / `ktx2-bc7`, or `None` with a `reason`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<&'static str>,
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
    /// The BC7 transcode's size when the size gate dropped it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gated_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<NoVariant>,
    pub times: StageTimes,
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
    /// Unique variant files written by this build and their bytes.
    pub unique_variants: u64,
    pub unique_variant_bytes: u64,
    /// Originals considered (standalone textures) and their bytes.
    pub original_bytes: u64,
    pub files: Vec<FileStat>,
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
        }
        s.files = files;
        s
    }

    /// The summary as JSON without the per-file rows.
    pub fn summary_json(&self) -> serde_json::Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        if let Some(o) = v.as_object_mut() {
            o.remove("files");
            let ms = |ns: u64| (ns as f64 / 1e6 * 10.0).round() / 10.0;
            let st = &self.stages;
            o.insert(
                "stages_ms".into(),
                serde_json::json!({
                    "fetch": ms(st.fetch_ns),
                    "decode": ms(st.decode_ns),
                    "resize": ms(st.resize_ns),
                    "bc1": ms(st.bc1_ns),
                    "bc5": ms(st.bc5_ns),
                    "bc7": ms(st.bc7_ns),
                    "transcode_uastc_bc7": ms(st.transcode_ns),
                    "zstd": ms(st.zstd_ns),
                    "hash_write": ms(st.hash_write_ns),
                    "total": ms(st.total_ns()),
                }),
            );
            o.remove("stages");
        }
        v
    }
}

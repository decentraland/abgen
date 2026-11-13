//! `glb-br` model variants: the original `.glb`, byte for byte, plus its Brotli `.br` sidecar
//! (stored as `{key}.br`, served with `Content-Encoding: br` to clients that accept it). The
//! variant is kept only when the sidecar is smaller than the original. Geometry is not
//! re-encoded, so a client needs no decoder beyond its glTF loader.
//!
//! `ABGEN_BEVY_MODELS=0` or `ABGEN_BEVY_PRECOMPRESS=none` turns model variants off.

use anyhow::{Context, Result};

/// The manifest `format` of the lane's model variants.
pub const GLB_BR_FORMAT: &str = "glb-br";
/// Brotli quality and window of the `.br` sidecar. Quality 9: within a few percent of 11's
/// size for a small fraction of its CPU time.
pub const BROTLI_QUALITY: u32 = 9;
pub const BROTLI_LGWIN: u32 = 22;
/// Brotli quality of a GLB variant's (`g` class) `.br` sidecar. Most of such a GLB is
/// zstd-compressed KTX2, which Brotli cannot shrink, so a higher quality spends its CPU on
/// bytes that do not compress; quality 5 still takes the geometry's redundancy.
pub const GLB_VARIANT_BROTLI_QUALITY: u32 = 5;

/// Why a model got no `glb-br` variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NoModelVariant {
    /// Content not available.
    Missing,
    /// The `.br` sidecar is not smaller than the original.
    SizeGate,
    /// Compressing or writing failed.
    Error,
    /// Not a `.glb`: a `.gltf` loads its buffers and images as separate files, so a copy of the
    /// JSON alone saves next to nothing (the lane's model variants are `.glb` only).
    NotGlb,
}

/// Brotli (quality [`BROTLI_QUALITY`], window [`BROTLI_LGWIN`]) of `data`: the `.br` sidecar.
pub fn brotli_compress(data: &[u8]) -> Result<Vec<u8>> {
    brotli_compress_with(data, BROTLI_QUALITY)
}

/// [`brotli_compress`] at another quality (window [`BROTLI_LGWIN`]).
pub fn brotli_compress_with(data: &[u8], quality: u32) -> Result<Vec<u8>> {
    use std::io::Write;
    let mut out = Vec::with_capacity(data.len() / 2);
    {
        let mut w = brotli::CompressorWriter::new(&mut out, 1 << 16, quality, BROTLI_LGWIN);
        w.write_all(data).context("brotli")?;
        w.flush().context("brotli flush")?;
    }
    Ok(out)
}

/// Brotli decode (for checks and tests).
pub fn brotli_decompress(data: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    brotli::Decompressor::new(data, 1 << 16)
        .read_to_end(&mut out)
        .context("brotli decode")?;
    Ok(out)
}

/// Path of the `.br` sidecar of a stored variant.
pub fn br_sidecar(path: &std::path::Path) -> std::path::PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".br");
    std::path::PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brotli_round_trips() {
        let data: Vec<u8> = (0..20_000u32).map(|i| (i % 37) as u8).collect();
        let br = brotli_compress(&data).unwrap();
        assert!(br.len() < data.len() / 10);
        assert_eq!(brotli_decompress(&br).unwrap(), data);
        assert_eq!(
            br_sidecar(std::path::Path::new("/x/bevy/contents/bafk")),
            std::path::PathBuf::from("/x/bevy/contents/bafk.br")
        );
    }
}

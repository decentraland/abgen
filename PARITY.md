# Parity posture

Where `abgen`'s reproduction of the Unity asset-bundle-converter's output is byte-exact, where it is
deterministic-but-configurable, and where it drops to render/visual equivalence.

## Byte parity: the deterministic profile

The deterministic profile's output is bit-identical across rebuilds and compile targets, held by two
gates:

- **Native reproducibility.** With the deterministic profile selected, 32 representative bundles
  reproduce bit-for-bit against a frozen baseline. Release binaries are additionally built twice from a
  clean tree in CI and required bit-identical before publishing.
- **Cross-target parity.** The native converter (with `ABGEN_JPEG_GLB_9C=1`) and the WebAssembly build
  agree on 56/56 measured bundle hashes. The wasm self-gate independently compares 26 windows/mac/webgl
  bundle hashes across 8 format fixtures (JPEG, crunched DXT5 normals, Draco, gamma,
  KHR_texture_transform, generated tangents, multi-material, a two-parcel scene with a baked LOD) — all
  bit-identical, wasm vs native.

Every byte-path transcendental is routed through a single pure-Rust math implementation on both
targets, so the two cannot drift.

## The native default trades bytes for fidelity on GLB-embedded JPEG

Native default JPEG decode uses turbojpeg: measurably closer to the upstream oracle but not
bit-reproducible across platforms (SIMD IDCT isn't portable). `ABGEN_JPEG_GLB_9C=1` switches to a
vendored libjpeg9c integer-ISLOW path that *is* bit-reproducible — the deterministic profile the byte
gates above are defined on.

Scoped to a small, bounded slice of output:

- It affects **only GLB-embedded JPEGs** — about **10 of 205** measured bundles. Standalone `.jpg`
  texture bundles are byte-identical under either decoder.
- On those ~10 bundles the deterministic path differs from upstream by a **worst-case mean channel
  delta of ~3/255** (turbojpeg: ~0.03/255); roughly half are ~equal either way.

The gap is sub-perceptual: deterministic is the right default for reproducibility, turbojpeg for
closest-to-upstream fidelity — hence the env switch, not a hard-coded choice.

## Remaining texture delta vs upstream is encoder block choice

Aside from JPEG decode, the largest residual per-texel differences are all **BC7-vs-BC7 at identical
dimensions and mip counts**: our encoder and upstream's pick different blocks/partitions for the same
input. Not closable byte-wise — two compliant BC7 encoders may disagree — and it lands in the
render-noise class, not a change in what a texture depicts.

## Wearables: render parity, not byte parity

Wearable bundles can **no longer be byte-gated against upstream by anyone**: upstream purged the
pre-2026 wearable asset-bundle payloads, and the remaining manifests have no payloads behind them.
Validated by **render/visual comparison** instead.

## The visual gate

The byte gates can't speak to encoder-block noise or wearables. The check for both is a **Unity render
gate**: convert with `abgen`, render the result and the upstream bundle in a Unity host, compare images
(screenshots + diff heatmaps) into pass/fail verdicts. The compare pipeline drives this stage; the
README documents pointing it at a Unity Editor.

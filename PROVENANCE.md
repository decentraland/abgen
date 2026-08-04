# Provenance

## Clean-room reimplementation

`abgen` is a clean-room reimplementation of the Decentraland Unity
[asset-bundle-converter](https://github.com/decentraland/asset-bundle-converter), derived from
**observed output bundles** and **public format documentation** — UnityFS container layout, GLTF/GLB,
texture codecs — never from reading the converter's source. The LOD and converter lanes in particular:
where the format was ambiguous, the answer came from bytes produced by the reference pipeline, never
from its code.

## Reproducible releases

Each release is **versioned and tagged**; the release pipeline
([`.github/workflows/release.yml`](.github/workflows/release.yml)) builds every target **once**.
The Linux binaries are built by **Nix from the committed `flake.lock`** — a hermetic, pinned
derivation anyone can reproduce locally with `nix build`, which is a stronger guarantee than
re-running the same build in CI; the archives bundle the loader and libraries behind the `abgen`
entry script, so they run on any Linux with no host requirements. The Windows and macOS binaries
are built with a pinned Rust toolchain, `--locked` against the committed `Cargo.lock`, and a fixed
`SOURCE_DATE_EPOCH` — a constant, not the commit date, so the same source yields the same bytes at
any tag.

## Vendored shader bundles

The two platform-global `DCL/Scene` shader bundles under `crate/shader/` are generated Decentraland
content artifacts harvested from the production CDN and sha256-pinned (verified by
`scripts/bootstrap-runtime.sh`, hard-verified by the converter at load):

- `scene_ignore_windows` — sha256 `5a5ce6694c85b77be165e367fc510f2c8f06a05fa1422330fcff4c3793d6c4b5`, harvested from the production `ab-cdn.decentraland.org` v41 era; the canonical URL has since 404'd, so the vendored copy is the source of truth.
- `scene_ignore_mac` — sha256 `4c8519343778b9806d985129dc0c2c7b7ae97c17d0cfb17a30e66189ad591ce9`, harvested from <https://ab-cdn.decentraland.org/v38/bafkreieitxq5m5n64cj2gtxx3xyvfmb67qtmh5qdqgh6oqml7mxpkmexce/dcl/scene_ignore_mac> (the per-entity form is the only one the production CDN serves).

The remaining shader-allowlist names (`scene_ignore_linux`, `lit_ignore_*`, `scene_texarray_ignore_*`)
aren't vendored because they don't exist upstream: both URL forms 404 on the production CDN, so serving
404 for them is production parity, not a gap.

## Third-party components

The canonical license summary for abgen's vendored code. Three decoders are vendored as reduced source
subsets with local build shims, each under its upstream license: **crunch** (CRN/DXT transcoder,
Zlib), **draco** (mesh decoder, MIT / Apache-2.0), **libjpeg9c** (baseline JPEG, IJG license). Only the
decode paths `abgen` needs are included, plus thin C/C++ glue to build them in this tree; full
unmodified license texts live under [`crate/third_party/`](crate/third_party/).

# abgen - development guide

Building, running, testing, the toolchain, the platform matrix and the environment
variables are all in [README.md](README.md) — that is the single copy, so it cannot
drift out of step with a second one. Only the release process lives here.

## Releasing

The release pipeline (`.github/workflows/release.yml`) is plain shell on GitHub-hosted
runners; the only non-shell steps are Determinate Systems' sha-pinned nix installer on
the linux legs and first-party actions/cache everywhere (rustup legs: cargo registry +
target dirs; nix legs: a nix file store carrying the crane deps-derivation closure,
moved with nix copy). Measured rationale: warm cargo caches cut the rustup legs 2-4x,
and the nix cache only pays because the flake splits dependency compilation into its
own crane derivation (keyed on manifests/lockfile, stable across source edits) -
without the split, a store cache replayed nothing. Every target builds **once**: Linux via
`nix build` from the committed flake.lock (hermetic; reproduce locally with `nix build` -
the archives bundle the loader + libs behind the `abgen` entry script and run on any
Linux, including NixOS); Windows and macOS via the pinned rustup toolchain with
a fixed `SOURCE_DATE_EPOCH`. Every release operation is idempotent, so job
re-runs converge on the full asset set.

1. Land a `chore: release X.Y.Z` PR bumping the crate version (Cargo.toml + Cargo.lock).
2. Tag the merge commit and push the tag:
   `git tag vX.Y.Z <merge-sha> && git push origin vX.Y.Z`
   Don't pre-create the release in the web UI: the pipeline adopts an existing release
   (that is how v0.11.0 first shipped assetless), but only a CI-created one carries the
   notes from `.github/release-notes.md`.
3. What runs: each leg builds, packages, smoke-tests (`--version` + `/readyz`; windows
   legs are cross-built and not smoke-run), and uploads its archive with `--clobber`.
   The `publish` job re-downloads the published assets, verifies them against the
   build-time hashes, uploads the aggregated `SHA256SUMS.txt`, and publishes seven npm
   packages (six platform binaries, then the `@dcl/abgen` connector); `napi-publish`
   publishes six more (five napi platform packages, then `@dcl/abgen-node`). Both go
   via npm Trusted Publishing: no `NPM_TOKEN`, each job's OIDC token is exchanged for a
   short-lived registry credential. All thirteen pin `decentraland/abgen` +
   `release.yml` as their trusted publisher, so renaming this workflow file breaks
   publishing until every package's configuration is updated.
4. Verify: 13 assets on the release page (6 `abgen-*` + 6 `abgen-native-*` archives, plus
   `SHA256SUMS.txt`), and that the release is no longer a draft.
5. Failed leg: re-run failed jobs from the Actions UI; everything converges.

`workflow_dispatch` on a branch is a build+smoke dry run: no release and no npm publish, but it
verifies the artifact hashes and uploads the archives as workflow artifacts, so a branch can be
tested on a real machine before a tag exists. `targets` narrows the matrix to one lane for
iteration; `record_hashes` re-records the manifest and refuses to run with `targets` set.

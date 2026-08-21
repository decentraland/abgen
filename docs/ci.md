# CI, derived backwards from the deliverables

Start from what ships, derive what must be built, then what must be
downloaded, then which hashes pin each step. Everything below is executed
by two scripts (`ci/build.sh`, `ci/napi.sh`) that run
identically on a laptop and in a workflow; the workflows are thin
schedulers around them.

## 1. Deliverables

| deliverable | targets | published to |
|---|---|---|
| `abgen` docker image | x86_64-linux, aarch64-linux | ghcr (multi-arch manifest) |
| `abgen-lambda` image | aarch64-linux | ECR (Graviton Lambda) |
| `abgen-<version>-<target>.tar.gz` (CLI) | 6 triples | GitHub Release |
| `abgen-native-<version>-<target>.tar.gz` (Unity lib + host) | 6 triples | GitHub Release |
| `@dcl/abgen-node` (napi addon) | 5 triples | npm |
| CLI npm wrapper (`npm/publish.sh`) | — | npm |

The 6 binary triples: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`,
`aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-pc-windows-gnu`,
`aarch64-pc-windows-gnullvm`.

## 2. Build classes (what each deliverable needs)

**Class nix** (both Linux triples, all images): one hermetic derivation
per arch builds `abgen`, `abgen-host`, `libabgen.{so,a}`; the images layer
the same closure. Needs: a Linux runner of the matching arch (no QEMU),
nix, the committed `flake.lock` + `Cargo.lock`. Nothing else.

**Class cargo** (darwin + windows triples): pinned rustup toolchain,
`cargo build --release --locked`. Per-triple extras:
- `aarch64-apple-darwin`: native on macos-15 (arm64). Xcode clang/cmake
  from the runner image.
- `x86_64-apple-darwin`: cross from the same runner (`rustup target add`);
  never executed on the builder (Rosetta smoke would prove nothing).
- `x86_64-pc-windows-gnu`: cross from ubuntu; `g++-mingw-w64-x86-64-posix`
  (apt), static libstdc++/crt so the artifacts are self-contained, and a
  content-addressed dlltool wrapper (`ci/stable-dlltool.sh`) because raw
  dlltool bakes temp paths into import libs.
- `aarch64-pc-windows-gnullvm`: cross from ubuntu with the pinned
  llvm-mingw tarball; static crt; LLVM's in-process COFF writer (no
  dlltool).
Windows execution coverage comes from ci.yml's real windows-2025 job, not
these cross legs.

**Class napi** (5 triples): node + `npm ci` + `napi build --release
--target <t>` on native runners (linux x86/arm, macos both, windows msvc).

## 3. Downloads, and the hash that pins each one

Everything fetched during a build is verified against a hash that is
committed to this repo (or transitively pinned by one that is). One
exception, noted last.

| download | pinned by | verified by |
|---|---|---|
| nixpkgs, crane | `flake.lock` narHashes | nix |
| cache.nixos.org store paths | nixpkgs pin | nix signature check |
| our binary cache (`file://`) | content-addressed | nix narinfo hash (fail-open: damage costs minutes, never a red job) |
| crates.io crates | `Cargo.lock` checksums | cargo / crane vendor |
| rustup toolchain 1.97.1 | `rust-toolchain.toml` + version in `rust-setup` action | rustup release signatures |
| llvm-mingw tarball | sha256 in `ci/build.sh` | `sha256sum -c` before extract |
| node + npm deps | setup-node pin + `package-lock.json` integrity | npm ci |
| **apt mingw-w64 (windows-gnu leg)** | ubuntu-24.04 archive (distro-trusted, not repo-pinned) | apt signatures only |

The apt gap is accepted: the bytes it contributes (`libstdc++.a`) are
digest-logged during the build, and any drift is caught downstream by the
artifact manifests (§4) before a tag can ship.

## 4. When hashes are computed, and from what

Three moments, in order:

1. **Pin time** (commit): the table above. Changing any pin changes the
   tree, which changes the artifact ids below.
2. **Build time** (deterministic outputs): `SOURCE_DATE_EPOCH=315532800`,
   `--remap-path-prefix $PWD=/build --remap-path-prefix $HOME=/home` (the
   nix sandbox gives this for free), `--no-insert-timestamp` on windows,
   and deterministic packaging (GNU tar `--sort=name --owner=0 --group=0
   --mtime=@epoch | gzip -n`). `ci/build.sh` re-packs every archive and
   asserts the bytes are identical before it will emit them. Result: the
   tarball hash is a pure function of the tree.
   - Two 12-hex artifact ids, one per build class, so a change only
     invalidates the artifacts it can actually alter. Rule: **every
     binary embeds the id that keys its own artifact**, so the stamp in
     `--version`/logs/`/status` names exactly the inputs that produced
     the bytes, and it rotates only when the bytes can.
     - `srcId` (`nix eval --raw .#srcId`) = hash of the filtered source
       tree — the cargo/napi legs' inputs (`Cargo.lock` and
       `rust-toolchain.toml` are in the tree). Keys and stamps the 4
       cargo archives and 5 napi artifacts.
     - `nixId` = hash of `srcId` + `flake.lock` + `flake.nix` +
       `nix/build.nix` — the nix legs additionally depend on that
       plumbing (nixpkgs toolchain, glibc). Keys and stamps the 2 nix
       archives and both images. A nixpkgs bump rebuilds those and
       leaves the other nine artifacts untouched.
     - Neither id can cover runner-provided tails (the mac runner's
       Xcode clang, apt's mingw): if those drift, the id stays but the
       bytes move — which is what the nightly comparison below catches.
       Commit-level provenance rides in the `BUILD-INFO.txt` sidecar and
       the GitHub attestation, never inside the archive bytes.
3. **Nightly reproducibility**: a scheduled release run rebuilds every
   leg from scratch and byte-compares the outputs against the stored
   artifacts for the same tree, uploading nothing. Inequality means the
   build stopped being reproducible (or a stored artifact was not
   produced from this tree) — red either way. There are no committed
   hash manifests: shipping trust is the fork-filtered same-repo
   artifact fetch plus `attest-build-provenance` binding the published
   bytes to the tag run.

## 5. The pipeline that falls out

- **build once, on main**: every push to main builds all 11 outputs as
  input-addressed workflow artifacts (`archives-<id>-<target>`,
  `napi-<srcId>-<target>`, `image-<nixId>-<name>`, where `<id>` is the
  class id from §4). A leg whose artifacts for this tree already exist
  is skipped at matrix-selection time.
- **tags publish, builds only if missing**: a tag looks up the same
  names (same-repo runs only — every artifact fetch filters on the
  creating run's head repository), waits for the commit's ci verdict,
  then attaches archives + SHA256SUMS + provenance attestations to the
  GitHub release, publishes both npm packages, and pushes the images
  (ghcr multi-arch manifest; ECR behind the `biz` environment).
- **ci is memoized by artifacts too**: lanes that pass upload
  `{nix,windows,node}-green-<treehash>` verdict artifacts (deny-list
  tree hash, `.github/actions/tree-hash`), and the nix binary cache is
  mirrored as a repo-wide artifact under its own key — so main's
  post-squash-merge run, which can never read PR-scoped actions/cache
  entries, skips or substitutes instead of rebuilding.
- **local = CI**: `ci/build.sh <triple>` and `ci/napi.sh <triple>` run
  the identical path on a laptop; the bytes match CI exactly when the
  toolchain matches (that's what §2 pins).

## 6. Testing the scripts

- `bash ci/build.sh aarch64-apple-darwin` on an arm Mac: full class-cargo
  leg incl. packaging, repro re-pack assert, and native smoke.
- `nix build .#abgen-native .#dockerImage` on either Linux arch: the
  entire class-nix leg minus staging.
- Workflows: `actionlint`.

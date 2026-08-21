# CI, derived backwards from the deliverables

Start from what ships, derive what must be built, then what must be
downloaded, then which hashes pin each step. Everything below is executed
by three scripts (`ci/build.sh`, `ci/napi.sh`, `ci/hashes.sh`) that run
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
       bytes move — which is precisely what the manifest verify in §3
       exists to catch. Commit-level provenance rides in
       `BUILD-INFO.txt`, not the bytes.
3. **Record / verify** (artifact manifests): `ci/artifact-hashes/<target>.sha256`
   holds the sha256 of the two shipped tarballs per target. Recorded by a
   real build (`ci/hashes.sh record`, via the release workflow's
   `record_hashes` dispatch — never hand-edited), then every later build
   of the same tree must reproduce those exact bytes: soft-warn on
   branches and main (nothing ships there), **hard-fail on tags**.
   Because archives are named by version (not git ref) and contain no
   ref-dependent bytes, the artifact a main push built *is* the artifact
   the tag publishes — promotion is a file copy, verified against the
   manifest, with no repack step.

## 5. The pipeline that falls out

- **build once, on main**: every push to main builds all 11 outputs as
  input-addressed workflow artifacts (`archives-<id>-<target>`,
  `napi-<srcId>-<target>`, `image-<nixId>-<name>`, where `<id>` is the
  class id from §4). A leg whose artifacts for this tree already exist
  is skipped at matrix-selection time.
- **tags publish, builds only if missing**: a tag looks up the same names
  (any completed run of this tree qualifies — the name embeds the tree
  hash and the manifest verify re-proves the bytes), waits for the
  commit's ci verdict, then attaches archives + SHA256SUMS + provenance
  attestations to the GitHub release, publishes both npm packages, and
  pushes the images (ghcr multi-arch manifest; ECR behind the `biz`
  environment).
- **local = CI**: `ci/build.sh <triple>` and `ci/napi.sh <triple>` run
  the identical path on a laptop; hashes recorded locally will match CI
  exactly when the toolchain matches (that's what §2 pins).

## 6. Testing the scripts

- `bash ci/build.sh aarch64-apple-darwin` on an arm Mac: full class-cargo
  leg incl. packaging, repro re-pack assert, and native smoke.
- `nix build .#abgen-native .#dockerImage` on either Linux arch: the
  entire class-nix leg minus staging.
- `ci/hashes.sh record/verify` round-trips on any files.
- Workflows: `actionlint`.

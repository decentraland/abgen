#![cfg_attr(target_arch = "wasm32", no_main)]
#![cfg(not(target_arch = "wasm32"))]

use abgen::catalyst::CatalystClient;
use abgen::lodgen::assemble;
use abgen::lodgen::simplify;
use abgen::lodgen::simplify_meshopt::SimplifyPolicy;
use abgen::lods;
use anyhow::{anyhow, bail, Context, Result};
use std::path::PathBuf;

mod qualify;

const BIN_NAME: &str = "abgen-lod";
const CATALYST: &str = "https://peer.decentraland.org/content";

fn usage() -> ! {
    abgen::clihelp::usage_error(usage_text());
}

/// Flag-cursor over argv: `next()` yields the current token, `val()` the
/// value of the flag just yielded (erroring exactly like the old inline
/// `need` closures: "<flag> needs a value").
struct Args<'a> {
    argv: &'a [String],
    i: usize,
}

impl<'a> Args<'a> {
    fn new(argv: &'a [String]) -> Self {
        Self { argv, i: 0 }
    }
    fn next(&mut self) -> Option<&'a String> {
        let a = self.argv.get(self.i);
        self.i += 1;
        a
    }
    fn val(&mut self) -> Result<&'a String> {
        let flag = &self.argv[self.i - 1];
        let v = self
            .argv
            .get(self.i)
            .ok_or_else(|| anyhow!("{flag} needs a value"))?;
        self.i += 1;
        Ok(v)
    }
}

fn atlas_mode(fixed: bool, adaptive: bool) -> abgen::lodgen::atlas::AtlasMode {
    if fixed {
        abgen::lodgen::atlas::AtlasMode::FullBleed
    } else if adaptive {
        abgen::lodgen::atlas::AtlasMode::Adaptive
    } else {
        abgen::lodgen::atlas::AtlasMode::Native
    }
}

fn ensure_parent(path: &std::path::Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

fn usage_text() -> &'static str {
    "abgen-lod — LOD asset-bundle builder

USAGE:
  abgen-lod bundle <src.glb> --entity <entityId> [--level 1]
            [--platform windows|mac|linux] [--out DIR] [--catalyst URL]
            [--base X,Y --parcels 'x,y;x,y;...'] [--timestamp N] [--vertical-clip H]
  abgen-lod placements (--coords X,Y | --scene <entityId>) [--iss auto|off]            [--catalyst URL] [--diff-iss ISS.json [--tol 1e-3]]
  abgen-lod parse-manifest <manifest.json> --scene <pointer|entityId>
            [--catalyst URL] [--diff-iss ISS.json [--tol 1e-3]]
  abgen-lod assemble (--scene <entityId|X,Y> | --entity-json FILE) -o out.glb
            [--catalyst URL] [--iss auto|off] [--cache DIR] [--level 1]            [--no-crop] [--no-atlas] [--raw-materials] [--max-size 256]
            [--padding 2] [--atlas-fixed] [--atlas-adaptive]
  abgen-lod atlas -i in.glb -o out.glb [--max-size 256] [--padding 2]
            [--atlas-mode meshbaker|native|adaptive|fullbleed]
            [--atlas-fixed] [--atlas-adaptive] [--crop-base X,Y --crop-parcels 'x,y;x,y;...']
  abgen-lod simplify -i in.glb -o out.glb [--ratio 0.1] [--tri-cap N]
            [--simplify-policy gltfpack-si|budget]
            [--simplifier meshopt|gltfpack] [--gltfpack PATH]
            [--allow-unsimplified]
  abgen-lod generate --scene <pointer|entityId> --out DIR
            [--platform windows|mac|linux[,windows|mac|linux...]]
            [--level 1] [--ratio 0.1] [--simplify-policy gltfpack-si|budget]
            [--tri-cap N|auto|parcels|off] [--atlas-max 2048]
            [--atlas-mode meshbaker|native|adaptive|fullbleed]
            [--atlas-fixed] [--atlas-adaptive] [--bake-order pre|post]
            [--no-crop] [--catalyst URL] [--iss auto|off]
            [--workdir DIR] [--cache DIR] [--simplifier meshopt|gltfpack]
            [--gltfpack PATH]
            [--allow-unsimplified] [--keep-glb] [--no-uv-reclamp] [--emissive]
            [--fidelity] [--gpu]
  abgen-lod compare <ours> <reference> [--json]
  abgen-lod qualify-corpus --out DIR [--report FILE] [--cache DIR] [-j JOBS]
            [--catalyst URL] [--worlds-url URL] [--platform windows,mac]
            [--level 0,1]
            [--city-min -150] [--city-max 150] [--no-city] [--no-worlds]
            [--world NAME[,NAME...]] [--entity-ids FILE]
            [--attempts 3] [--snapshot-passes 8]
            [--shard-count N --shard-index I]
            [--reference-cdn https://ab-cdn.decentraland.org]
compare: structural diff of two LOD bundles, each a local path or an http(s)
  URL (e.g. https://ab-cdn.decentraland.org/LOD/1/{sid}_1_mac): material,
  texture, mesh, vertex and triangle counts, bytes, and the per-texture
  format/size lists side by side, with ours-minus-reference deltas; then a
  per-material diff pairing materials by name and comparing everything the
  production converter's SetLODShaderMaterial writes (shader pptr, keyword
  set, render queue, RenderType tag, every saved float, colour vectors incl.
  _PlaneClipping/_VerticalClipping/_BaseColor, texture slot bindings with
  tiling/offset). Properties declared on one side only are counted, not
  flagged. No byte parity is implied (production textures went through a
  JPEG q85 round-trip and gltfpack's simplifier, ours do not). --json prints
  both inventories, the per-side texture/material detail, the delta and the
  material diff. Exits 0 whenever both bundles parse.
qualify-corpus: snapshots active Genesis City deployments from the configured
  Catalyst and all deployed scenes from the paginated Worlds API, converts
  immutable entity hashes into scratch output with bounded workers, rechecks
  the snapshot, and writes a versioned JSON report plus Explorer risk
  candidates. It never publishes. Any discovery, generation, self-gate, or
  snapshot-stability failure produces exit status 1. --reference-cdn BASE
  additionally fetches the production bundle at
  BASE/LOD/{level}/{sid}_{level}_{platform} for every built level/platform
  and records a `compare`-style inventory of both sides, the delta and the
  per-material property diff under each scene's `reference` list (a 404 is
  recorded as found=false, a fetch error as an error string; neither fails
  the scene). The summary counts compared/found/missing/errors, how many
  bundles match production on material count and on texture count, and how
  many agree on every shared material property.
Unresolved glTF sources: a scene whose code names a model file its deployment
  does not ship (a filename drift, or a bare identifier that was never a file)
  is built without those entities. The parser never had a content hash for
  them, the Explorer renders nothing there, and production's
  StaticSceneDescriptorBuilder skips them the same way (missingHashes). A WARN
  names the files, the qualify-corpus report records them per scene
  (placements.unresolved_src / unresolved_srcs), and the ISS descriptor never
  carries them downstream. A scene with nothing renderable left still fails.

bundle: stages <src.glb> as {entityIdLower}_{level}.glb and builds
  {out}/{entityIdLower}/LOD/{level}/{entityIdLower}_{level}_{platform}.
  Scene base/parcels are resolved the way the upstream converter does, unless
  --base/--parcels override: POST /entities/active on catalyst-style hosts
  (a stale/redeployed entity id does NOT resolve), GET /contents/{id} when
  the host is a worlds-content-server. An unresolvable entity is a warning,
  not an error: the bundle is built with zeroed plane/vertical clipping and
  a zero root position, matching the upstream Unity LOD converter.
placements: resolves the scene, then prints its GLB placement list as JSON.
  --iss auto (default) always executes the scene in the embedded SDK runtime
  (start + 90 simulated frames) and reads the renderer state it produced;
  main.crdt is only the runtime's initial state, never read as data, and
  production ISS is never consumed. --iss accepts only auto or off (an alias
  for auto).
  --diff-iss FILE is comparison-only and never supplies placements. Node is not required;
  generate --cache stores content-addressed inputs only; cached output never
  authorizes placements or survives an entity redeployment as scene truth.
  --manifest-builder is deprecated and ignored. --diff-iss FILE
  compares the list against a production InitialSceneState as a multiset
  (content hash + TRS within --tol per component, rotation sign-insensitive),
  prints `iss-diff: ours=N ref=M missing=A extra=B trs-mismatch=C` instead of
  the JSON and exits 1 when A+B+C > 0; pass --catalyst explicitly, the
  default resolves on peer.decentraland.org.
parse-manifest: reads a <sceneId>-lod-manifest.json written by the npm
  scene-lod-entities-manifest-builder, resolves the scene for its file->hash
  map, and prints the same pretty placement JSON as `placements` — the
  bridge scripts/lod-parity-oracle.sh diffs against the embedded runtime.
  --diff-iss / --tol behave as for `placements`.
assemble: resolves placements like `placements`, fetches every referenced GLB
  and generates the scene's SDK primitives (MeshRenderer box/sphere/plane/
  cylinder with their Material, explorer-exact geometry; production's
  descriptor never carried these, so `placements` only counts them on stderr)
  (--cache DIR caches content by hash; --entity-json FILE reads a catalyst
  entity document from disk instead of resolving --scene, for offline runs
  against a prestaged cache), bakes all instances into one flat
  merged GLB in glTF right-handed space (the bundler applies the RH->LH flip),
  crops it on x/z to the exact parcel rect (production crops the same way:
  geometry overhanging neighbouring parcels is clipped at the parcel line,
  not dropped; the +-0.05 margin exists only in the _PlaneClipping shader
  vector; disable with --no-crop), atlases it into per-alpha-class
  TextureBakeResult materials
  (disable with --no-atlas) and writes it to -o. --raw-materials (requires
  --no-atlas) keeps SOURCE material truth in the emitted glb instead of the
  LOD normalization: explicit source metallicFactor/roughnessFactor (spec
  default 1.0 written as 1.0, never forced to 0), metallicRoughnessTexture,
  normalTexture, doubleSided, and the UNfolded baseColorFactor with the raw
  emissiveFactor/emissiveTexture (+KHR_materials_emissive_strength) — the
  ground-truth reference lane for material-fidelity comparisons.
atlas: re-runs only the atlas stage on an existing merged GLB: dedupe +
  skyline-pack tiles into one square power-of-two atlas per alpha class,
  merge each class into a single primitive (welding duplicate verts), remap
  uvs. Buckets follow the glTF alphaMode: OPAQUE -> TextureBakeResult-mat,
  MASK -> TextureBakeResult-mat-cutout, BLEND ->
  TextureBakeResult-mat-transparent (the production MeshBaker names; the
  -metal bucket exists only under generate --fidelity), with one production
  quirk kept: KHR_materials_transmission materials land in -transparent
  whatever their alphaMode, because glTFast imports transmission as a
  blended surface. --atlas-mode picks
  the sizing policy. meshbaker (generate's default) reproduces
  lod-generator-unity's MeshBaker bake: sources over 1024 are downscaled
  first, the natural edge per bucket is ceil(sqrt(sum(min(w,1024) x
  min(h,1024)))) over its distinct textures, and every bucket shares ONE
  canvas edge clamp(next_pow2(max(ceil(max_natural x 0.1), 512)), 512,
  2048) capped by --max-size; padding 2, tiles at native texels, opaque
  atlas JPEG q85, cutout/transparent PNG; a bucket fed by exactly one source
  texture (untextured materials do not count) ships that texture with its
  tiling uvs instead of an atlas, unchanged unless its edge exceeds
  min(1024, --max-size), in which case it is downscaled to that edge as
  PNG. native (this subcommand's
  default, with --max-size 256) pins every canvas to --max-size, tiles at
  native texels, solid tiles fill the canvas, remainder alpha-bled, opaque
  JPEG under 512 else PNG. adaptive (--atlas-adaptive) is the old
  shrink-to-content bake (canvas shrunk to the packed extent, flat tiles
  8x8) for non-Unity consumers; fullbleed (--atlas-fixed) is the retired
  full-bleed bake (tiles scaled to fill). The LOD bundle converter clamps
  every texture to 512 BC7, so the GLB canvas edge above 512 only buys
  source fidelity for the bake. --crop-base/--crop-parcels clip the model to
  the parcel-union rects before atlasing (the generate stage order), for
  staged crop runs without a catalyst entity.
simplify: decimates a GLB. --simplify-policy picks the triangle target
  (default gltfpack-si; --tri-cap N selects budget). gltfpack-si is the
  production recipe `gltfpack -si <ratio> -se 0.01 -kn`: every primitive
  gets exactly one topology-preserving pass toward ceil(tris x ratio)
  bounded by the 1e-2 relative error, no cap, no sloppy retry, so the
  output scales with the source. budget is the parcel-scaled cap lane
  (--tri-cap, else ratio x input tris). --simplifier picks the backend
  (default from ABGEN_SIMPLIFIER, else meshopt). meshopt runs the in-crate
  meshoptimizer simplifier; under budget the tri budget is apportioned per
  primitive by triangle share, each primitive gets one topology-preserving
  pass with a loose error bound so the count target dominates, a sloppy
  (topology-ignoring) retry when that stops early above target, then
  orphan-vertex compaction; a capped result still over budget is a hard
  error. gltfpack shells out (gltfpack-si: -si <ratio> -se 0.01 -kn -noq;
  budget: -si <ratio> -noq; binary resolved --gltfpack > ABGEN_GLTFPACK >
  PATH): under budget with --tri-cap N, when the plain quality pass stays
  over the cap the ladder re-runs at the budget-true ratio (cap/source)
  escalating the error limit (-sp -se 0.03|0.1|0.3|1.0) and stops at the
  mildest rung that fits; -sa is a genuine last resort. A fit below 0.8*cap
  fills back toward the cap by bisecting -se on the quality path (ratio
  without -sa on a plain fit; -sa bisection only when the fit itself was
  -sa). In both backends inputs already satisfying ratio>=1 (+ cap) pass
  through untouched. The report names the policy. --allow-unsimplified
  copies the input through verbatim (loud warning) when the simplifier is
  unavailable or fails.
generate/placements/assemble run without node: abgen executes the current
  deployment's SDK in-process (QuickJS) and derives every placement from the
  state the scene reaches after its simulated frames.
generate: the full sync chain: resolve scene -> independently derive placements
  -> assemble -> crop -> atlas -> simplify -> bundle via the LOD build mode
  into {out}/{sceneId}/LOD/{level}/{sceneId}_{level}_{platform}, plus
  {out}/{sceneId}/LOD.manifest.json. --level takes a comma-separated list (default 1, the
  production level set; level 2 is refused; production stopped emitting
  it): every level shares
  ONE assemble/crop/atlas bake and gets its own simplify pass, staged
  {sceneId}_{level}.glb, bundles and self-gate table (labels L{level}: /
  L{level}:{platform}:). Level 0 = that bake un-decimated (ratio 1.0):
  always the pass-through lane, gltfpack is neither run nor resolved, and a
  numeric --tri-cap is ignored with a warning. This DIVERGES from legacy
  production LOD0 (a real-scene bundle with per-source meshes/materials on
  dcl/scene_ignore_windows per prod-inspection.md and the LOD0 section of
  PROD-CHARACTERIZATION.md); the ISS path is the
  production-current LOD0 replacement. At level 1 the decimation policy
  defaults to --simplify-policy gltfpack-si, production's `gltfpack -si 0.1
  -kn` recipe (default -se 1e-2): every primitive gets one
  topology-preserving pass toward ceil(tris x --ratio) bounded by the 1e-2
  relative error, never a cap, so the final mesh scales with the source
  (Tea Park, 28 parcels: 272542 source tris -> ~27k, production's _1.glb
  has 26758). --tri-cap N|auto|parcels|off switches to the budget policy:
  auto and parcels cap at 500 x parcels (the pre-parity default; scenes at
  or under the cap pass through bit-identically, larger ones are decimated
  with the -se escalation ladder into [0.8*cap, cap], hard error if the cap
  is unreachable), N caps at N, off is the uncapped ratio lane
  (pass-through at or under 500 x parcels, else ratio decimation).
  --simplify-policy gltfpack-si|budget names the policy explicitly; the
  last of --tri-cap/--simplify-policy wins and --ratio feeds both. Textures:
  --atlas-mode (default meshbaker, see atlas above) with --atlas-max 2048
  capping the canvas edge and the single-texture pass-through;
  --atlas-fixed/--atlas-adaptive are the fullbleed/adaptive shorthands.
  Bundle textures are BC7 square POT <= 512 (the LOD converter clamps) and
  the self-gate checks exactly that per texture, at or under
  min(next_pow2(largest image edge in the level's GLB), 512), plus
  material-buckets (every bundle material is one of the three
  production bucket names, four under --fidelity). --bake-order post
  reorders the chain to
  production's ordering: assemble -> crop -> raw multi-material GLB ->
  simplify -> re-ingest -> atlas -> bundle, so atlas UVs are baked onto the
  final decimated triangles and simplification can never smear them across
  atlas tiles; the default pre keeps the atlas-then-simplify chain.
  --simplifier picks the decimation backend
  exactly as in `simplify` above (default from ABGEN_SIMPLIFIER, else
  meshopt). Every budget-policy capped run adds a tri-cap self-gate
  check (tris_after <= cap); an --allow-unsimplified verbatim copy passes
  it with a recorded waiver. Unsupported renderer state forces SDK execution;
  zero placements or unresolved glTF sources afterward quarantine the run before publication.
  No persistent placement baseline is consulted or written. The crop stage
  (default on, matching
  production; --no-crop disables) clips merged geometry to the exact parcel
  rect and adds a crop-bounds self-gate check. --platform takes a
  comma-separated list (windows|mac|linux; webgl is refused — upstream webgl
  LOD bundles use an empty suffix and are unsupported here): every platform
  bundle is built from the same bake and simplify pass, listed in ONE union LOD.manifest.json, and self-gated
  separately (one gate table per platform, including a target-platform
  check: windows=19 mac=2 linux=24). Every run also writes the ISS
  descriptor {out}/{sceneId}/{sceneId}_InitialSceneState.json
  next to LOD.manifest.json — the production InitialSceneState shape
  ({version, sceneId, assets:[{hash, position, rotation, scale}]}) with the
  independently derived placements serialized verbatim in the pinned
  base-relative frame; the abcdn server serves it at
  /lods-unity/manifests/{sceneId}_InitialSceneState.json and an
  iss-descriptor self-gate check re-parses it. Every run ends with a
  structural self-gate; any FAIL exits nonzero. --keep-glb keeps the
  intermediate merged GLBs in the workdir. --emissive (default off) carries
  glTF emission through instead of folding emissiveFactor into the base
  colour: a second atlas per alpha class is baked with the same packed rects
  (emissive texel x factor in linear light, black for non-glowing sources)
  and bound as _EmissionMap with _EmissionColor white in the bundle, so
  glowing materials glow in the client; scenes with no glowing material
  emit no emission atlas. --fidelity (default off; off is byte-identical to
  the production-parity bake) restores source material state production
  normalizes away. Transparent class: double-sided glass gets its back
  faces (source doubleSided OR per alpha class -> _Cull 0) together with
  glTF BLEND depth semantics (_ZWrite 0 - the pair ships as one change),
  the forced 0.8 base alpha becomes the data-true per-texel atlas alpha,
  and the constant spec sheen is killed (_SpecColor 0.05,
  _SpecularHighlights/_EnvironmentReflections 0). The opaque class keeps
  production culling: ~95% of source opaque materials are doubleSided
  (exporter default), so restoring them would flip the whole class. Metal
  class: opaque materials with effective metallic >= 0.5 or a usable MR
  texture split into a 4th merged material carrying _Metallic/_Smoothness
  floats; when a source MR texture rides the same UV set and transform as
  _BaseMap it is carried per-texel into a metal-rough atlas plane on the
  base rects, repacked from glTF ORM to Unity layout (metallic = B x
  metallicFactor in R, smoothness = 1 - G x roughnessFactor in A, linear
  BC7) and bound as _MetallicGlossMap with the _METALLICSPECGLOSSMAP
  keyword and _Metallic/_Smoothness pinned to 1; factor-only metals keep
  the keyword-free float path and scenes without a usable MR texture emit
  no MR plane.

--help/-h prints this help; --version/-V prints the version."
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "abgen=warn".into()),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = argv.first() else { usage() };
    let rc = match cmd.as_str() {
        "bundle" => cmd_bundle(&argv[1..]),
        "placements" => cmd_placements(&argv[1..]),
        "parse-manifest" => cmd_parse_manifest(&argv[1..]),
        "assemble" => cmd_assemble(&argv[1..]),
        "atlas" => cmd_atlas(&argv[1..]),
        "simplify" => cmd_simplify(&argv[1..]),
        "generate" => cmd_generate(&argv[1..]),
        "compare" => cmd_compare(&argv[1..]),
        "qualify-corpus" => qualify::run(&argv[1..]),
        "-h" | "--help" => abgen::clihelp::print_help(usage_text()),
        "-V" | "--version" => abgen::clihelp::print_version(BIN_NAME),
        other => {
            eprintln!("unknown subcommand {other:?}");
            usage();
        }
    };
    match rc {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
    }
}

use abgen::lodgen::parse_parcel;

fn parse_parcels(s: &str) -> Result<Vec<(i32, i32)>> {
    let out = s
        .split(';')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(parse_parcel)
        .collect::<Result<Vec<_>>>()?;
    if out.is_empty() {
        bail!("--parcels {s:?} has no parcels");
    }
    Ok(out)
}

type EntityGeometry = ((i32, i32), Vec<(i32, i32)>);

fn entity_geometry(client: &CatalystClient, entity_id: &str) -> Result<EntityGeometry> {
    lods::resolve_scene_geometry(client, entity_id)
        .with_context(|| format!("resolve scene entity {entity_id}"))
}

fn cmd_bundle(argv: &[String]) -> Result<i32> {
    let mut src: Option<String> = None;
    let mut entity: Option<String> = None;
    let mut level: u32 = 1;
    let mut platform = "windows".to_string();
    let mut out = "lodgen-out".to_string();
    let mut catalyst = CATALYST.to_string();
    let mut base: Option<String> = None;
    let mut parcels: Option<String> = None;
    let mut timestamp: Option<i64> = None;
    let mut vertical_clip: Option<f64> = None;

    let mut a = Args::new(argv);
    while let Some(arg) = a.next() {
        match arg.as_str() {
            "--entity" => entity = Some(a.val()?.clone()),
            "--level" => level = a.val()?.parse().context("--level")?,
            "--platform" => platform = a.val()?.clone(),
            "--out" => out = a.val()?.clone(),
            "--catalyst" => catalyst = a.val()?.clone(),
            "--base" => base = Some(a.val()?.clone()),
            "--parcels" => parcels = Some(a.val()?.clone()),
            "--timestamp" => timestamp = Some(a.val()?.parse().context("--timestamp")?),
            "--vertical-clip" => vertical_clip = Some(a.val()?.parse().context("--vertical-clip")?),
            "-h" | "--help" => abgen::clihelp::print_help(usage_text()),
            other if other.starts_with("--") => bail!("unknown flag {other:?}"),
            other => {
                if src.is_some() {
                    bail!("unexpected positional {other:?}");
                }
                src = Some(other.to_string());
            }
        }
    }
    let src = src.ok_or_else(|| anyhow!("bundle needs a <src.glb> positional"))?;
    let entity = entity.ok_or_else(|| anyhow!("bundle needs --entity"))?;
    lods::validate_lod_platform(&platform)?;
    let sid = entity.to_lowercase();

    let client = CatalystClient::from_args(&catalyst, None);
    let (base_parcel, parcel_list) = match (&base, &parcels) {
        (Some(b), Some(p)) => (parse_parcel(b)?, parse_parcels(p)?),
        (None, None) => match entity_geometry(&client, &entity) {
            Ok(geometry) => geometry,
            Err(e) => {
                eprintln!(
                    "WARN: could not resolve scene entity {sid}: {e:#}; \
                     converting with zeroed clipping (upstream converter behavior)"
                );
                ((0, 0), Vec::new())
            }
        },
        _ => bail!("--base and --parcels must be given together"),
    };
    println!(
        "entity {sid}: base={},{} parcels={}",
        base_parcel.0,
        base_parcel.1,
        parcel_list.len()
    );
    let plane = lods::plane_clipping(&parcel_list);
    let vertical = match vertical_clip {
        Some(h) => [0.0, h, 0.0, 0.0],
        None => lods::vertical_clipping(parcel_list.len()),
    };
    println!(
        "planeClipping=({},{},{},{}) verticalClipping=({},{},{},{}) clientPlacement=({},{},{})",
        plane[0],
        plane[1],
        plane[2],
        plane[3],
        vertical[0],
        vertical[1],
        vertical[2],
        vertical[3],
        lods::client_placement(base_parcel)[0],
        lods::client_placement(base_parcel)[1],
        lods::client_placement(base_parcel)[2]
    );

    let work = PathBuf::from(&out).join(".work");
    std::fs::create_dir_all(&work)?;
    let staged = work.join(format!("{sid}_{level}.glb"));
    std::fs::copy(&src, &staged).with_context(|| format!("copy {src} -> {}", staged.display()))?;

    let opts = lods::LodOptions {
        platform: platform.clone(),
        lod: Some(lods::LodGenMeta {
            parcels: parcel_list,
            base: base_parcel,
            timestamp,
            vertical_override: vertical_clip,
            fidelity: false,
        }),
        ..Default::default()
    };
    let conv = lods::convert_lods(
        &client,
        &[staged.to_string_lossy().into_owned()],
        &out,
        &opts,
    )?;
    for r in &conv.results {
        println!(
            "built {}/{}/{} ({} bytes)",
            out, r.scene_id, r.rel_path, r.bytes
        );
    }
    for (loc, err) in &conv.skipped {
        eprintln!("SKIP {loc}: {err}");
    }
    Ok(if conv.skipped.is_empty() { 0 } else { 1 })
}

fn warn_manifest_builder_ignored() {
    eprintln!("deprecated: --manifest-builder is ignored; the scene runtime is embedded");
}

/// `--diff-iss`: compares `ours` against a production InitialSceneState as a
/// multiset, prints the one-line summary on stdout (details on stderr) and
/// maps any difference to exit code 1.
fn report_iss_diff(
    ours: &[abgen::lodgen::placements::Placement],
    reference_path: &str,
    tol: f64,
) -> Result<i32> {
    let bytes =
        std::fs::read(reference_path).with_context(|| format!("read ISS {reference_path}"))?;
    let reference = abgen::lodgen::placements::parse_iss(&bytes)?;
    let diff = abgen::lodgen::placements::diff_iss(ours, &reference, tol);
    for line in &diff.details {
        eprintln!("{line}");
    }
    println!("{}", diff.summary());
    Ok(if diff.is_clean() { 0 } else { 1 })
}

fn parse_tol(v: &str) -> Result<f64> {
    let tol: f64 = v
        .parse()
        .with_context(|| format!("--tol {v:?} is not a number"))?;
    if tol.is_nan() || tol < 0.0 {
        bail!("--tol must be >= 0");
    }
    Ok(tol)
}

const ISS_DIFF_DEFAULT_TOL: f64 = 1e-3;

fn cmd_placements(argv: &[String]) -> Result<i32> {
    let mut coords: Option<String> = None;
    let mut scene: Option<String> = None;
    let mut iss = "auto".to_string();
    let mut catalyst = CATALYST.to_string();
    let mut diff_iss: Option<String> = None;
    let mut tol = ISS_DIFF_DEFAULT_TOL;

    let mut a = Args::new(argv);
    while let Some(arg) = a.next() {
        match arg.as_str() {
            "--coords" => coords = Some(a.val()?.clone()),
            "--scene" => scene = Some(a.val()?.clone()),
            "--iss" => iss = a.val()?.clone(),
            "--catalyst" => catalyst = a.val()?.clone(),
            "--diff-iss" => diff_iss = Some(a.val()?.clone()),
            "--tol" => tol = parse_tol(a.val()?)?,
            "--manifest-builder" => {
                a.val()?;
                warn_manifest_builder_ignored();
            }
            "-h" | "--help" => abgen::clihelp::print_help(usage_text()),
            other => bail!("unknown placements arg {other:?}"),
        }
    }
    let target = match (&coords, &scene) {
        (Some(c), None) => {
            parse_parcel(c)?;
            c.clone()
        }
        (None, Some(s)) => s.clone(),
        _ => bail!("placements needs exactly one of --coords or --scene"),
    };

    let client = CatalystClient::from_args(&catalyst, None);
    let ent = client
        .resolve_scene(&target)
        .with_context(|| format!("resolve scene {target:?}"))?;
    eprintln!("scene entity: {}", ent.entity_id);

    let full = abgen::lodgen::acquire_placements(&client, &ent, &iss)?;
    eprintln!(
        "primitives: {} ({} mesh-renderer skipped, {} missing textures); not part of the descriptor listing",
        full.primitives.len(),
        full.skipped_mesh_renderer,
        full.missing_textures
    );
    let list = full.placements;
    if let Some(reference) = diff_iss {
        return report_iss_diff(&list, &reference, tol);
    }
    println!("{}", serde_json::to_string_pretty(&list)?);
    Ok(0)
}

fn cmd_parse_manifest(argv: &[String]) -> Result<i32> {
    let mut manifest: Option<String> = None;
    let mut scene: Option<String> = None;
    let mut catalyst = CATALYST.to_string();
    let mut diff_iss: Option<String> = None;
    let mut tol = ISS_DIFF_DEFAULT_TOL;

    let mut a = Args::new(argv);
    while let Some(arg) = a.next() {
        match arg.as_str() {
            "--scene" => scene = Some(a.val()?.clone()),
            "--catalyst" => catalyst = a.val()?.clone(),
            "--diff-iss" => diff_iss = Some(a.val()?.clone()),
            "--tol" => tol = parse_tol(a.val()?)?,
            "-h" | "--help" => abgen::clihelp::print_help(usage_text()),
            other if other.starts_with("--") => bail!("unknown parse-manifest flag {other:?}"),
            other => {
                if manifest.is_some() {
                    bail!("unexpected positional {other:?}");
                }
                manifest = Some(other.to_string());
            }
        }
    }
    let manifest =
        manifest.ok_or_else(|| anyhow!("parse-manifest needs a <manifest.json> positional"))?;
    let target = scene.ok_or_else(|| anyhow!("parse-manifest needs --scene <pointer|entityId>"))?;

    let bytes = std::fs::read(&manifest).with_context(|| format!("read {manifest}"))?;
    let client = CatalystClient::from_args(&catalyst, None);
    let ent = client
        .resolve_scene(&target)
        .with_context(|| format!("resolve scene {target:?}"))?;
    eprintln!("scene entity: {}", ent.entity_id);
    let full = abgen::lodgen::placements::parse_lod_manifest_full(&bytes, &ent.content_by_file())?;
    eprintln!(
        "source: manifest ({} placements, {} primitives, {} mesh-renderer skipped, {} missing textures, \
         {} unresolved src, {} invisible skipped, {} excluded src)",
        full.placements.len(),
        full.primitives.len(),
        full.skipped_mesh_renderer,
        full.missing_textures,
        full.unresolved_src,
        full.invisible_skipped,
        full.excluded_src
    );
    if let Some(reference) = diff_iss {
        return report_iss_diff(&full.placements, &reference, tol);
    }
    println!("{}", serde_json::to_string_pretty(&full.placements)?);
    Ok(0)
}

fn cmd_assemble(argv: &[String]) -> Result<i32> {
    let mut scene: Option<String> = None;
    let mut entity_json: Option<String> = None;
    let mut out: Option<String> = None;
    let mut iss = "auto".to_string();
    let mut catalyst = CATALYST.to_string();
    let mut cache: Option<String> = None;
    let mut level: u32 = 1;
    let mut no_crop = false;
    let mut no_atlas = false;
    let mut raw_materials = false;
    let mut max_size: u32 = 256;
    let mut padding: u32 = 2;
    let mut atlas_fixed = false;
    let mut atlas_adaptive = false;

    let mut a = Args::new(argv);
    while let Some(arg) = a.next() {
        match arg.as_str() {
            "--scene" => scene = Some(a.val()?.clone()),
            "--entity-json" => entity_json = Some(a.val()?.clone()),
            "-o" | "--out" => out = Some(a.val()?.clone()),
            "--iss" => iss = a.val()?.clone(),
            "--catalyst" => catalyst = a.val()?.clone(),
            "--manifest-builder" => {
                a.val()?;
                warn_manifest_builder_ignored();
            }
            "--cache" => cache = Some(a.val()?.clone()),
            "--level" => level = a.val()?.parse().context("--level")?,
            "--no-crop" => no_crop = true,
            "--no-atlas" => no_atlas = true,
            "--raw-materials" => raw_materials = true,
            "--max-size" => max_size = a.val()?.parse().context("--max-size")?,
            "--padding" => padding = a.val()?.parse().context("--padding")?,
            "--atlas-fixed" => atlas_fixed = true,
            "--atlas-adaptive" => atlas_adaptive = true,
            "-h" | "--help" => abgen::clihelp::print_help(usage_text()),
            other => bail!("unknown assemble arg {other:?}"),
        }
    }
    let out = out.ok_or_else(|| anyhow!("assemble needs -o <out.glb>"))?;
    if raw_materials && !no_atlas {
        bail!("--raw-materials requires --no-atlas: the atlased lane would re-normalize the materials it claims to preserve");
    }

    let client = CatalystClient::from_args(&catalyst, None);
    let ent = match &entity_json {
        Some(path) => {
            let bytes = std::fs::read(path).with_context(|| format!("read entity json {path}"))?;
            let v: serde_json::Value = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse entity json {path}"))?;
            CatalystClient::parse_entity(&v)?
        }
        None => {
            let target = scene.ok_or_else(|| {
                anyhow!("assemble needs --scene <entityId|X,Y> or --entity-json FILE")
            })?;
            client
                .resolve_scene(&target)
                .with_context(|| format!("resolve scene {target:?}"))?
        }
    };
    eprintln!("scene entity: {}", ent.entity_id);

    let full = abgen::lodgen::acquire_placements(&client, &ent, &iss)?;
    eprintln!(
        "placements: {} primitives: {}",
        full.placements.len(),
        full.primitives.len()
    );

    let cache_dir = cache.as_deref().map(std::path::Path::new);
    if let Some(dir) = cache_dir {
        std::fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
    }
    let mut model = assemble::assemble(
        &client,
        &ent,
        &full.placements,
        &full.primitives,
        level,
        cache_dir,
        abgen::lodgen::model::MatLane {
            raw_materials,
            ..Default::default()
        },
    )?;
    if !no_crop {
        let (base, parcels) = abgen::lodgen::scene_geometry(&ent)?;
        let rects = abgen::lodgen::crop::crop_rects_rh(base, &parcels);
        let report = abgen::lodgen::crop::crop(&mut model, &rects);
        eprintln!("crop: {}", report.summary());
    }
    let model = if no_atlas {
        model
    } else {
        let mode = atlas_mode(atlas_fixed, atlas_adaptive);
        abgen::lodgen::atlas::atlas_with(&model, max_size, padding, mode, false)?
    };
    for line in &model.log {
        eprintln!("{line}");
    }
    for line in model.log.iter().filter(|l| l.starts_with("atlas:")) {
        println!("{line}");
    }

    let glb = abgen::lodgen::emit::emit_glb(&model)?;
    ensure_parent(std::path::Path::new(&out))?;
    std::fs::write(&out, &glb).with_context(|| format!("write {out}"))?;

    let summary = model
        .log
        .iter()
        .rev()
        .find(|l| l.starts_with("summary:"))
        .cloned()
        .unwrap_or_default();
    println!("{summary}");
    println!(
        "tris={} materials={} images={} bytes={}",
        model.total_tris(),
        model.materials.len(),
        model.images.len(),
        glb.len()
    );
    let (mn, mx) = model.bounds();
    println!(
        "aabb_rh min=({},{},{}) max=({},{},{})",
        mn[0], mn[1], mn[2], mx[0], mx[1], mx[2]
    );
    println!(
        "aabb_unity_local min=({},{},{}) max=({},{},{})",
        -mx[0], mn[1], mn[2], -mn[0], mx[1], mx[2]
    );
    if let Some(base) = ent
        .metadata
        .get("scene")
        .and_then(|s| s.get("base"))
        .and_then(|b| b.as_str())
        .and_then(|b| parse_parcel(b).ok())
    {
        let (bx, by) = (base.0 as f32 * 16.0, base.1 as f32 * 16.0);
        println!(
            "base={},{} aabb_unity_world min=({},{},{}) max=({},{},{})",
            base.0,
            base.1,
            -mx[0] + bx,
            mn[1],
            mn[2] + by,
            -mn[0] + bx,
            mx[1],
            mx[2] + by
        );
    }
    println!("wrote {out}");
    Ok(0)
}

fn cmd_atlas(argv: &[String]) -> Result<i32> {
    let mut input: Option<String> = None;
    let mut out: Option<String> = None;
    let mut max_size: u32 = 256;
    let mut padding: u32 = 2;
    let mut atlas_fixed = false;
    let mut atlas_adaptive = false;
    let mut mode_flag: Option<abgen::lodgen::atlas::AtlasMode> = None;
    let mut crop_base: Option<String> = None;
    let mut crop_parcels: Option<String> = None;

    let mut a = Args::new(argv);
    while let Some(arg) = a.next() {
        match arg.as_str() {
            "-i" | "--in" => input = Some(a.val()?.clone()),
            "-o" | "--out" => out = Some(a.val()?.clone()),
            "--max-size" => max_size = a.val()?.parse().context("--max-size")?,
            "--padding" => padding = a.val()?.parse().context("--padding")?,
            "--atlas-mode" => mode_flag = Some(abgen::lodgen::atlas::AtlasMode::parse(a.val()?)?),
            "--atlas-fixed" => atlas_fixed = true,
            "--atlas-adaptive" => atlas_adaptive = true,
            "--crop-base" => crop_base = Some(a.val()?.clone()),
            "--crop-parcels" => crop_parcels = Some(a.val()?.clone()),
            "-h" | "--help" => abgen::clihelp::print_help(usage_text()),
            other => bail!("unknown atlas arg {other:?}"),
        }
    }
    let input = input.ok_or_else(|| anyhow!("atlas needs -i <in.glb>"))?;
    let out = out.ok_or_else(|| anyhow!("atlas needs -o <out.glb>"))?;

    let bytes = std::fs::read(&input).with_context(|| format!("read {input}"))?;
    let stem = std::path::Path::new(&input)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("lod")
        .to_string();
    let mut model = abgen::lodgen::model::from_glb_bytes(&bytes, &stem)
        .with_context(|| format!("parse {input}"))?;
    match (&crop_base, &crop_parcels) {
        (Some(b), Some(p)) => {
            let base = parse_parcel(b).context("--crop-base")?;
            let parcels = parse_parcels(p).context("--crop-parcels")?;
            let rects = abgen::lodgen::crop::crop_rects_rh(base, &parcels);
            let report = abgen::lodgen::crop::crop(&mut model, &rects);
            eprintln!("crop: {}", report.summary());
        }
        (None, None) => {}
        _ => bail!("--crop-base and --crop-parcels must be given together"),
    }
    let mode = mode_flag.unwrap_or_else(|| atlas_mode(atlas_fixed, atlas_adaptive));
    eprintln!("atlas mode: {}", mode.name());
    let atlased = abgen::lodgen::atlas::atlas_with(&model, max_size, padding, mode, false)?;
    for line in atlased.log.iter().filter(|l| l.starts_with("atlas:")) {
        println!("{line}");
    }
    let glb = abgen::lodgen::emit::emit_glb(&atlased)?;
    ensure_parent(std::path::Path::new(&out))?;
    std::fs::write(&out, &glb).with_context(|| format!("write {out}"))?;
    println!(
        "tris_in={} tris_out={} materials={} images={} bytes={}",
        model.total_tris(),
        atlased.total_tris(),
        atlased.materials.len(),
        atlased.images.len(),
        glb.len()
    );
    if atlased.total_tris() != model.total_tris() {
        bail!(
            "atlas changed triangle count: {} -> {}",
            model.total_tris(),
            atlased.total_tris()
        );
    }
    println!("wrote {out}");
    Ok(0)
}

fn cmd_simplify(argv: &[String]) -> Result<i32> {
    let mut input: Option<String> = None;
    let mut out: Option<String> = None;
    let mut ratio: f64 = 0.1;
    let mut tri_cap: Option<u64> = None;
    let mut policy = SimplifyPolicy::default();
    let mut backend = simplify::SimplifierBackend::from_env();
    let mut gltfpack: Option<String> = None;
    let mut allow_unsimplified = false;

    let mut a = Args::new(argv);
    while let Some(arg) = a.next() {
        match arg.as_str() {
            "-i" | "--in" => input = Some(a.val()?.clone()),
            "-o" | "--out" => out = Some(a.val()?.clone()),
            "--ratio" => ratio = a.val()?.parse().context("--ratio")?,
            "--tri-cap" => {
                tri_cap = Some(a.val()?.parse().context("--tri-cap")?);
                policy = SimplifyPolicy::Budget;
            }
            "--simplify-policy" => policy = SimplifyPolicy::parse(a.val()?)?,
            "--simplifier" => backend = simplify::SimplifierBackend::parse(a.val()?)?,
            "--gltfpack" => gltfpack = Some(a.val()?.clone()),
            "--allow-unsimplified" => allow_unsimplified = true,
            "-h" | "--help" => abgen::clihelp::print_help(usage_text()),
            other => bail!("unknown simplify arg {other:?}"),
        }
    }
    let input = PathBuf::from(input.ok_or_else(|| anyhow!("simplify needs -i <in.glb>"))?);
    let out = PathBuf::from(out.ok_or_else(|| anyhow!("simplify needs -o <out.glb>"))?);
    ensure_parent(&out)?;

    let policy = policy.with_ratio(ratio as f32);
    let run_meshopt = || match policy {
        SimplifyPolicy::GltfpackSi {
            ratio,
            target_error,
        } => abgen::lodgen::simplify_meshopt::simplify_file_si(&input, &out, ratio, target_error),
        SimplifyPolicy::Budget => {
            abgen::lodgen::simplify_meshopt::simplify_file(&input, &out, ratio, tri_cap)
        }
    };
    let run_gltfpack = |bin: &std::path::Path| match policy {
        SimplifyPolicy::GltfpackSi {
            ratio,
            target_error,
        } => simplify::simplify_si(&input, &out, ratio, target_error, bin),
        SimplifyPolicy::Budget => simplify::simplify(&input, &out, ratio, tri_cap, bin),
    };
    let report = match backend {
        simplify::SimplifierBackend::Meshopt => {
            eprintln!(
                "simplifier: meshopt (in-crate meshoptimizer), policy {}",
                policy.name()
            );
            match run_meshopt() {
                Ok(r) => r,
                Err(e) if allow_unsimplified => {
                    eprintln!(
                        "WARNING: meshopt simplify failed ({e:#}); --allow-unsimplified passthrough"
                    );
                    simplify::copy_unsimplified(&input, &out)?
                }
                Err(e) => return Err(e),
            }
        }
        simplify::SimplifierBackend::Gltfpack => {
            match simplify::resolve_gltfpack(gltfpack.as_deref().map(std::path::Path::new)) {
                Ok(bin) => {
                    eprintln!("gltfpack: {} (policy {})", bin.display(), policy.name());
                    match run_gltfpack(&bin) {
                        Ok(r) => r,
                        Err(e) if allow_unsimplified => {
                            eprintln!(
                                "WARNING: gltfpack failed ({e:#}); --allow-unsimplified passthrough"
                            );
                            simplify::copy_unsimplified(&input, &out)?
                        }
                        Err(e) => return Err(e),
                    }
                }
                Err(e) if allow_unsimplified => {
                    eprintln!("WARNING: {e:#}; --allow-unsimplified passthrough");
                    simplify::copy_unsimplified(&input, &out)?
                }
                Err(e) => return Err(e),
            }
        }
    };
    println!("simplify: {}", report.summary());
    println!("wrote {}", out.display());
    Ok(0)
}

fn cmd_compare(argv: &[String]) -> Result<i32> {
    use abgen::lodgen::inventory::{diff_materials, load_locator, BundleInventory};

    let mut positional: Vec<String> = Vec::new();
    let mut json = false;
    let mut a = Args::new(argv);
    while let Some(arg) = a.next() {
        match arg.as_str() {
            "--json" => json = true,
            "-h" | "--help" => abgen::clihelp::print_help(usage_text()),
            other if other.starts_with('-') => bail!("unknown compare arg {other:?}"),
            other => positional.push(other.to_string()),
        }
    }
    let [ours_loc, ref_loc] = positional.as_slice() else {
        bail!("compare needs exactly two bundles: <ours> <reference>");
    };
    let load = |locator: &str| -> Result<BundleInventory> {
        let bytes = load_locator(locator)?.ok_or_else(|| anyhow!("{locator}: not found (404)"))?;
        abgen::lodgen::inventory(&bytes).with_context(|| format!("inventory {locator}"))
    };
    let ours = load(ours_loc)?;
    let reference = load(ref_loc)?;
    let delta = ours.delta_from(&reference);
    let materials = diff_materials(&ours, &reference);

    if json {
        let detail = |inv: &BundleInventory| {
            serde_json::json!({
                "textures": inv.texture_list,
                "materials": inv.material_list,
            })
        };
        let out = serde_json::json!({
            "ours": { "locator": ours_loc, "inventory": ours, "detail": detail(&ours) },
            "reference": { "locator": ref_loc, "inventory": reference, "detail": detail(&reference) },
            "delta": delta,
            "materials": materials,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(0);
    }

    println!("ours:      {ours_loc}");
    println!("reference: {ref_loc}");
    println!("{:<16}{:>14}{:>14}{:>14}", "", "ours", "reference", "delta");
    let row = |label: &str, a: u64, b: u64| {
        println!("{label:<16}{a:>14}{b:>14}{:>+14}", a as i64 - b as i64);
    };
    row("bytes", ours.bytes as u64, reference.bytes as u64);
    row(
        "materials",
        ours.materials as u64,
        reference.materials as u64,
    );
    row("textures", ours.textures as u64, reference.textures as u64);
    row(
        "texture_pixels",
        ours.texture_pixels,
        reference.texture_pixels,
    );
    row("meshes", ours.meshes as u64, reference.meshes as u64);
    row("vertices", ours.vertices, reference.vertices);
    row("triangles", ours.triangles, reference.triangles);
    println!("vertices delta vs reference: {:+.1}%", delta.vertices_pct);
    for (side, inv) in [("ours", &ours), ("reference", &reference)] {
        println!("textures[{side}]:");
        for t in &inv.texture_list {
            println!(
                "  {:<40} fmt={:<3} {}x{} mips={}",
                t.name, t.format, t.width, t.height, t.mips
            );
        }
    }
    println!(
        "materials: {} paired, {} only ours {:?}, {} only reference {:?}, {} shared-property mismatch(es); \
         properties declared on one side only: ours {} reference {}",
        materials.matched,
        materials.only_ours.len(),
        materials.only_ours,
        materials.only_reference.len(),
        materials.only_reference,
        materials.mismatches.len(),
        materials.props_only_ours,
        materials.props_only_reference
    );
    for m in &materials.mismatches {
        println!("  {m}");
    }
    if materials.identical() {
        println!("materials: IDENTICAL on every shared property");
    }
    Ok(0)
}

fn cmd_generate(argv: &[String]) -> Result<i32> {
    abgen::texencode_cache::enable_memory_only_with_profile(
        abgen::texencode_cache::CacheProfile::Batch,
    );
    abgen::decode_cache::enable();
    let mut params = abgen::lodgen::GenerateParams::default();
    let mut scene: Option<String> = None;
    let mut out: Option<String> = None;
    let mut gpu_flag = false;

    let mut a = Args::new(argv);
    while let Some(arg) = a.next() {
        match arg.as_str() {
            "--scene" => scene = Some(a.val()?.clone()),
            "--out" => out = Some(a.val()?.clone()),
            "--platform" => {
                let mut list: Vec<String> = a
                    .val()?
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                let mut seen = std::collections::HashSet::new();
                list.retain(|p| seen.insert(p.clone()));
                if list.is_empty() {
                    bail!("--platform needs at least one of windows|mac|linux");
                }
                for p in &list {
                    lods::validate_lod_platform(p)?;
                }
                params.platform = list[0].clone();
                params.platforms = list;
            }
            "--level" => {
                let mut list: Vec<u32> = Vec::new();
                for tok in a.val()?.split(',') {
                    let tok = tok.trim();
                    if tok.is_empty() {
                        continue;
                    }
                    list.push(tok.parse().context("--level")?);
                }
                params.levels = abgen::lodgen::normalize_levels(&list)?;
            }
            "--ratio" => params.ratio = a.val()?.parse().context("--ratio")?,
            "--simplify-policy" => params.simplify_policy = SimplifyPolicy::parse(a.val()?)?,
            "--tri-cap" => {
                params.simplify_policy = SimplifyPolicy::Budget;
                match a.val()?.as_str() {
                    "auto" | "parcels" => {
                        params.tri_cap = None;
                        params.tri_cap_auto = true;
                    }
                    "off" => {
                        params.tri_cap = None;
                        params.tri_cap_auto = false;
                    }
                    v => {
                        params.tri_cap = Some(v.parse().context("--tri-cap")?);
                        params.tri_cap_auto = false;
                    }
                }
            }
            "--atlas-max" => params.atlas_max = a.val()?.parse().context("--atlas-max")?,
            "--atlas-mode" => params.atlas_mode = abgen::lodgen::atlas::AtlasMode::parse(a.val()?)?,
            "--atlas-fixed" => params.atlas_mode = abgen::lodgen::atlas::AtlasMode::FullBleed,
            "--atlas-adaptive" => params.atlas_mode = abgen::lodgen::atlas::AtlasMode::Adaptive,
            "--bake-order" => match a.val()?.as_str() {
                "pre" => params.bake_after_simplify = false,
                "post" => params.bake_after_simplify = true,
                other => bail!("--bake-order must be pre|post, got {other:?}"),
            },
            "--no-crop" => params.crop = false,
            "--catalyst" => params.catalyst = a.val()?.clone(),
            "--iss" => params.iss = a.val()?.clone(),
            "--manifest-builder" => {
                a.val()?;
                warn_manifest_builder_ignored();
            }
            "--workdir" => params.workdir = Some(PathBuf::from(a.val()?)),
            "--cache" => params.cache = Some(PathBuf::from(a.val()?)),
            "--simplifier" => params.simplifier = simplify::SimplifierBackend::parse(a.val()?)?,
            "--gltfpack" => params.gltfpack = Some(PathBuf::from(a.val()?)),
            "--allow-unsimplified" => params.allow_unsimplified = true,
            "--keep-glb" => params.keep_glb = true,
            "--no-uv-reclamp" => params.uv_reclamp = false,
            "--emissive" => params.emissive_channel = true,
            "--fidelity" => params.fidelity = true,
            "--gpu" => gpu_flag = true,
            "-h" | "--help" => abgen::clihelp::print_help(usage_text()),
            other => bail!("unknown generate arg {other:?}"),
        }
    }
    if gpu_flag {
        abgen::arm_gpu_explicit();
    } else {
        abgen::arm_gpu_default();
    }
    params.scene = scene.ok_or_else(|| anyhow!("generate needs --scene <pointer|entityId>"))?;
    params.out_dir = out.ok_or_else(|| anyhow!("generate needs --out DIR"))?;
    params.simplify_policy = params.simplify_policy.with_ratio(params.ratio as f32);

    let outcome = abgen::lodgen::generate(&params)?;
    for line in &outcome.log {
        eprintln!("{line}");
    }
    println!(
        "entity={} scene_id={} source_tris={}",
        outcome.entity_id, outcome.scene_id, outcome.source_tris
    );
    for lb in &outcome.levels {
        println!(
            "level={} final_tris={} bundle_bytes={} rel={}",
            lb.level, lb.simplify.tris_after, lb.bundle_bytes, lb.rel_path
        );
        println!("simplify[{}]: {}", lb.level, lb.simplify.summary());
        if let Some(glb) = &lb.glb_path {
            println!("kept glb[{}]: {}", lb.level, glb.display());
        }
        println!("bundle[{}]: {}", lb.level, lb.bundle_path.display());
    }
    for c in &outcome.gate {
        println!(
            "{} self-gate {}: {}",
            if c.ok { "PASS" } else { "FAIL" },
            c.label,
            c.detail
        );
    }
    let failures = abgen::lodgen::gate_failures(&outcome.gate);
    if failures == 0 {
        println!("SELF-GATE PASSED ({} checks)", outcome.gate.len());
        Ok(0)
    } else {
        println!(
            "SELF-GATE FAILED ({failures} of {} checks)",
            outcome.gate.len()
        );
        Ok(1)
    }
}

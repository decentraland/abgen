#![cfg_attr(target_arch = "wasm32", no_main)]
#![cfg(not(target_arch = "wasm32"))]

use abgen::catalyst::CatalystClient;
use abgen::lodgen::assemble;
use abgen::lodgen::simplify;
use abgen::lods;
use anyhow::{anyhow, bail, Context, Result};
use std::path::PathBuf;

mod compare;

use compare::cmd_compare;

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
    "abgen-lod — LOD asset-bundle builder + structural comparator

USAGE:
  abgen-lod bundle <src.glb> --entity <entityId> [--level 1]
            [--platform windows|mac|linux] [--out DIR] [--catalyst URL]
            [--base X,Y --parcels 'x,y;x,y;...'] [--timestamp N] [--vertical-clip H]
  abgen-lod compare <ours> <prod> [--prod-ab vNN] [--allow-legacy]
  abgen-lod placements (--coords X,Y | --scene <entityId>) [--iss FILE|auto|off]
            [--catalyst URL]
  abgen-lod parse-manifest <manifest.json> --scene <pointer|entityId>
            [--catalyst URL]
  abgen-lod assemble (--scene <entityId|X,Y> | --entity-json FILE) -o out.glb
            [--catalyst URL] [--iss FILE|auto|off] [--cache DIR] [--level 1]
            [--no-crop] [--no-atlas] [--raw-materials] [--max-size 256]
            [--padding 2] [--atlas-fixed] [--atlas-adaptive]
  abgen-lod atlas -i in.glb -o out.glb [--max-size 256] [--padding 2]
            [--atlas-fixed] [--atlas-adaptive] [--crop-base X,Y --crop-parcels 'x,y;x,y;...']
  abgen-lod simplify -i in.glb -o out.glb [--ratio 0.1] [--tri-cap N]
            [--simplifier meshopt|gltfpack] [--gltfpack PATH]
            [--allow-unsimplified]
  abgen-lod generate --scene <pointer|entityId> --out DIR
            [--platform windows|mac|linux[,windows|mac|linux...]]
            [--level 0,1] [--ratio 0.1] [--tri-cap N|auto|off] [--atlas-max 256]
            [--atlas-fixed] [--atlas-adaptive] [--bake-order pre|post]
            [--no-crop] [--catalyst URL] [--iss FILE|auto|off]
            [--workdir DIR] [--cache DIR] [--simplifier meshopt|gltfpack]
            [--gltfpack PATH]
            [--allow-unsimplified] [--keep-glb] [--no-uv-reclamp] [--emissive]
            [--fidelity] [--gpu]

bundle: stages <src.glb> as {entityIdLower}_{level}.glb and builds
  {out}/{entityIdLower}/LOD/{level}/{entityIdLower}_{level}_{platform} (+.br).
  Scene base/parcels are resolved the way the upstream converter does, unless
  --base/--parcels override: POST /entities/active on catalyst-style hosts
  (a stale/redeployed entity id does NOT resolve), GET /contents/{id} when
  the host is a worlds-content-server. An unresolvable entity is a warning,
  not an error: the bundle is built with zeroed plane/vertical clipping and
  a zero root position, matching the upstream Unity LOD converter.
compare: parses both bundles and prints PASS/FAIL per structural check; exits 1 on FAIL.
  --prod-ab passes the reference build's asset-bundle version (from the
  asset-bundle-registry — it is NOT recorded inside the bundle): versions
  before v49 predate the current LOD lane and are skipped with exit 2
  instead of compared (--allow-legacy forces the comparison anyway).
placements: resolves the scene, then prints its GLB placement list as JSON.
  --iss auto (default) tries the production InitialSceneState descriptor first
  (404 falls through); --iss FILE reads a local descriptor; --iss off skips ISS.
  Without ISS the scene runs in the embedded scene runtime (node is not
  required); --manifest-builder is deprecated and ignored.
parse-manifest: reads a <sceneId>-lod-manifest.json written by the npm
  scene-lod-entities-manifest-builder, resolves the scene for its file->hash
  map, and prints the same pretty placement JSON as `placements` — the
  bridge scripts/lod-parity-oracle.sh diffs against the embedded runtime.
assemble: resolves placements like `placements`, fetches every referenced GLB
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
  skyline-pack tiles into one square power-of-two atlas per alpha class
  (opaque JPEG, mask/transparent PNG after alpha bleed), merge each class
  into a single primitive (welding duplicate verts), remap uvs. Default
  matches production's fixed per-level budget: every canvas is pinned to
  --max-size (256, production's current LOD budget; 512 for the pre-2025
  vintage), tiles composited at native texels, solid tiles fill the whole
  canvas, remainder alpha-bled. The client copies each atlas into a
  fixed-size shared texture-array slot, so undersized or mixed sizes get
  skipped and render untextured. --atlas-adaptive selects the old
  shrink-to-content bake (canvas shrunk to the packed extent, flat tiles
  8x8) for non-Unity consumers; --atlas-fixed selects the retired-lane
  full-bleed bake (tiles scaled to fill). --crop-base/--crop-parcels
  clip the model to the parcel-union rects before atlasing (the generate
  stage order), for staged crop runs without a catalyst entity.
simplify: decimates a GLB. --simplifier picks the backend (default from
  ABGEN_SIMPLIFIER, else meshopt). meshopt runs the in-crate meshoptimizer
  simplifier: the tri budget (--tri-cap, else ratio x input tris) is
  apportioned per primitive by triangle share, each primitive gets one
  topology-preserving pass with a loose error bound so the count target
  dominates, a sloppy (topology-ignoring) retry when that stops early
  above target, then orphan-vertex compaction; a capped result still over
  budget is a hard error. gltfpack shells out (-si <ratio> -noq;
  binary resolved --gltfpack > ABGEN_GLTFPACK > PATH): --tri-cap N: when
  the plain quality pass stays over the cap, the ladder re-runs at the
  budget-true ratio (cap/source) escalating the error limit
  (-sp -se 0.03|0.1|0.3|1.0) and stops at the mildest rung that fits; -sa
  is a genuine last resort. A fit below 0.8*cap fills back toward the cap
  by bisecting -se on the quality path (ratio without -sa on a plain fit;
  -sa bisection only when the fit itself was -sa). In both backends inputs
  already satisfying ratio>=1 + cap pass through untouched.
  --allow-unsimplified copies the input through verbatim (loud warning)
  when the simplifier is unavailable or fails.
generate/placements/assemble run without node: scenes lacking an ISS
  descriptor are executed by the embedded scene runtime.
generate: the full sync chain: resolve scene -> placements (iss|embedded
  scene runtime) -> assemble -> crop -> atlas -> simplify -> bundle via the LOD build mode
  into {out}/{sceneId}/LOD/{level}/{sceneId}_{level}_{platform} (+.br,
  LOD.manifest.json). --level takes a comma-separated list (default 0,1;
  level 2 is refused; production stopped emitting it): every level shares
  ONE assemble/crop/atlas bake and gets its own simplify pass, staged
  {sceneId}_{level}.glb, bundles and self-gate table (labels L{level}: /
  L{level}:{platform}:). Level 0 = that bake un-decimated (ratio 1.0):
  always the pass-through lane, gltfpack is neither run nor resolved, and a
  numeric --tri-cap is ignored with a warning. This DIVERGES from legacy
  production LOD0 (a real-scene bundle with per-source meshes/materials on
  dcl/scene_ignore_windows per prod-inspection.md and the LOD0 section of
  PROD-CHARACTERIZATION.md); the ISS path is the
  production-current LOD0 replacement. At level 1 the tri budget defaults to
  --tri-cap auto: cap = 500 x parcels, the production budget, so the final
  mesh is min(source, 500 x parcels) tris. Scenes at or under the cap pass
  through bit-identically (without resolving gltfpack); larger scenes are
  decimated with the -se escalation ladder into [0.8*cap, cap] (hard error
  if the cap is unreachable).
  --tri-cap N overrides the cap; --tri-cap off restores the legacy
  ratio-only lane (pass-through at or under 500 x parcels, else an
  uncapped ratio decimation). --bake-order post reorders the chain to
  production's ordering: assemble -> crop -> raw multi-material GLB ->
  simplify -> re-ingest -> atlas -> bundle, so atlas UVs are baked onto the
  final decimated triangles and simplification can never smear them across
  atlas tiles; the default pre keeps the atlas-then-simplify chain.
  --simplifier picks the decimation backend
  exactly as in `simplify` above (default from ABGEN_SIMPLIFIER, else
  meshopt). Every capped run adds a tri-cap self-gate
  check (tris_after <= cap); an --allow-unsimplified verbatim copy passes
  it with a recorded waiver. A scene whose
  placements resolve to nothing (e.g. the scene runtime captures no
  renderer state) builds a content-free bundle: no meshes, materials or
  textures, metadata dependencies []. The crop stage (default on, matching
  production; --no-crop disables) clips merged geometry to the exact parcel
  rect and adds a crop-bounds self-gate check. --platform takes a
  comma-separated list (windows|mac|linux; webgl is refused — upstream webgl
  LOD bundles use an empty suffix and are unsupported here): every platform
  bundle is built from the same bake and simplify pass, written with its own
  .br sidecar, listed in ONE union LOD.manifest.json, and self-gated
  separately (one gate table per platform, including a target-platform
  check: windows=19 mac=2 linux=24). Every run also writes the ISS
  descriptor {out}/{sceneId}/{sceneId}_InitialSceneState.json (+.br)
  next to LOD.manifest.json — the production InitialSceneState shape
  ({version, sceneId, assets:[{hash, position, rotation, scale}]}) with the
  acquired placements serialized verbatim in the pinned base-relative
  frame, in BOTH lanes (ISS pass-through and embedded scene runtime;
  empty scene => assets []); the abcdn server serves it at
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
        "compare" => cmd_compare(&argv[1..]),
        "placements" => cmd_placements(&argv[1..]),
        "parse-manifest" => cmd_parse_manifest(&argv[1..]),
        "assemble" => cmd_assemble(&argv[1..]),
        "atlas" => cmd_atlas(&argv[1..]),
        "simplify" => cmd_simplify(&argv[1..]),
        "generate" => cmd_generate(&argv[1..]),
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

fn cmd_placements(argv: &[String]) -> Result<i32> {
    let mut coords: Option<String> = None;
    let mut scene: Option<String> = None;
    let mut iss = "auto".to_string();
    let mut catalyst = CATALYST.to_string();

    let mut a = Args::new(argv);
    while let Some(arg) = a.next() {
        match arg.as_str() {
            "--coords" => coords = Some(a.val()?.clone()),
            "--scene" => scene = Some(a.val()?.clone()),
            "--iss" => iss = a.val()?.clone(),
            "--catalyst" => catalyst = a.val()?.clone(),
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

    let list = abgen::lodgen::acquire_placements(&client, &ent, &iss)?;
    println!("{}", serde_json::to_string_pretty(&list)?);
    Ok(0)
}

fn cmd_parse_manifest(argv: &[String]) -> Result<i32> {
    let mut manifest: Option<String> = None;
    let mut scene: Option<String> = None;
    let mut catalyst = CATALYST.to_string();

    let mut a = Args::new(argv);
    while let Some(arg) = a.next() {
        match arg.as_str() {
            "--scene" => scene = Some(a.val()?.clone()),
            "--catalyst" => catalyst = a.val()?.clone(),
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
        "source: manifest ({} placements, {} mesh-renderer-only skipped, {} unresolved src)",
        full.placements.len(),
        full.skipped_mesh_renderer,
        full.unresolved_src
    );
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

    let list = abgen::lodgen::acquire_placements(&client, &ent, &iss)?;
    eprintln!("placements: {}", list.len());

    let cache_dir = cache.as_deref().map(std::path::Path::new);
    if let Some(dir) = cache_dir {
        std::fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
    }
    let mut model = assemble::assemble(
        &client,
        &ent,
        &list,
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
    let mut crop_base: Option<String> = None;
    let mut crop_parcels: Option<String> = None;

    let mut a = Args::new(argv);
    while let Some(arg) = a.next() {
        match arg.as_str() {
            "-i" | "--in" => input = Some(a.val()?.clone()),
            "-o" | "--out" => out = Some(a.val()?.clone()),
            "--max-size" => max_size = a.val()?.parse().context("--max-size")?,
            "--padding" => padding = a.val()?.parse().context("--padding")?,
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
    let mode = atlas_mode(atlas_fixed, atlas_adaptive);
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
    let mut backend = simplify::SimplifierBackend::from_env();
    let mut gltfpack: Option<String> = None;
    let mut allow_unsimplified = false;

    let mut a = Args::new(argv);
    while let Some(arg) = a.next() {
        match arg.as_str() {
            "-i" | "--in" => input = Some(a.val()?.clone()),
            "-o" | "--out" => out = Some(a.val()?.clone()),
            "--ratio" => ratio = a.val()?.parse().context("--ratio")?,
            "--tri-cap" => tri_cap = Some(a.val()?.parse().context("--tri-cap")?),
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

    let report = match backend {
        simplify::SimplifierBackend::Meshopt => {
            eprintln!("simplifier: meshopt (in-crate meshoptimizer)");
            match abgen::lodgen::simplify_meshopt::simplify_file(&input, &out, ratio, tri_cap) {
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
                    eprintln!("gltfpack: {}", bin.display());
                    match simplify::simplify(&input, &out, ratio, tri_cap, &bin) {
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

fn cmd_generate(argv: &[String]) -> Result<i32> {
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
            "--tri-cap" => match a.val()?.as_str() {
                "auto" => {
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
            },
            "--atlas-max" => params.atlas_max = a.val()?.parse().context("--atlas-max")?,
            "--atlas-fixed" => params.atlas_fixed = true,
            "--atlas-adaptive" => params.atlas_adaptive = true,
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

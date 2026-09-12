use crate::builder::{build_bundle, build_bundle_multi, BuildOpts, LodBuildParams};
#[cfg(not(target_arch = "wasm32"))]
use crate::catalyst::CatalystClient;
use crate::naming;
use anyhow::{anyhow, bail, Context, Result};
#[cfg(not(target_arch = "wasm32"))]
use rayon::prelude::*;
#[cfg(not(target_arch = "wasm32"))]
use std::collections::BTreeMap;
#[cfg(not(target_arch = "wasm32"))]
use std::path::{Path, PathBuf};

pub const DEFAULT_LODS_BUCKET: &str = "https://lod-generator-unity-cdn-decentraland-org-contentbucket-4bd1977.s3.amazonaws.com/lods-unity/fbx-deprecated-sources";

pub const LOD_ERA_MIN_AB_VERSION: u32 = 49;

pub fn ab_version_num(version: &str) -> Option<u32> {
    version
        .trim()
        .trim_start_matches(['v', 'V'])
        .parse::<u32>()
        .ok()
}

pub fn ab_version_is_lod_era(version: &str) -> bool {
    ab_version_num(version).is_some_and(|n| n >= LOD_ERA_MIN_AB_VERSION)
}

#[derive(Clone, Debug)]
pub struct LodGenMeta {
    pub parcels: Vec<(i32, i32)>,
    pub base: (i32, i32),
    pub timestamp: Option<i64>,
    pub vertical_override: Option<f64>,
    pub fidelity: bool,
}

#[derive(Clone, Debug)]
pub struct LodOptions {
    pub platform: String,

    pub ab_version: String,

    pub keep_forward_plus: bool,

    pub lod: Option<LodGenMeta>,
}

impl Default for LodOptions {
    fn default() -> Self {
        LodOptions {
            platform: "windows".to_string(),
            ab_version: crate::manifest::DEFAULT_AB_VERSION.to_string(),
            keep_forward_plus: true,
            lod: None,
        }
    }
}

pub fn plane_clipping(parcels: &[(i32, i32)]) -> [f64; 4] {
    if parcels.is_empty() {
        return [0.0; 4];
    }
    let min_x = parcels.iter().map(|p| p.0).min().unwrap_or(0);
    let max_x = parcels.iter().map(|p| p.0).max().unwrap_or(0);
    let min_y = parcels.iter().map(|p| p.1).min().unwrap_or(0);
    let max_y = parcels.iter().map(|p| p.1).max().unwrap_or(0);
    [
        min_x as f64 * 16.0 - 0.05,
        (max_x + 1) as f64 * 16.0 + 0.05,
        min_y as f64 * 16.0 - 0.05,
        (max_y + 1) as f64 * 16.0 + 0.05,
    ]
}

/// Slack below the ground plane, mirroring the 0.05 margin `plane_clipping`
/// already gives the horizontal planes. Production ships a hard 0.0 floor, but a
/// scene floor sits at exactly y = 0 - the same plane it is clipped against - so
/// interpolated world-y lands either side of it across a large quad and a share
/// of the fragments clip. The floor then reads as translucent rather than
/// missing. Dropping the plane just under the ground keeps whole floors; real
/// sunken geometry (kerbs, foundations) still clips the way production clips it.
pub const VERTICAL_CLIP_SLACK: f64 = 0.05;

pub fn vertical_clipping(n_parcels: usize) -> [f64; 4] {
    // An unresolved scene zeroes every clipping vector, slack included.
    if n_parcels == 0 {
        return [0.0; 4];
    }
    let height = 20.0f32 * crate::detmath::log2f((n_parcels + 1) as f32);
    [-VERTICAL_CLIP_SLACK, height as f64, 0.0, 0.0]
}

pub fn client_placement(base: (i32, i32)) -> [f64; 3] {
    [base.0 as f64 * 16.0, 0.0, base.1 as f64 * 16.0]
}

pub fn root_position(base: (i32, i32)) -> [f64; 3] {
    [base.0 as f64 * 16.0, 0.0, base.1 as f64 * 16.0]
}

pub fn lod_main_asset(scene_id: &str, level: u32) -> String {
    format!("{}_{}.prefab", scene_id.to_lowercase(), level)
}

#[derive(Clone, Debug)]
pub struct LodSource {
    pub scene_id: String,

    pub level: u32,

    pub origin: String,

    pub ext: String,
}

#[derive(Debug)]
pub struct LodResult {
    pub scene_id: String,
    pub level: u32,

    pub bundle_name: String,

    pub bytes: usize,

    pub rel_path: String,
}

#[derive(Debug, Default)]
pub struct LodConversion {
    pub scene_id: String,
    pub results: Vec<LodResult>,

    pub skipped: Vec<(String, String)>,
}

impl LodConversion {
    pub fn total_bytes(&self) -> usize {
        self.results.iter().map(|r| r.bytes).sum()
    }
}

pub fn parse_lod_filename(locator: &str) -> Result<(String, u32, String)> {
    let no_query = locator.split(['?', '#']).next().unwrap_or(locator);
    let file = if no_query.starts_with("http://") || no_query.starts_with("https://") {
        no_query.rsplit('/').next().unwrap_or(no_query)
    } else {
        std::path::Path::new(no_query)
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or(no_query)
    };
    let ext = naming::file_extension(file);
    let stem = match file.rfind('.') {
        Some(i) => &file[..i],
        None => file,
    };

    let underscore = stem
        .rfind('_')
        .ok_or_else(|| anyhow!("LOD source name {file:?} has no _<level> suffix"))?;
    let (scene_part, level_part) = stem.split_at(underscore);
    let level_str = &level_part[1..];
    let level: u32 = level_str
        .parse()
        .map_err(|_| anyhow!("LOD source name {file:?} has non-numeric level {level_str:?}"))?;
    if scene_part.is_empty() {
        bail!("LOD source name {file:?} has empty scene id");
    }

    Ok((scene_part.to_lowercase(), level, ext))
}

pub fn validate_lod_platform(p: &str) -> Result<()> {
    match p {
        "windows" | "mac" | "linux" => Ok(()),
        "webgl" => bail!(
            "LOD platform \"webgl\" unsupported: upstream webgl LOD bundles use an empty \
             platform suffix and are not generated here (want windows|mac|linux)"
        ),
        other => bail!("unknown LOD platform {other:?} (want windows|mac|linux)"),
    }
}

pub fn lod_bundle_name(scene_id: &str, level: u32, platform: &str) -> String {
    format!("{}_{}_{}", scene_id.to_lowercase(), level, platform)
}

/// Scene-relative directory of the published GLB family (`lods-unity/lods/{file}`).
pub const PUBLISHED_GLB_DIR: &str = "lods-unity/lods";

/// File name of the published GLB: abgen lower-cases every LOD key, production
/// names it with the verbatim entity id (identical for `bafk…` ids).
pub fn published_glb_name(scene_id: &str, level: u32) -> String {
    format!("{}_{}.glb", scene_id.to_lowercase(), level)
}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
fn lod_rel_path(level: u32, bundle_name: &str) -> String {
    format!("LOD/{level}/{bundle_name}")
}

#[cfg(not(target_arch = "wasm32"))]
fn fetch_lod_bytes(client: &CatalystClient, locator: &str) -> Result<Vec<u8>> {
    if locator.starts_with("http://") || locator.starts_with("https://") {
        return http_get(locator).with_context(|| format!("download LOD {locator}"));
    }
    let p = Path::new(locator);
    if p.exists() {
        return std::fs::read(p).with_context(|| format!("read LOD file {locator}"));
    }

    client
        .fetch_content(locator)
        .with_context(|| format!("fetch LOD content {locator}"))
}

#[cfg(not(target_arch = "wasm32"))]
fn http_get(url: &str) -> Result<Vec<u8>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(120)))
        .build()
        .into();
    let resp = agent
        .get(url)
        .header("User-Agent", crate::catalyst::UA)
        .call()
        .map_err(|e| anyhow!("GET {url}: {e}"))?;
    let mut buf = Vec::new();
    use std::io::Read;
    resp.into_body().into_reader().read_to_end(&mut buf)?;
    Ok(buf)
}

#[cfg(not(target_arch = "wasm32"))]
fn prepare_lod_source(client: &CatalystClient, locator: &str) -> Result<(String, u32, Vec<u8>)> {
    let (sid, level, ext) = parse_lod_filename(locator)?;
    if ext == ".fbx" {
        bail!(
            "FBX LOD source {locator:?}: FBX -> GLB transcoder not yet implemented \
             in the pure-Rust port (TODO: add an FBX importer crate or shell out to \
             a converter; track in the abgen issue tracker). Workaround: \
             re-export the asset as .glb upstream and re-run."
        );
    }
    if ext != ".glb" && ext != ".gltf" {
        bail!("unsupported LOD source extension {ext:?} for {locator:?}");
    }

    let glb = fetch_lod_bytes(client, locator)?;
    Ok((sid, level, glb))
}

#[cfg(not(target_arch = "wasm32"))]
fn is_worlds_host(base_url: &str) -> bool {
    let rest = base_url.split("://").nth(1).unwrap_or(base_url);
    let host = rest.split(['/', ':']).next().unwrap_or("");
    host.to_ascii_lowercase()
        .starts_with("worlds-content-server")
}

#[cfg(not(target_arch = "wasm32"))]
pub fn resolve_scene_geometry(
    client: &CatalystClient,
    sid: &str,
) -> Result<((i32, i32), Vec<(i32, i32)>)> {
    let ent = if is_worlds_host(client.base_url()) {
        client.fetch_entity(sid)?
    } else {
        client.fetch_active_entity_by_id(sid)?
    };
    crate::lodgen::scene_geometry(&ent)
}

#[cfg(not(target_arch = "wasm32"))]
fn resolve_scene_meta(client: &CatalystClient, sid: &str) -> LodGenMeta {
    match resolve_scene_geometry(client, sid) {
        Ok((base, parcels)) => LodGenMeta {
            parcels,
            base,
            timestamp: None,
            vertical_override: None,
            fidelity: false,
        },
        Err(e) => {
            eprintln!(
                "WARN: could not resolve scene entity {sid}: {e:#}; \
                 converting with zeroed clipping (upstream converter behavior)"
            );
            LodGenMeta {
                parcels: Vec::new(),
                base: (0, 0),
                timestamp: None,
                vertical_override: None,
                fidelity: false,
            }
        }
    }
}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
fn build_lod_bundles(
    glb: &[u8],
    locator: &str,
    sid: &str,
    level: u32,
    platforms: &[String],
    opts: &LodOptions,
    meta: &LodGenMeta,
) -> Vec<(String, Result<(LodResult, Vec<u8>)>)> {
    let bundle_names: Vec<String> = platforms
        .iter()
        .map(|platform| lod_bundle_name(sid, level, platform))
        .collect();
    let root_hash = format!("{}_{}", sid, level);
    let lod_params = LodBuildParams {
        level,
        plane_clipping: plane_clipping(&meta.parcels),
        vertical_clipping: match meta.vertical_override {
            Some(h) => [-VERTICAL_CLIP_SLACK, h, 0.0, 0.0],
            None => vertical_clipping(meta.parcels.len()),
        },
        root_position: root_position(meta.base),
        main_asset: lod_main_asset(sid, level),
        timestamp: meta.timestamp,
        fidelity: meta.fidelity,
    };
    let build_opts = BuildOpts {
        keep_forward_plus: opts.keep_forward_plus,
        source_file: Some(locator),
        lod: Some(&lod_params),
        ..Default::default()
    };

    let shareable: Vec<usize> = platforms
        .iter()
        .enumerate()
        .filter_map(|(i, platform)| matches!(platform.as_str(), "windows" | "mac").then_some(i))
        .collect();
    let mut artifacts: Vec<Option<Result<crate::builder::BundleArtifact>>> =
        std::iter::repeat_with(|| None)
            .take(platforms.len())
            .collect();

    if shareable.len() >= 2 {
        let names: Vec<String> = shareable.iter().map(|&i| bundle_names[i].clone()).collect();
        if let Ok(built) = build_bundle_multi(glb, &names, &root_hash, &build_opts) {
            for (&i, artifact) in shareable.iter().zip(built) {
                artifacts[i] = Some(Ok(artifact));
            }
        }
    }
    for (i, slot) in artifacts.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(
                build_bundle(glb, &bundle_names[i], &root_hash, &build_opts)
                    .with_context(|| format!("build LOD bundle for {locator:?}")),
            );
        }
    }

    platforms
        .iter()
        .cloned()
        .zip(bundle_names)
        .zip(artifacts)
        .map(|((platform, bundle_name), artifact)| {
            let result = artifact
                .expect("every platform build is populated")
                .map(|artifact| {
                    let data = artifact.data;
                    let result = LodResult {
                        scene_id: sid.to_string(),
                        level,
                        bytes: data.len(),
                        rel_path: lod_rel_path(level, &bundle_name),
                        bundle_name,
                    };
                    (result, data)
                });
            (platform, result)
        })
        .collect()
}

#[cfg(not(target_arch = "wasm32"))]
pub fn convert_lods(
    client: &CatalystClient,
    sources: &[String],
    out_dir: &str,
    opts: &LodOptions,
) -> Result<LodConversion> {
    convert_lods_platforms(
        client,
        sources,
        out_dir,
        opts,
        std::slice::from_ref(&opts.platform),
    )
}

#[cfg(not(target_arch = "wasm32"))]
pub fn convert_lods_platforms(
    client: &CatalystClient,
    sources: &[String],
    out_dir: &str,
    opts: &LodOptions,
    platforms: &[String],
) -> Result<LodConversion> {
    if sources.is_empty() {
        bail!("no LOD sources given");
    }
    let platform_list: Vec<String> = if platforms.is_empty() {
        vec![opts.platform.clone()]
    } else {
        platforms.to_vec()
    };
    let mut conv = LodConversion::default();

    let mut written: BTreeMap<String, (String, Vec<u8>)> = BTreeMap::new();
    let mut scene_id: Option<String> = None;
    let mut meta_cache: BTreeMap<String, LodGenMeta> = BTreeMap::new();

    let prepared: Vec<_> = sources
        .iter()
        .map(|locator| {
            let source = prepare_lod_source(client, locator).map(|(sid, level, glb)| {
                let meta = match &opts.lod {
                    Some(m) => m.clone(),
                    None => meta_cache
                        .entry(sid.clone())
                        .or_insert_with(|| resolve_scene_meta(client, &sid))
                        .clone(),
                };
                (sid, level, glb, meta)
            });
            (locator, source)
        })
        .collect();
    let packaged: Vec<_> = prepared
        .into_par_iter()
        .map(|(locator, source)| {
            let builds = source.map(|(sid, level, glb, meta)| {
                build_lod_bundles(&glb, locator, &sid, level, &platform_list, opts, &meta)
            });
            (locator, builds)
        })
        .collect();

    for (locator, builds) in packaged {
        match builds {
            Ok(builds) => {
                for (platform, build) in builds {
                    match build {
                        Ok((r, data)) => {
                            scene_id.get_or_insert_with(|| r.scene_id.clone());
                            written.insert(r.bundle_name.clone(), (r.rel_path.clone(), data));
                            conv.results.push(r);
                        }
                        Err(e) => {
                            let key = if platform_list.len() == 1 {
                                locator.clone()
                            } else {
                                format!("{locator} [{platform}]")
                            };
                            conv.skipped.push((key, format!("{e:#}")));
                        }
                    }
                }
            }
            Err(e) => conv.skipped.push((locator.clone(), format!("{e:#}"))),
        }
    }

    let sid = match scene_id.clone() {
        Some(s) => s,
        None => {
            let mut msg = format!("no LOD source converted ({} skipped)", conv.skipped.len());
            for (locator, err) in &conv.skipped {
                msg.push_str(&format!("\n  {locator}: {err}"));
            }
            bail!("{msg}");
        }
    };
    conv.scene_id = sid.clone();

    let entity_dir = PathBuf::from(out_dir).join(&sid);
    written
        .par_iter()
        .try_for_each(|(_, (rel, data))| -> Result<()> {
            let path = entity_dir.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            write_atomic(&path, data)
        })?;

    write_lod_manifest(&entity_dir, &conv, &opts.ab_version)?;
    Ok(conv)
}

pub fn s3_source_urls(
    bucket: &str,
    parcel: &str,
    timestamp: &str,
    scene_id: &str,
    levels: &[u32],
    ext: &str,
) -> Vec<String> {
    let bucket = bucket.trim_end_matches('/');
    let ext = ext.trim_start_matches('.');
    levels
        .iter()
        .map(|lvl| format!("{bucket}/{parcel}/LOD/Sources/{timestamp}/{scene_id}_{lvl}.{ext}"))
        .collect()
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedObject {
    pub key: String,
    pub path: PathBuf,
}

/// Space objects a generated LOD scene directory publishes, keyed the way
/// production lays them out: bundles at `LOD/{level}/{file}`, the ISS
/// descriptor at `lods-unity/manifests/{file}` and the published GLB at
/// `lods-unity/lods/{file}` — all unversioned, unlike asset bundles. Upload
/// metadata (Content-Type/Cache-Control/Content-Encoding) is derived from the
/// key by `space::object_headers`, not carried here.
#[cfg(not(target_arch = "wasm32"))]
pub fn published_objects(scene_dir: &Path, levels: &[u32]) -> Vec<PublishedObject> {
    let mut out: Vec<PublishedObject> = Vec::new();
    for level in levels {
        let dir = scene_dir.join("LOD").join(level.to_string());
        for name in dir_file_names(&dir) {
            out.push(PublishedObject {
                key: format!("LOD/{level}/{name}"),
                path: dir.join(&name),
            });
        }
    }
    for name in dir_file_names(scene_dir) {
        if !name.ends_with(crate::lodgen::placements::ISS_SUFFIX) {
            continue;
        }
        out.push(PublishedObject {
            key: format!("lods-unity/manifests/{name}"),
            path: scene_dir.join(&name),
        });
    }
    let glb_dir = scene_dir.join(PUBLISHED_GLB_DIR);
    for name in dir_file_names(&glb_dir) {
        out.push(PublishedObject {
            key: format!("{PUBLISHED_GLB_DIR}/{name}"),
            path: glb_dir.join(&name),
        });
    }
    out
}

#[cfg(not(target_arch = "wasm32"))]
fn dir_file_names(dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = rd
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| !n.contains(".tmp.") && !n.ends_with(".br"))
        .collect();
    names.sort();
    names
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let mut tmp_os = path.as_os_str().to_owned();
    tmp_os.push(format!(".tmp.{}", std::process::id()));
    let tmp = PathBuf::from(tmp_os);
    std::fs::write(&tmp, data).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Writes `<scene_dir>/lods-unity/lods/<sidLower>_<level>.glb` in the
/// gltfpack layout from the staged float GLB, which stays the bundle input.
#[cfg(not(target_arch = "wasm32"))]
pub fn write_published_glb(
    scene_dir: &Path,
    scene_id: &str,
    level: u32,
    float_glb: &[u8],
) -> Result<PathBuf> {
    let bytes = crate::lodgen::quantize::write_gltfpack_layout(float_glb, scene_id)
        .with_context(|| format!("quantize {scene_id} level {level}"))?;
    let dir = scene_dir.join(PUBLISHED_GLB_DIR);
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let path = dir.join(published_glb_name(scene_id, level));
    write_atomic(&path, &bytes)?;
    Ok(path)
}

#[cfg(not(target_arch = "wasm32"))]
fn write_lod_manifest(entity_dir: &Path, conv: &LodConversion, ab_version: &str) -> Result<()> {
    std::fs::create_dir_all(entity_dir)?;
    let mut files: Vec<serde_json::Value> = conv
        .results
        .iter()
        .map(|r| serde_json::Value::String(r.rel_path.clone()))
        .collect();
    files.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
    let levels: Vec<serde_json::Value> = {
        let mut v: Vec<u32> = conv.results.iter().map(|r| r.level).collect();
        v.sort_unstable();
        v.dedup();
        v.into_iter().map(serde_json::Value::from).collect()
    };
    let manifest = serde_json::json!({
        "version": ab_version,
        "sceneId": conv.scene_id,
        "levels": levels,
        "files": files,
        "exitCode": if conv.results.is_empty() { 1 } else { 0 },
    });
    let text = serde_json::to_string_pretty(&manifest)?;
    let mpath = entity_dir.join("LOD.manifest.json");
    write_atomic(&mpath, text.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn era_gate_only_accepts_post_v49_ab_versions() {
        assert!(ab_version_is_lod_era("v49"));
        assert!(ab_version_is_lod_era("v50"));
        assert!(ab_version_is_lod_era("V49"));
        assert!(ab_version_is_lod_era("49"));
        assert!(!ab_version_is_lod_era("v36"));
        assert!(!ab_version_is_lod_era("v48"));
        assert!(!ab_version_is_lod_era("v7"));
        assert!(!ab_version_is_lod_era(""));
        assert!(!ab_version_is_lod_era("v0-abgen"));
        assert!(!ab_version_is_lod_era("garbage"));
        assert_eq!(ab_version_num("v49"), Some(49));
        assert_eq!(ab_version_num("v0-abgen"), None);
    }

    #[test]
    fn unresolved_scene_zeroes_clipping_like_upstream() {
        assert_eq!(plane_clipping(&[]), [0.0; 4]);
        assert_eq!(vertical_clipping(0), [0.0; 4]);
        assert_eq!(root_position((0, 0)), [0.0; 3]);
    }

    #[test]
    fn parses_lod_filenames() {
        let (sid, lvl, ext) =
            parse_lod_filename("https://b.s3.amazonaws.com/-17,-21/LOD/Sources/170/bafkrei_0.fbx")
                .unwrap();
        assert_eq!(sid, "bafkrei");
        assert_eq!(lvl, 0);
        assert_eq!(ext, ".fbx");

        let (sid, lvl, ext) = parse_lod_filename("BafkReiABC_2.glb").unwrap();
        assert_eq!(sid, "bafkreiabc");
        assert_eq!(lvl, 2);
        assert_eq!(ext, ".glb");

        let (sid, lvl, _) = parse_lod_filename("x/scene_1.glb?token=abc").unwrap();
        assert_eq!(sid, "scene");
        assert_eq!(lvl, 1);
    }

    #[test]
    fn rejects_bad_names() {
        #[cfg(windows)]
        {
            let (sid, lvl, ext) =
                parse_lod_filename(r"C:\Users\b\AppData\Local\Temp\t-1\bafkreiscene_1.glb")
                    .unwrap();
            assert_eq!(sid, "bafkreiscene");
            assert_eq!(lvl, 1);
            assert_eq!(ext, ".glb");
        }
        {
            let native = std::path::Path::new("tmp").join("bafkreiscene_1.glb");
            let (sid, lvl, _) = parse_lod_filename(native.to_str().unwrap()).unwrap();
            assert_eq!(sid, "bafkreiscene");
            assert_eq!(lvl, 1);
        }
        assert!(parse_lod_filename("noLevel.glb").is_err());
        assert!(parse_lod_filename("scene_x.glb").is_err());
        assert!(parse_lod_filename("_3.glb").is_err());
    }

    #[test]
    fn validate_lod_platform_matrix() {
        for ok in ["windows", "mac", "linux"] {
            assert!(validate_lod_platform(ok).is_ok(), "{ok}");
        }
        let webgl = format!("{:#}", validate_lod_platform("webgl").unwrap_err());
        assert!(webgl.contains("empty"), "{webgl}");
        assert!(webgl.contains("unsupported"), "{webgl}");
        for bad in ["", "osx", "win", "WINDOWS", "windows,mac"] {
            assert!(validate_lod_platform(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn bundle_name_matches_client_key() {
        assert_eq!(
            lod_bundle_name("BafkRei", 1, "windows"),
            "bafkrei_1_windows"
        );
        assert_eq!(lod_bundle_name("scene", 0, "mac"), "scene_0_mac");
    }

    #[test]
    fn rel_path_is_per_level_folder() {
        assert_eq!(lod_rel_path(2, "scene_2_windows"), "LOD/2/scene_2_windows");
    }

    #[test]
    fn plane_clipping_is_world_parcel_rect_with_margin() {
        assert_eq!(
            plane_clipping(&[(8, -83)]),
            [127.95, 144.05, -1328.05, -1311.95]
        );
        let plaza: Vec<(i32, i32)> = (-3..=3)
            .flat_map(|x| (-4..=9).map(move |y| (x, y)))
            .collect();
        assert_eq!(plane_clipping(&plaza), [-48.05, 64.05, -64.05, 160.05]);
    }

    #[test]
    fn vertical_clipping_matches_height_limit_formula() {
        assert_eq!(vertical_clipping(1), [-VERTICAL_CLIP_SLACK, 20.0, 0.0, 0.0]);
        let v = vertical_clipping(70);
        assert!(
            (v[1] - 122.99493).abs() < 1e-3,
            "vertical_clipping(70)[1] = {}",
            v[1]
        );
        assert_eq!(v[0], -VERTICAL_CLIP_SLACK);
        assert_eq!(v[2], 0.0);
        assert_eq!(v[3], 0.0);
    }

    #[test]
    fn client_placement_is_base_parcel_world_position() {
        assert_eq!(client_placement((8, -83)), [128.0, 0.0, -1328.0]);
        assert_eq!(client_placement((-3, -2)), [-48.0, 0.0, -32.0]);
    }

    #[test]
    fn main_asset_is_lowercased_prefab_key() {
        assert_eq!(lod_main_asset("BafkReiABC", 1), "bafkreiabc_1.prefab");
        assert_eq!(
            lod_main_asset("qmccggwqvb7v3b3vqxajzcjimmzhzrrvmk3ulkt6qxsesd", 1),
            "qmccggwqvb7v3b3vqxajzcjimmzhzrrvmk3ulkt6qxsesd_1.prefab"
        );
    }

    #[test]
    fn s3_urls_built() {
        let urls = s3_source_urls(
            DEFAULT_LODS_BUCKET,
            "-17,-21",
            "1707776785658",
            "bafkrei",
            &[0, 1, 2],
            ".fbx",
        );
        assert_eq!(urls.len(), 3);
        assert!(urls[0].ends_with("/-17,-21/LOD/Sources/1707776785658/bafkrei_0.fbx"));
        assert!(urls[2].ends_with("bafkrei_2.fbx"));
    }

    #[test]
    fn published_objects_lists_bundles_and_iss_and_glb() {
        let base = std::env::temp_dir().join(format!("lods-pub-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let scene = base.join("bafkscene");
        for level in [0u32, 1] {
            std::fs::create_dir_all(scene.join("LOD").join(level.to_string())).unwrap();
        }
        std::fs::write(scene.join("LOD/0/bafkscene_0_windows"), b"a").unwrap();
        std::fs::write(scene.join("LOD/0/bafkscene_0_windows.br"), b"b").unwrap();
        std::fs::write(scene.join("LOD/1/bafkscene_1_mac"), b"c").unwrap();
        std::fs::write(scene.join("LOD/1/bafkscene_1_mac.tmp.9"), b"d").unwrap();
        std::fs::write(scene.join("bafkscene_InitialSceneState.json"), b"{}").unwrap();
        std::fs::write(scene.join("bafkscene_InitialSceneState.json.br"), b"z").unwrap();
        std::fs::write(scene.join("LOD.manifest.json"), b"{}").unwrap();
        std::fs::create_dir_all(scene.join("lods-unity/lods")).unwrap();
        std::fs::write(scene.join("lods-unity/lods/bafkscene_1.glb"), b"g").unwrap();
        std::fs::write(scene.join("lods-unity/lods/bafkscene_1.glb.br"), b"h").unwrap();
        std::fs::write(scene.join("lods-unity/lods/bafkscene_1.glb.tmp.3"), b"i").unwrap();

        let objs = published_objects(&scene, &[0, 1]);
        let keys: Vec<&str> = objs.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "LOD/0/bafkscene_0_windows",
                "LOD/1/bafkscene_1_mac",
                "lods-unity/manifests/bafkscene_InitialSceneState.json",
                "lods-unity/lods/bafkscene_1.glb",
            ]
        );
        assert_eq!(objs[1].path, scene.join("LOD/1/bafkscene_1_mac"));
        assert_eq!(objs[3].path, scene.join("lods-unity/lods/bafkscene_1.glb"));
        assert!(published_objects(&base.join("missing"), &[0, 1]).is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn write_published_glb_writes_only_gltfpack_layout() {
        use crate::lodgen::model::{LodMaterial, LodModel, LodPrimitive};
        let model = LodModel {
            root_name: "bafkscene_1".to_string(),
            primitives: vec![LodPrimitive {
                positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
                normals: vec![[0.0, 0.0, 1.0]; 3],
                uvs: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
                indices: vec![0, 1, 2],
                material: 0,
                ..Default::default()
            }],
            materials: vec![LodMaterial {
                name: "TextureBakeResult-mat".to_string(),
                ..Default::default()
            }],
            images: Vec::new(),
            log: Vec::new(),
        };
        let float = crate::lodgen::emit::emit_glb(&model).unwrap();
        let base = std::env::temp_dir().join(format!("lods-glb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let scene = base.join("BafkScene");
        std::fs::create_dir_all(&scene).unwrap();

        let path = write_published_glb(&scene, "BafkScene", 1, &float).unwrap();
        assert_eq!(path, scene.join("lods-unity/lods/bafkscene_1.glb"));
        let bytes = std::fs::read(&path).unwrap();
        let (json, _) = crate::gltf::load_gltf_inputs(&bytes, ".glb", None).unwrap();
        assert_eq!(json["scenes"][0]["name"], "BafkScene");
        assert_eq!(json["extensionsRequired"][0], "KHR_mesh_quantization");
        assert!(!scene.join("lods-unity/lods/bafkscene_1.glb.br").exists());
        let keys: Vec<String> = published_objects(&scene, &[1])
            .into_iter()
            .map(|o| o.key)
            .collect();
        assert_eq!(keys, vec!["lods-unity/lods/bafkscene_1.glb"]);
        let _ = std::fs::remove_dir_all(&base);
    }
}

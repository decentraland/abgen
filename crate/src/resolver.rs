use std::path::{Path, PathBuf};

pub const PLATFORMS: &[(&str, &str)] = &[
    ("_windows", "windows"),
    ("_mac", "mac"),
    ("_linux", "linux"),
    ("_webgl", "webgl"),
];

pub fn is_platform(name: &str) -> bool {
    PLATFORMS.iter().any(|(_, p)| *p == name)
}

pub fn platform_of(name: &str) -> &'static str {
    split_platform(name).0
}

pub fn split_platform(name: &str) -> (&'static str, &str) {
    for (suffix, bare) in PLATFORMS {
        if let Some(stem) = name.strip_suffix(suffix) {
            return (bare, stem);
        }
    }
    ("webgl", name)
}

pub fn is_safe_component(c: &str) -> bool {
    !c.is_empty()
        && c != "."
        && c != ".."
        && !c.contains('/')
        && !c.contains('\\')
        && !c.contains('\0')
}

pub fn manifest_path(root: &Path, name_with_suffix: &str) -> Option<PathBuf> {
    let (platform, entity_id) = split_platform(name_with_suffix);
    if !is_safe_component(entity_id) {
        return None;
    }
    Some(
        root.join(&*crate::naming::fs_safe_component(entity_id))
            .join(format!("{platform}.manifest.json")),
    )
}

pub fn binary_path(root: &Path, entity: &str, filename: &str) -> Option<PathBuf> {
    if !is_safe_component(entity) || !is_safe_component(filename) {
        return None;
    }
    let stored_name = crate::naming::fs_safe_component(filename);
    let flat = root.join(&*stored_name);
    if flat.is_file() {
        return Some(flat);
    }
    let name_for_platform = filename.strip_suffix(".br").unwrap_or(filename);
    let platform = platform_of(name_for_platform);
    let candidate = root
        .join(&*crate::naming::fs_safe_component(entity))
        .join(platform)
        .join(&*stored_name);
    if candidate.is_file() {
        return Some(candidate);
    }
    if let Some(hit) = digest_qualified_alias(&candidate, filename) {
        return Some(hit);
    }
    Some(candidate)
}

fn digest_qualified_alias(candidate: &Path, filename: &str) -> Option<PathBuf> {
    let is_br = filename.ends_with(".br");
    let raw = filename.strip_suffix(".br").unwrap_or(filename);
    if crate::naming::bundle_name_has_digest(raw) {
        return None;
    }
    let (platform, bare) = split_platform(raw);
    if bare == raw {
        return None;
    }
    let prefix = format!("{}_", bare.to_lowercase());
    let suffix = format!("_{platform}{}", if is_br { ".br" } else { "" }).to_lowercase();
    let dir = candidate.parent()?;
    for entry in std::fs::read_dir(dir).ok()? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let lower = name.to_lowercase();
        let Some(mid) = lower
            .strip_prefix(&prefix)
            .and_then(|r| r.strip_suffix(&suffix))
        else {
            continue;
        };
        if mid.len() == 32 && mid.bytes().all(|b| b.is_ascii_hexdigit()) && entry.path().is_file() {
            return Some(entry.path());
        }
    }
    None
}

pub fn lod_path(root: &Path, level: &str, filename: &str) -> Option<PathBuf> {
    if !is_safe_component(level) || !is_safe_component(filename) {
        return None;
    }
    let raw = filename.strip_suffix(".br").unwrap_or(filename);
    let (_, no_platform) = split_platform(raw);
    let scene_id = no_platform
        .strip_suffix(&format!("_{level}"))
        .unwrap_or(no_platform);
    if !is_safe_component(scene_id) {
        return None;
    }
    Some(root.join(scene_id).join("LOD").join(level).join(filename))
}

pub fn iss_manifest_path(root: &Path, filename: &str) -> Option<PathBuf> {
    if !is_safe_component(filename) {
        return None;
    }
    let stem = filename.strip_suffix(".br").unwrap_or(filename);
    let sid = stem.strip_suffix(crate::lodgen::placements::ISS_SUFFIX)?;
    if sid.is_empty() || !is_safe_component(sid) {
        return None;
    }
    Some(root.join(sid).join(filename))
}

pub fn bvpack_path(root: &Path, entity: &str, filename: &str) -> Option<PathBuf> {
    if !is_safe_component(entity) || !is_safe_component(filename) {
        return None;
    }
    let raw = filename.strip_suffix(".br").unwrap_or(filename);
    if raw != format!("{entity}.pack") {
        return None;
    }
    let br = if filename.ends_with(".br") { ".br" } else { "" };
    Some(
        root.join(entity)
            .join(crate::bvwebgpu::BVW_PLATFORM)
            .join(format!("{}{br}", crate::bvwebgpu::pack_file_name(entity))),
    )
}

pub fn resolve_with_casing(exact: &Path) -> Option<PathBuf> {
    if exact.is_file() {
        return Some(exact.to_path_buf());
    }
    let parent = exact.parent()?;
    let target = exact.file_name()?.to_str()?.to_ascii_lowercase();
    let mut found: Option<PathBuf> = None;
    for entry in std::fs::read_dir(parent).ok()? {
        let entry = entry.ok()?;
        if let Some(name) = entry.file_name().to_str() {
            if name.to_ascii_lowercase() == target && entry.path().is_file() {
                if found.is_some() {
                    return None;
                }
                found = Some(entry.path());
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn platform_detection() {
        assert_eq!(platform_of("Qm123_windows"), "windows");
        assert_eq!(platform_of("bafk_mac"), "mac");
        assert_eq!(platform_of("x_linux"), "linux");
        assert_eq!(platform_of("Qm123"), "webgl");
        assert_eq!(platform_of("Qm123_webgl"), "webgl");
        assert_eq!(split_platform("Qm123_webgl"), ("webgl", "Qm123"));
        assert_eq!(platform_of("staticscene_3_mac"), "mac");
    }

    #[test]
    fn manifest_mapping() {
        let root = Path::new("/out");
        assert_eq!(
            manifest_path(root, "bafkEnt_windows").unwrap(),
            Path::new("/out/bafkEnt/windows.manifest.json")
        );
        assert_eq!(
            manifest_path(root, "bafkEnt").unwrap(),
            Path::new("/out/bafkEnt/webgl.manifest.json")
        );
    }

    #[test]
    fn binary_mapping() {
        let root = Path::new("/out");
        assert_eq!(
            binary_path(root, "bafkScene", "Qmhash_windows").unwrap(),
            Path::new("/out/bafkScene/windows/Qmhash_windows")
        );
        assert_eq!(
            binary_path(root, "bafkScene", "Qmhash_mac.br").unwrap(),
            Path::new("/out/bafkScene/mac/Qmhash_mac.br")
        );
    }

    #[test]
    fn lod_mapping() {
        let root = Path::new("/out");
        assert_eq!(
            lod_path(root, "1", "bafkscene_1_mac").unwrap(),
            Path::new("/out/bafkscene/LOD/1/bafkscene_1_mac")
        );
        assert_eq!(
            lod_path(root, "2", "bafkscene_2_windows.br").unwrap(),
            Path::new("/out/bafkscene/LOD/2/bafkscene_2_windows.br")
        );
        assert_eq!(
            lod_path(root, "0", "bafkscene_0").unwrap(),
            Path::new("/out/bafkscene/LOD/0/bafkscene_0")
        );
    }

    #[test]
    fn iss_manifest_mapping() {
        let root = Path::new("/out");
        assert_eq!(
            iss_manifest_path(root, "bafkscene_InitialSceneState.json").unwrap(),
            Path::new("/out/bafkscene/bafkscene_InitialSceneState.json")
        );
        assert_eq!(
            iss_manifest_path(root, "bafkscene_InitialSceneState.json.br").unwrap(),
            Path::new("/out/bafkscene/bafkscene_InitialSceneState.json.br")
        );
        assert!(iss_manifest_path(root, "bafkscene-lod-manifest.json").is_none());
        assert!(iss_manifest_path(root, "bafkscene_InitialSceneState.jsonx").is_none());
        assert!(iss_manifest_path(root, "LOD.manifest.json").is_none());
        assert!(iss_manifest_path(root, "_InitialSceneState.json").is_none());
        assert!(iss_manifest_path(root, "_InitialSceneState.json.br").is_none());
        assert!(iss_manifest_path(root, ".._InitialSceneState.json").is_none());
        assert!(iss_manifest_path(root, "a/b_InitialSceneState.json").is_none());
        assert!(iss_manifest_path(root, "a\\b_InitialSceneState.json").is_none());
    }

    #[test]
    fn oversized_names_collapse_to_bounded_storage_components() {
        let root = Path::new("/out");
        let long_entity = format!("b64-{}", "a".repeat(300));
        let long_file = format!("b64-{}_mac", "b".repeat(300));

        let manifest = manifest_path(root, &format!("{long_entity}_mac")).unwrap();
        let bundle = binary_path(root, &long_entity, &long_file).unwrap();

        for path in [&manifest, &bundle] {
            for component in path.components() {
                let name = component.as_os_str().to_string_lossy();
                assert!(name.len() <= 255, "component too long: {name}");
            }
        }

        assert_eq!(bundle, binary_path(root, &long_entity, &long_file).unwrap());
        assert!(bundle.to_string_lossy().contains("xn-"));

        let short = binary_path(root, "bafkScene", "Qmhash_windows").unwrap();
        assert_eq!(short, Path::new("/out/bafkScene/windows/Qmhash_windows"));

        let mixed = format!("b64-{}_windows", "QzpcVXNlcnNc".repeat(20));
        assert_eq!(
            binary_path(root, &long_entity, &mixed).unwrap(),
            binary_path(root, &long_entity, &mixed.to_lowercase()).unwrap()
        );
    }

    #[test]
    fn bvpack_mapping() {
        let root = Path::new("/out");
        assert_eq!(
            bvpack_path(root, "bafkEnt", "bafkEnt.pack").unwrap(),
            Path::new("/out/bafkEnt/bvwebgpu/bafkEnt_bv4.pack")
        );
        assert_eq!(
            bvpack_path(root, "bafkEnt", "bafkEnt.pack.br").unwrap(),
            Path::new("/out/bafkEnt/bvwebgpu/bafkEnt_bv4.pack.br")
        );
        assert!(bvpack_path(root, "bafkEnt", "other.pack").is_none());
        assert!(bvpack_path(root, "bafkEnt", "bafkEnt.zip").is_none());
        assert!(bvpack_path(root, "..", "...pack").is_none());
    }

    #[test]
    fn rejects_traversal() {
        assert!(!is_safe_component("../etc"));
        assert!(!is_safe_component("a/b"));
        assert!(manifest_path(Path::new("/out"), "../../etc/passwd").is_none());
        assert!(binary_path(Path::new("/out"), "..", "x").is_none());
    }
}

use std::path::{Path, PathBuf};

use crate::resolver::is_safe_component;

pub const SHADER_PLATFORMS: [&str; 3] = ["windows", "mac", "linux"];

pub struct ShaderTarget {
    pub url_ver: String,
    pub canonical: String,
}

pub fn shader_allowlisted(canonical: &str) -> bool {
    SHADER_PLATFORMS.iter().any(|p| {
        canonical == format!("dcl/scene_ignore_{p}")
            || canonical == format!("dcl/universal render pipeline/lit_ignore_{p}")
            || canonical == crate::shader::texarray_bundle_name(p)
    })
}

fn hexval(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(hi), Some(lo)) = (hexval(b[i + 1]), hexval(b[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn shader_target(path: &str) -> Option<ShaderTarget> {
    let decoded = percent_decode(path);
    let segs: Vec<&str> = decoded.split('/').collect();
    if segs.len() < 3 || !is_safe_component(segs[0]) {
        return None;
    }
    if matches!(segs[0], "manifest" | "LOD" | "lods-unity") {
        return None;
    }
    for seg in &segs[1..] {
        if !is_safe_component(seg) {
            return None;
        }
    }
    let canonical = if segs[1] == "dcl" {
        segs[1..].join("/")
    } else if segs.len() >= 4 && segs[2] == "dcl" {
        segs[2..].join("/")
    } else {
        return None;
    };
    if !shader_allowlisted(&canonical) {
        return None;
    }
    Some(ShaderTarget {
        url_ver: segs[0].to_string(),
        canonical,
    })
}

pub fn shader_path(root: &Path, canonical: &str) -> Option<PathBuf> {
    let mut out = root.to_path_buf();
    for seg in canonical.split('/') {
        if !is_safe_component(seg) {
            return None;
        }
        out.push(seg);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn shader_target_strips_scene_id_to_one_canonical() {
        let three = shader_target("v41/dcl/scene_ignore_windows").unwrap();
        assert_eq!(three.url_ver, "v41");
        assert_eq!(three.canonical, "dcl/scene_ignore_windows");

        let four = shader_target("v41/bafkscene/dcl/scene_ignore_windows").unwrap();
        assert_eq!(four.url_ver, "v41");
        assert_eq!(four.canonical, "dcl/scene_ignore_windows");

        let lit4 = shader_target("v41/dcl/universal render pipeline/lit_ignore_mac").unwrap();
        assert_eq!(
            lit4.canonical,
            "dcl/universal render pipeline/lit_ignore_mac"
        );

        let lit5 =
            shader_target("v41/bafkscene/dcl/universal render pipeline/lit_ignore_mac").unwrap();
        assert_eq!(lit5.canonical, lit4.canonical);

        let tex = shader_target("v41/bafkscene/dcl/scene_texarray_ignore_linux").unwrap();
        assert_eq!(tex.canonical, "dcl/scene_texarray_ignore_linux");

        let enc = shader_target("v41/dcl/universal%20render%20pipeline/lit_ignore_mac").unwrap();
        assert_eq!(
            enc.canonical,
            "dcl/universal render pipeline/lit_ignore_mac"
        );
        let enc5 = shader_target("v41/bafkscene/dcl/universal%20render%20pipeline/lit_ignore_mac")
            .unwrap();
        assert_eq!(enc5.canonical, enc.canonical);
    }

    #[test]
    fn shader_target_rejects_non_allowlisted_and_traversal() {
        assert!(shader_target("v41/dcl/scene_ignore_webgl").is_none());
        assert!(shader_target("v41/dcl/anything_else").is_none());
        assert!(shader_target("v41/bafkscene/notdcl/scene_ignore_windows").is_none());
        assert!(shader_target("v41/../dcl/scene_ignore_windows").is_none());
        assert!(shader_target("v41/dcl/../scene_ignore_windows").is_none());
        assert!(shader_target("v41/bafkEntity/some/nested/file.bin").is_none());
        assert!(shader_target("v41/dcl").is_none());
        assert!(shader_target("dcl/scene_ignore_windows").is_none());
        assert!(shader_target("manifest/dcl/scene_ignore_windows").is_none());
        assert!(shader_target("LOD/dcl/scene_ignore_windows").is_none());
        assert!(shader_target("v41/dcl/scene_ignore_windows.br").is_none());
        assert!(shader_allowlisted(
            "dcl/universal render pipeline/lit_ignore_windows"
        ));
        assert!(!shader_allowlisted(
            "dcl/universal render pipeline/lit_ignore_webgl"
        ));
    }

    #[test]
    fn shader_disk_path_nests_under_root() {
        assert_eq!(
            shader_path(Path::new("/out"), "dcl/scene_ignore_windows").unwrap(),
            Path::new("/out/dcl/scene_ignore_windows")
        );
        assert_eq!(
            shader_path(
                Path::new("/out"),
                "dcl/universal render pipeline/lit_ignore_mac"
            )
            .unwrap(),
            Path::new("/out/dcl/universal render pipeline/lit_ignore_mac")
        );
        assert!(shader_path(Path::new("/out"), "dcl/../x").is_none());
    }
}

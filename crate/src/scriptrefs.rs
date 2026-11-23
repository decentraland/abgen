//! Normal maps no glTF names: images bound from a scene's code, or named and shaped like one.
//!
//! A standalone image is classified linear/normal from how the glTFs in its entity use it
//! ([`crate::glbscan`]). An SDK `Material` set from code never passes through a glTF: SDK7
//! writes `bumpTexture: Material.Texture.Common({ src: 'water/normal.png' })`, SDK6
//! `material.bumpTexture = new Texture('water/normal.png')`. The explorer loads such a texture
//! as linear when it fetches the raw file (`TextureType.NormalMap`), but cannot change the
//! colour space of a bundled `Texture2D`, so an image only the code uses as a normal map
//! must be classified here or its bundle ships as an sRGB colour texture.
//!
//! The scan is lexical, not an evaluation: it reads the value expression after each
//! `bumpTexture` key or assignment, collects the image-path string literals inside it, and
//! matches them against the entity's files on a path-segment boundary. That resolves a
//! literal path (`'images/n.png'`) and a folder-variable concatenation whose literal
//! carries the file name (`dir + "n.png"`). A path held in a constant declared elsewhere, or
//! a file name assembled from fragments (`name + "_normal.png"`), is not resolved; those
//! images keep the colour classification they had before, unless the name rule below
//! catches them.
//!
//! The name rule is the fallback for bindings the lexical scan cannot resolve: an image no
//! glTF references, whose file name ends in a normal-map suffix ([`NORMAL_NAME_SUFFIXES`],
//! the `_N` of the creator docs' texture naming among them), and whose pixels decode to
//! normal-map-shaped texels ([`crate::builder::image_looks_like_normal_map`]). Both
//! conditions are required: the name alone would turn a UI sprite called `button_n.png` into
//! a linear texture, the pixels alone a flat sky-blue albedo. An image a glTF does reference
//! is classified by that use, whatever it is called.
//!
//! Normal maps are the only slot that needs this. The SDK material's other image slots
//! (`texture`, `alphaTexture`, `emissiveTexture`) are colour data the explorer loads as sRGB,
//! the same as the default here; the glTF data slots (normal, metallic-roughness) are already
//! classified by [`crate::glbscan`].

use std::collections::{HashMap, HashSet};

const KEY: &[u8] = b"bumpTexture";

/// Upper bound on how far one value expression is followed, so a malformed or
/// unterminated expression cannot walk the rest of a multi-megabyte bundle.
const MAX_EXPR_BYTES: usize = 4096;

const IMAGE_EXTENSIONS: [&str; 4] = [".png", ".jpg", ".jpeg", ".ktx2"];

/// File-name stem endings that mark a normal map, compared case-insensitively. `_n` is the
/// creator docs' `T_<Name>_N`; the rest are the common exporter spellings.
pub const NORMAL_NAME_SUFFIXES: [&str; 4] = ["_n", "_normal", "_normalmap", "_nrm"];

/// Whether a content file is scene code the normal-map scan reads.
pub fn is_scene_script(file: &str) -> bool {
    file.to_ascii_lowercase().ends_with(".js")
}

fn is_ident_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
}

fn is_image_path(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    IMAGE_EXTENSIONS.iter().any(|e| l.ends_with(e))
}

/// Reads one string literal starting at the quote `b[start]`; returns its text and the
/// index just past the closing quote. A template literal keeps only the text after its
/// last `${…}` interpolation, the only part that can be matched as a path tail.
fn read_string(b: &[u8], start: usize) -> (String, usize) {
    let quote = b[start];
    let mut i = start + 1;
    let mut out: Vec<u8> = Vec::new();
    while i < b.len() {
        let c = b[i];
        if c == b'\\' && i + 1 < b.len() {
            out.push(b[i + 1]);
            i += 2;
            continue;
        }
        if c == quote {
            i += 1;
            break;
        }
        if c == b'\n' && quote != b'`' {
            break;
        }
        out.push(c);
        i += 1;
    }
    let mut s = String::from_utf8_lossy(&out).into_owned();
    if quote == b'`' {
        if let Some(p) = s.rfind('}') {
            s = s[p + 1..].to_string();
        }
    }
    (s, i)
}

/// Image-path string literals inside the value bound to each `bumpTexture` in `js`.
pub fn bump_texture_literals(js: &str) -> Vec<String> {
    let b = js.as_bytes();
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(off) = find(&b[from..], KEY) {
        let pos = from + off;
        from = pos + KEY.len();
        if pos > 0 && is_ident_byte(b[pos - 1]) {
            continue;
        }
        let mut i = pos + KEY.len();
        if i < b.len() && is_ident_byte(b[i]) {
            continue;
        }
        if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
            i += 1;
        }
        while i < b.len() && (b[i] == b' ' || b[i] == b'\t' || b[i] == b'\r' || b[i] == b'\n') {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        match b[i] {
            b':' => i += 1,
            b'=' if b.get(i + 1).is_some_and(|&c| c != b'=' && c != b'>') => i += 1,
            _ => continue,
        }
        collect_expression_literals(b, i, &mut out);
    }
    out
}

/// Walks the expression starting at `b[start]` up to the `,` `;` or closing bracket that
/// ends it at depth zero, or a line break once the expression has started, and pushes the
/// image-path literals inside it. Stopping early is the safe direction: a literal from the
/// next property would mark a colour texture as a normal map.
fn collect_expression_literals(b: &[u8], start: usize, out: &mut Vec<String>) {
    let end = (start + MAX_EXPR_BYTES).min(b.len());
    let mut depth = 0usize;
    let mut started = false;
    let mut i = start;
    while i < end {
        let c = b[i];
        match c {
            b'"' | b'\'' | b'`' => {
                let (s, next) = read_string(b, i);
                if is_image_path(&s) {
                    out.push(s);
                }
                i = next;
                started = true;
                continue;
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                if depth == 0 {
                    return;
                }
                depth -= 1;
            }
            b',' | b';' if depth == 0 => return,
            b'\n' if depth == 0 && started => return,
            b' ' | b'\t' | b'\r' | b'\n' => {}
            _ => started = true,
        }
        i += 1;
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Content hashes of the entity files a literal names: the whole path, or a tail of it that
/// starts on a `/` boundary. Case-insensitive, like the content server's file map.
pub fn resolve_literal<'a>(
    literal: &str,
    content_by_file: &'a HashMap<String, String>,
) -> Vec<&'a String> {
    let l = literal.trim().to_lowercase();
    let l = l.trim_start_matches("./");
    let bare = l.trim_start_matches('/');
    if bare.is_empty() {
        return Vec::new();
    }
    let tail = format!("/{bare}");
    content_by_file
        .iter()
        .filter(|(f, _)| {
            let f = f.to_lowercase();
            f == bare || f.ends_with(&tail)
        })
        .map(|(_, h)| h)
        .collect()
}

/// Whether an image's file name ends in a normal-map suffix ([`NORMAL_NAME_SUFFIXES`]).
pub fn has_normal_map_name(file: &str) -> bool {
    if !is_image_path(file) {
        return false;
    }
    let base = file.rsplit(['/', '\\']).next().unwrap_or(file);
    let stem = base
        .rsplit_once('.')
        .map_or(base, |(s, _)| s)
        .to_ascii_lowercase();
    NORMAL_NAME_SUFFIXES
        .iter()
        .any(|sfx| stem.len() > sfx.len() && stem.ends_with(sfx))
}

/// Content hashes of the images to build as linear normal maps beyond what the glTFs
/// declare: everything [`bump_texture_hashes`] finds, plus every image that passes the name
/// rule (see the module docs) and is neither referenced by a glTF (`model_refs`) nor already
/// a glTF normal map (`glb_normals`). `fetch` returns a file's bytes, or `None` when it is not
/// available locally; a missing file contributes nothing.
pub fn code_normal_hashes(
    content_by_file: &HashMap<String, String>,
    model_refs: &HashSet<String>,
    glb_normals: &HashSet<String>,
    fetch: impl Fn(&str) -> Option<Vec<u8>>,
) -> HashSet<String> {
    let mut out = bump_texture_hashes(content_by_file, &fetch);
    let mut checked: HashSet<&str> = HashSet::new();
    for (f, h) in content_by_file {
        if !has_normal_map_name(f)
            || model_refs.contains(h)
            || glb_normals.contains(h)
            || out.contains(h)
            || !checked.insert(h.as_str())
        {
            continue;
        }
        if fetch(h).is_some_and(|raw| crate::builder::image_looks_like_normal_map(&raw)) {
            out.insert(h.clone());
        }
    }
    out
}

/// Content hashes of every image the entity's scripts bind as `bumpTexture`. `fetch`
/// returns a script's bytes, or `None` when it is not available locally; a missing script
/// contributes nothing.
pub fn bump_texture_hashes(
    content_by_file: &HashMap<String, String>,
    fetch: impl Fn(&str) -> Option<Vec<u8>>,
) -> HashSet<String> {
    let mut seen_scripts: HashSet<&str> = HashSet::new();
    let mut out = HashSet::new();
    for (f, h) in content_by_file {
        if !is_scene_script(f) || !seen_scripts.insert(h.as_str()) {
            continue;
        }
        let Some(bytes) = fetch(h) else {
            continue;
        };
        let js = String::from_utf8_lossy(&bytes);
        for lit in bump_texture_literals(&js) {
            // The literal ends in an image extension and a match ends with the literal, so
            // every resolved file is an image.
            out.extend(resolve_literal(&lit, content_by_file).into_iter().cloned());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cozyfishing.dcl.eth water material, as the SDK7 bundler minifies it: the folder
    /// is a variable, the file name the literal, and the albedo `texture` sits beside it.
    const SDK7_MINIFIED: &str = r#"x={material:{$case:"pbr",pbr:{castShadows:!1,roughness:1e-4,texture:t?je.Texture.Common({src:ge.assetFolder.waterPond+"water.png",wrapMode:Pi.TWM_REPEAT}):void 0,bumpTexture:je.Texture.Common({src:ge.assetFolder.waterPond+"PoolBake_normal.png",wrapMode:Pi.TWM_REPEAT,filterMode:Jp.TFM_BILINEAR})}}}"#;

    #[test]
    fn sdk7_minified_takes_only_the_bump_literal() {
        assert_eq!(
            bump_texture_literals(SDK7_MINIFIED),
            vec!["PoolBake_normal.png"]
        );
    }

    #[test]
    fn bump_before_albedo_stops_at_its_own_value() {
        let js = r#"Material.setPbrMaterial(e,{bumpTexture:Material.Texture.Common({src:"images/n.png"}),texture:Material.Texture.Common({src:"images/albedo.png"})})"#;
        assert_eq!(bump_texture_literals(js), vec!["images/n.png"]);
    }

    #[test]
    fn sdk6_assignment() {
        let js = "var m = new Material();\nm.albedoTexture = new Texture(\"a.png\");\nm.bumpTexture = new Texture(\"models/n.png\");\nm.metallic = 0;";
        assert_eq!(bump_texture_literals(js), vec!["models/n.png"]);
    }

    #[test]
    fn sdk6_assignment_without_semicolons_stops_at_the_line() {
        let js = "m.bumpTexture = new Texture('n.png')\nm.albedoTexture = new Texture('a.png')\n";
        assert_eq!(bump_texture_literals(js), vec!["n.png"]);
    }

    #[test]
    fn quoted_key_and_template_tail() {
        let js = r#"{"bumpTexture": T({src: `${dir}/water/n.png`})}"#;
        assert_eq!(bump_texture_literals(js), vec!["/water/n.png"]);
    }

    #[test]
    fn ignores_codec_and_comparisons() {
        let js = r#"bumpTexture:e.bumpTexture?TextureUnion.toJSON(e.bumpTexture):void 0;if(a.bumpTexture==="x.png"){}"#;
        assert!(bump_texture_literals(js).is_empty());
        let js = "notbumpTexture:T('a.png'),bumpTextures:T('b.png')";
        assert!(bump_texture_literals(js).is_empty());
    }

    fn files(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(f, h)| (f.to_lowercase(), h.to_string()))
            .collect()
    }

    #[test]
    fn resolves_on_segment_boundary_only() {
        let c = files(&[
            ("assets/fishinggame/water/poolbake_normal.png", "H1"),
            ("assets/fishinggame/environment/poolbake_normal.png", "H1"),
            ("assets/other/xpoolbake_normal.png", "H2"),
        ]);
        let mut got: Vec<&String> = resolve_literal("PoolBake_normal.png", &c);
        got.sort();
        got.dedup();
        assert_eq!(got, vec!["H1"]);
        assert_eq!(
            resolve_literal("./assets/other/xpoolbake_normal.png", &c),
            vec!["H2"]
        );
        assert!(resolve_literal("bake_normal.png", &c).is_empty());
    }

    #[test]
    fn normal_map_names() {
        for f in [
            "assets/water/PoolBake_normal.png",
            "T_BrickWall_N.png",
            "rock_nrm.JPG",
            "floor_NormalMap.ktx2",
        ] {
            assert!(has_normal_map_name(f), "{f}");
        }
        for f in [
            "assets/normal.png",
            "_n.png",
            "button_on.png",
            "abnormal.png",
            "PoolBake_normal.glb",
            "notes_n.txt",
        ] {
            assert!(!has_normal_map_name(f), "{f}");
        }
    }

    fn png(px: [u8; 4]) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(8, 8, image::Rgba(px));
        let mut out = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn name_rule_needs_normal_pixels_and_no_gltf_use() {
        let c = files(&[
            ("a/flat_n.png", "FLAT"),
            ("a/sprite_n.png", "SPRITE"),
            ("a/used_normal.png", "USED"),
            ("a/plain.png", "PLAIN"),
        ]);
        let normal_px = png([128, 128, 255, 255]);
        let red_px = png([255, 0, 0, 255]);
        let got = code_normal_hashes(
            &c,
            &HashSet::from(["USED".to_string()]),
            &HashSet::new(),
            |h| match h {
                "FLAT" | "USED" | "PLAIN" => Some(normal_px.clone()),
                "SPRITE" => Some(red_px.clone()),
                _ => None,
            },
        );
        assert_eq!(got, HashSet::from(["FLAT".to_string()]));
    }

    #[test]
    fn bump_texture_hashes_reads_scripts_and_skips_missing() {
        let c = files(&[
            ("bin/index.js", "JS"),
            ("bin/missing.js", "GONE"),
            ("assets/water/PoolBake_normal.png", "NRM"),
            ("assets/water/water.png", "ALB"),
        ]);
        let got = bump_texture_hashes(&c, |h| match h {
            "JS" => Some(SDK7_MINIFIED.as_bytes().to_vec()),
            _ => None,
        });
        assert_eq!(got, HashSet::from(["NRM".to_string()]));
    }
}

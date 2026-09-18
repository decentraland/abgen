use anyhow::Result;
use std::collections::BTreeMap;
#[cfg(not(target_arch = "wasm32"))]
use std::path::Path;
use std::path::PathBuf;
pub const DEFAULT_AB_VERSION: &str = "v0-abgen";

pub const DEFAULT_CONTENT_SERVER_URL: &str = "https://peer.decentraland.org/content";

pub const EXIT_CONVERSION_ERRORS_TOLERATED: i32 = 12;

pub fn exit_code_for_failures(failures: usize) -> i32 {
    if failures == 0 {
        0
    } else {
        EXIT_CONVERSION_ERRORS_TOLERATED
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub struct CorpusManifestSpec<'a> {
    pub out_root: &'a Path,
    pub entity_id: &'a str,
    pub platform: &'a str,
    pub built: &'a [String],
    pub ab_version: &'a str,
    pub content_server_url: &'a str,
    pub exit_code: i32,
    pub date: &'a str,
    /// Generations of every recipe governing the bundles in `built` — the union
    /// [`crate::recipes::recorded_generations`] produced across them, baselines included —
    /// or `None` from a writer that cannot vouch for the whole set. See [`recipes_block`].
    pub recipes: Option<&'a std::collections::BTreeMap<&'static str, u32>>,
}

/// The `recipes` block the manifest carries, or `None` to leave the key out entirely.
///
/// Written whenever the writer can vouch for the whole set, at baseline or not — the block
/// records every recipe that governs the entity's bundles together with where each stood,
/// so it is only useful if it is there *before* the bump it is meant to catch. A block that
/// appeared only once something had been bumped would be too late to catch that first bump,
/// and a block that listed only the bumped recipes would be blind to every bump after it
/// ([`crate::recipes::recorded_generations`] explains why).
///
/// Absent therefore means one thing: written before recipes existed at all. A writer that
/// cannot vouch for the whole set passes `None` and takes the reconversion rather than
/// claim a currency it does not know. [`crate::recipes::recorded_is_current`] reads it back.
#[cfg(not(target_arch = "wasm32"))]
fn recipes_block(
    recipes: Option<&std::collections::BTreeMap<&'static str, u32>>,
) -> Option<serde_json::Value> {
    recipes.map(|r| serde_json::json!(r))
}

#[cfg(not(target_arch = "wasm32"))]
pub fn write_corpus_manifest(m: &CorpusManifestSpec) -> Result<PathBuf> {
    let mut files: Vec<String> = m.built.to_vec();
    files.sort();
    files.dedup();
    files.push("dcl".to_string());
    let mut manifest = serde_json::json!({
        "version": m.ab_version,
        "files": files,
        "exitCode": m.exit_code,
        "contentServerUrl": m.content_server_url,
        "date": m.date,
    });
    if let Some(block) = recipes_block(m.recipes) {
        manifest["recipes"] = block;
    }
    let dir = m
        .out_root
        .join(&*crate::naming::fs_safe_component(m.entity_id));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.manifest.json", m.platform));
    let text = serde_json::to_string_pretty(&manifest)?;
    let tmp = crate::tmppath::tmp_sibling(&path);
    std::fs::write(&tmp, &text)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

pub fn write_scene(
    out_dir: &str,
    entity_id: &str,
    platform: &str,
    bundles: &BTreeMap<String, Vec<u8>>,
    ab_version: &str,
    exit_code: i32,
    date: &str,
) -> Result<PathBuf> {
    crate::naming::ensure_writable_component(entity_id)?;
    for fname in bundles.keys() {
        crate::naming::ensure_writable_component(fname)?;
    }
    let base = PathBuf::from(out_dir).join(entity_id);
    let pdir = base.join(platform);
    std::fs::create_dir_all(&pdir)?;

    for (fname, data) in bundles {
        std::fs::write(pdir.join(fname), data)?;
    }

    let files: Vec<serde_json::Value> = bundles
        .keys()
        .map(|k| serde_json::Value::String(k.clone()))
        .collect();

    let manifest = serde_json::json!({
        "version": ab_version,
        "files": files,
        "exitCode": exit_code,
        "date": date,
    });

    let text = serde_json::to_string_pretty(&manifest)?;
    std::fs::write(base.join(format!("{platform}.manifest.json")), &text)?;
    Ok(base)
}

pub fn provenance(entity_id: &str) -> String {
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(entity_id.as_bytes());
    let inputs: String = h
        .finalize()
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("{inputs}+{}", env!("ABGEN_BUILD_ID"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_DATE: &str = "2026-01-02T03:04:05.000Z";

    #[test]
    fn corpus_manifest_shape_and_determinism() {
        let tmp =
            std::env::temp_dir().join(format!("abgen_corpus_manifest_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);

        let built = vec![
            "QmB_deadbeef_windows".to_string(),
            "QmA_deadbeef_windows".to_string(),
            "QmB_deadbeef_windows".to_string(),
        ];
        let p = write_corpus_manifest(&CorpusManifestSpec {
            out_root: &tmp,
            entity_id: "entityZ",
            platform: "windows",
            built: &built,
            ab_version: "v41",
            content_server_url: "http://cs",
            exit_code: 0,
            date: TEST_DATE,
            recipes: None,
        })
        .unwrap();
        assert_eq!(p, tmp.join("entityZ").join("windows.manifest.json"));
        let first = std::fs::read_to_string(&p).unwrap();
        let m: serde_json::Value = serde_json::from_str(&first).unwrap();
        assert_eq!(m["version"], "v41");
        assert_eq!(m["exitCode"], 0);
        assert_eq!(m["contentServerUrl"], "http://cs");
        assert_eq!(m["date"], TEST_DATE);
        assert_eq!(
            m["files"],
            serde_json::json!(["QmA_deadbeef_windows", "QmB_deadbeef_windows", "dcl"])
        );

        let second = std::fs::read_to_string(
            write_corpus_manifest(&CorpusManifestSpec {
                out_root: &tmp,
                entity_id: "entityZ",
                platform: "windows",
                built: &built,
                ab_version: "v41",
                content_server_url: "http://cs",
                exit_code: 0,
                date: TEST_DATE,
                recipes: None,
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(first, second);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn recipes_block_is_written_whenever_the_writer_can_vouch_for_it() {
        // Written at baseline too: the block has to be in place *before* the bump it is
        // meant to catch, so gating it on `any_bumped` would miss the first one.
        let recorded = crate::recipes::recorded_generations(&crate::recipes::Recipe::ALL);
        assert!(recipes_block(Some(&recorded)).is_some());
        assert!(recipes_block(Some(&BTreeMap::new())).is_some());
        // A writer that cannot vouch for the set never writes the block, so its manifests
        // reconvert once after a bump rather than claiming a currency they don't know.
        assert!(recipes_block(None).is_none());

        let tmp = std::env::temp_dir().join(format!("abgen_corpus_recipes_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let built = vec!["QmA_deadbeef_windows".to_string()];
        let p = write_corpus_manifest(&CorpusManifestSpec {
            out_root: &tmp,
            entity_id: "entityZ",
            platform: "windows",
            built: &built,
            ab_version: "v41",
            content_server_url: "http://cs",
            exit_code: 0,
            date: TEST_DATE,
            recipes: Some(&recorded),
        })
        .unwrap();
        let m: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        // Appended after `date`, so nothing about the existing field order moves.
        assert_eq!(
            m.as_object().unwrap().keys().last().map(String::as_str),
            Some("recipes")
        );
        // Whatever the table says, a conversion's own record of it reads back as current.
        assert!(crate::recipes::recorded_is_current(m.get("recipes")));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn corpus_manifest_carries_nonzero_exit_code() {
        let tmp =
            std::env::temp_dir().join(format!("abgen_corpus_exitcode_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let built = vec!["QmA_deadbeef_windows".to_string()];
        let p = write_corpus_manifest(&CorpusManifestSpec {
            out_root: &tmp,
            entity_id: "entityZ",
            platform: "windows",
            built: &built,
            ab_version: "v41",
            content_server_url: "http://cs",
            exit_code: exit_code_for_failures(3),
            date: TEST_DATE,
            recipes: None,
        })
        .unwrap();
        let m: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(m["exitCode"], EXIT_CONVERSION_ERRORS_TOLERATED);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn exit_code_mapping_matches_upstream_error_codes() {
        assert_eq!(exit_code_for_failures(0), 0);
        assert_eq!(exit_code_for_failures(1), 12);
        assert_eq!(exit_code_for_failures(352), 12);
        assert_eq!(EXIT_CONVERSION_ERRORS_TOLERATED, 12);
    }

    #[test]
    fn manifest_date_is_the_passed_build_scoped_date_not_provenance() {
        let tmp = std::env::temp_dir().join(format!("abgen_corpus_date_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let built: Vec<String> = Vec::new();
        let p = write_corpus_manifest(&CorpusManifestSpec {
            out_root: &tmp,
            entity_id: "entityZ",
            platform: "mac",
            built: &built,
            ab_version: "v41",
            content_server_url: "http://cs",
            exit_code: 0,
            date: TEST_DATE,
            recipes: None,
        })
        .unwrap();
        let m: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(m["date"], TEST_DATE);
        assert_ne!(m["date"], provenance("entityZ"));
        assert!(chrono::DateTime::parse_from_rfc3339(m["date"].as_str().unwrap()).is_ok());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn write_scene_refuses_oversized_names_before_writing() {
        let tmp = std::env::temp_dir().join(format!("abgen_manifest_guard_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let long = format!("b64-{}", "Q".repeat(260));

        let mut bundles = BTreeMap::new();
        bundles.insert("a_windows".to_string(), b"x".to_vec());
        let err = write_scene(
            tmp.to_str().unwrap(),
            &long,
            "windows",
            &bundles,
            DEFAULT_AB_VERSION,
            0,
            TEST_DATE,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("Refusing to write"), "{err}");

        let mut bundles = BTreeMap::new();
        bundles.insert("a_windows".to_string(), b"x".to_vec());
        bundles.insert(format!("{long}_windows"), b"x".to_vec());
        let err = write_scene(
            tmp.to_str().unwrap(),
            "entityY",
            "windows",
            &bundles,
            DEFAULT_AB_VERSION,
            0,
            TEST_DATE,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("Refusing to write"), "{err}");
        assert!(
            !tmp.exists(),
            "all names are vetted before anything is written"
        );
    }

    #[test]
    fn writes_layout() {
        let tmp = std::env::temp_dir().join(format!("abgen_manifest_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let mut bundles = BTreeMap::new();
        bundles.insert("b_windows".to_string(), b"data2".to_vec());
        bundles.insert("a_windows".to_string(), b"data1".to_vec());
        let base = write_scene(
            tmp.to_str().unwrap(),
            "entityX",
            "windows",
            &bundles,
            DEFAULT_AB_VERSION,
            0,
            TEST_DATE,
        )
        .unwrap();
        assert!(base.join("windows").join("a_windows").exists());
        let mtext = std::fs::read_to_string(base.join("windows.manifest.json")).unwrap();
        let m: serde_json::Value = serde_json::from_str(&mtext).unwrap();
        assert_eq!(m["version"], "v0-abgen");
        assert_eq!(m["exitCode"], 0);
        assert_eq!(m["date"], TEST_DATE);

        assert_eq!(m["files"][0], "a_windows");
        assert_eq!(m["files"][1], "b_windows");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

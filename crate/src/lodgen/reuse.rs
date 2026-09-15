//! Deciding whether a LOD build already exists, without a registry.
//!
//! A LOD build is a pure function of a small amount of state, and every deployment that
//! would produce the same bundles can find them by content address rather than by asking
//! who deployed before it. Two indexes make that possible, both keyed by a sha256 digest
//! and both storing a [`ReuseRecord`] that names the scene whose published bundles are the
//! bytes a fresh build would produce:
//!
//! - `lod-reuse/by-inputs/{digest}.json`, keyed by [`Inputs`]: what the scene runtime is a
//!   function of. Equal inputs mean equal placements without executing anything.
//! - `lod-reuse/by-state/{digest}.json`, keyed by the LOD state document from
//!   [`super::lod_state`]: what the build is a function of once placements are known.
//!   Equal state means equal bundles even when the code that produced it changed.
//!
//! A record is written by the job that builds and rewritten by every job that reuses it,
//! so `builtBy` names the most recent deployment holding the bytes. Reuse is an
//! optimization: a record that is missing, unparsable, or fails any check below costs a
//! build, never a job.

use crate::catalyst::Scene;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

pub const INPUTS_INDEX_PREFIX: &str = "lod-reuse/by-inputs/";
pub const STATE_INDEX_PREFIX: &str = "lod-reuse/by-state/";

pub fn inputs_index_key(digest: &str) -> String {
    format!("{INPUTS_INDEX_PREFIX}{digest}.json")
}

pub fn state_index_key(digest: &str) -> String {
    format!("{STATE_INDEX_PREFIX}{digest}.json")
}

/// Everything the embedded scene runtime's output is a function of.
///
/// The runtime gives the scene no real network and a fixed virtual clock, so its 90 frames
/// are decided by the code it evaluates, the `main.crdt` snapshot it starts from, and the
/// parcels the result is cropped to. Hold these equal and executing the scene cannot
/// produce different placements. What those placements resolve to — the glTFs, buffers and
/// textures — is checked separately against [`ReuseRecord::dependencies`], because it can
/// change under identical inputs when an asset is re-uploaded under the same name.
///
/// Only SDK7 scenes have inputs. An SDK6 scene runs through an adaption layer fetched from
/// the network at build time, which is an input this document cannot pin, so those scenes
/// go straight to the state comparison.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Inputs {
    pub generation: String,
    pub runtime_version: String,
    pub base: String,
    /// Sorted `x,y` strings, so parcel order in `scene.json` does not matter.
    pub parcels: Vec<String>,
    /// Hash of `main.crdt`, or `None` when the deployment ships none. A scene that adds a
    /// snapshot changes its inputs even when nothing else moved.
    pub main_crdt: Option<String>,
    /// Lower-cased `file -> hash` of every `.js` file plus the scene's `main` entry.
    pub code: BTreeMap<String, String>,
}

/// The [`Inputs`] of a resolved scene, or `None` when the scene has no pinnable inputs.
pub fn inputs(ent: &Scene) -> Result<Option<Inputs>> {
    let runtime_version = ent
        .metadata
        .get("runtimeVersion")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if runtime_version != "7" {
        return Ok(None);
    }
    let (base, parcels) = super::scene_geometry(ent)?;
    let mut parcels: Vec<String> = parcels.iter().map(|(x, y)| format!("{x},{y}")).collect();
    parcels.sort();
    parcels.dedup();
    let content = ent.content_by_file();
    let mut code: BTreeMap<String, String> = content
        .iter()
        .filter(|(file, _)| file.ends_with(".js"))
        .map(|(file, hash)| (file.clone(), hash.clone()))
        .collect();
    if let Some(main) = ent.metadata.get("main").and_then(|v| v.as_str()) {
        let lower = main.to_lowercase();
        let hash = content
            .get(&lower)
            .ok_or_else(|| anyhow!("scene {} content does not list main {main}", ent.entity_id))?;
        code.insert(lower, hash.clone());
    }
    if code.is_empty() {
        // Nothing executes, so nothing can be reasoned about; let execution decide.
        return Ok(None);
    }
    Ok(Some(Inputs {
        generation: super::LOD_GENERATION.to_string(),
        runtime_version: runtime_version.to_string(),
        base: format!("{},{}", base.0, base.1),
        parcels,
        main_crdt: content.get("main.crdt").cloned(),
        code,
    }))
}

/// Content address of an [`Inputs`] document. Field order is fixed by the struct and the
/// maps are sorted, so equal inputs always digest the same.
pub fn inputs_digest(inputs: &Inputs) -> String {
    crate::hashes::sha256_hex(&serde_json::to_vec(inputs).unwrap_or_default())
}

/// What one build published and what it was a function of.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ReuseRecord {
    /// The most recent scene whose per-scene keys hold these bundles.
    pub built_by: String,
    pub generation: String,
    pub levels: Vec<u32>,
    pub platforms: Vec<String>,
    /// Every object `built_by` published, so a job wanting more than this build produced
    /// misses cheaply instead of failing a copy halfway.
    pub keys: Vec<String>,
    /// Lower-cased `file -> hash` of every deployment file the build read.
    pub dependencies: BTreeMap<String, String>,
    /// glTF sources the scene named that its deployment did not ship. If a later
    /// deployment ships one, the geometry gains an asset the record never saw.
    #[serde(default)]
    pub unresolved: Vec<String>,
    /// The inputs the build ran under, when it had pinnable ones, and their digest: the
    /// `by-inputs` key this record is (also) filed under.
    #[serde(default)]
    pub inputs: Option<Inputs>,
    #[serde(default)]
    pub inputs_digest: Option<String>,
    /// The LOD state document the build was a function of, and its digest: the `by-state`
    /// key this record is filed under.
    pub state: serde_json::Value,
    pub state_digest: String,
}

impl ReuseRecord {
    pub fn covers(&self, levels: &[u32], platforms: &[String]) -> bool {
        levels.iter().all(|l| self.levels.contains(l))
            && platforms.iter().all(|p| self.platforms.contains(p))
    }

    /// Why this record cannot stand in for a build of the deployment whose lower-cased
    /// `file -> hash` listing is `content`, or `Ok` when nothing the build read has changed.
    pub fn check_content(&self, content: &HashMap<String, String>) -> Result<(), String> {
        if self.generation != super::LOD_GENERATION {
            return Err(format!(
                "generation {} is not {}",
                self.generation,
                super::LOD_GENERATION
            ));
        }
        for (file, hash) in &self.dependencies {
            match content.get(file) {
                Some(now) if now == hash => {}
                Some(_) => return Err(format!("{file} changed")),
                None => return Err(format!("{file} is gone")),
            }
        }
        for src in &self.unresolved {
            if content.contains_key(&src.to_lowercase()) {
                return Err(format!("{src} is now shipped"));
            }
        }
        Ok(())
    }

    /// Whether the bundles this record names are what a build under `inputs` would produce
    /// for a deployment listing `content`.
    pub fn accepts_inputs(
        &self,
        inputs: &Inputs,
        content: &HashMap<String, String>,
        levels: &[u32],
        platforms: &[String],
    ) -> Result<(), String> {
        if self.inputs.as_ref() != Some(inputs) {
            return Err("inputs differ".to_string());
        }
        self.accept(content, levels, platforms)
    }

    /// Whether the bundles this record names are what a build from `state` would produce
    /// for a deployment listing `content`.
    pub fn accepts_state(
        &self,
        state: &serde_json::Value,
        content: &HashMap<String, String>,
        levels: &[u32],
        platforms: &[String],
    ) -> Result<(), String> {
        if &self.state != state {
            return Err("state differs".to_string());
        }
        self.accept(content, levels, platforms)
    }

    fn accept(
        &self,
        content: &HashMap<String, String>,
        levels: &[u32],
        platforms: &[String],
    ) -> Result<(), String> {
        if !self.covers(levels, platforms) {
            return Err(format!(
                "built levels {:?} platforms {:?}, need {levels:?} {platforms:?}",
                self.levels, self.platforms
            ));
        }
        self.check_content(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalyst::ContentEntry;

    fn scene(runtime: &str, parcels: &[&str], content: &[(String, String)]) -> Scene {
        Scene {
            entity_id: "bafkscene".into(),
            entity_type: "scene".into(),
            pointers: parcels.iter().map(|p| p.to_string()).collect(),
            content: content
                .iter()
                .map(|(file, hash)| ContentEntry {
                    file: file.clone(),
                    hash: hash.clone(),
                })
                .collect(),
            metadata: serde_json::json!({
                "runtimeVersion": runtime,
                "main": "bin/index.js",
                "scene": {"base": parcels[0], "parcels": parcels},
            }),
        }
    }

    const BASE: &[(&str, &str)] = &[
        ("bin/index.js", "bafkcode"),
        ("main.crdt", "bafkcrdt"),
        ("scene.json", "bafkscenejson"),
        ("models/Tree.glb", "bafktree"),
        ("models/tree.png", "bafkbark"),
        ("scene-thumbnail.png", "bafkthumb"),
    ];

    fn owned(content: &[(&str, &str)]) -> Vec<(String, String)> {
        content
            .iter()
            .map(|(f, h)| (f.to_string(), h.to_string()))
            .collect()
    }

    /// `BASE` with `file` set to `hash`, added when absent.
    fn with(file: &str, hash: &str) -> Vec<(String, String)> {
        let mut out = owned(BASE);
        if let Some(slot) = out.iter_mut().find(|(f, _)| f == file) {
            slot.1 = hash.to_string();
        } else {
            out.push((file.to_string(), hash.to_string()));
        }
        out
    }

    fn without(file: &str) -> Vec<(String, String)> {
        owned(BASE).into_iter().filter(|(f, _)| f != file).collect()
    }

    fn digest_of(runtime: &str, parcels: &[&str], content: &[(String, String)]) -> Option<String> {
        inputs(&scene(runtime, parcels, content))
            .unwrap()
            .map(|i| inputs_digest(&i))
    }

    fn base_digest() -> String {
        digest_of("7", &["0,0", "1,0"], &owned(BASE)).unwrap()
    }

    #[test]
    fn inputs_ignore_what_cannot_move_a_placement() {
        let base = base_digest();
        let thumb = digest_of(
            "7",
            &["0,0", "1,0"],
            &with("scene-thumbnail.png", "bafknew"),
        );
        assert_eq!(thumb.as_deref(), Some(base.as_str()), "thumbnail");
        let retextured = digest_of("7", &["0,0", "1,0"], &with("models/tree.png", "bafknew"));
        assert_eq!(
            retextured.as_deref(),
            Some(base.as_str()),
            "asset bytes are dependencies, not inputs"
        );
        let added = digest_of("7", &["0,0", "1,0"], &with("models/extra.glb", "bafkextra"));
        assert_eq!(added.as_deref(), Some(base.as_str()), "unplaced asset");
        let other_base = digest_of("7", &["1,0", "0,0"], &owned(BASE));
        assert_ne!(
            other_base.as_deref(),
            Some(base.as_str()),
            "base moved: the crop moves"
        );
        let same_parcels_other_order = {
            let mut s = scene("7", &["0,0", "1,0"], &owned(BASE));
            s.metadata["scene"]["parcels"] = serde_json::json!(["1,0", "0,0"]);
            inputs_digest(&inputs(&s).unwrap().unwrap())
        };
        assert_eq!(same_parcels_other_order, base, "parcel order in scene.json");
    }

    #[test]
    fn inputs_move_with_everything_that_decides_placements() {
        let base = base_digest();
        for (file, hash) in [
            ("bin/index.js", "bafkcode2"),
            ("main.crdt", "bafkcrdt2"),
            ("bin/other.js", "bafkmore"),
        ] {
            let d = digest_of("7", &["0,0", "1,0"], &with(file, hash)).unwrap();
            assert_ne!(d, base, "{file}");
        }
        let fewer_parcels = digest_of("7", &["0,0"], &owned(BASE)).unwrap();
        assert_ne!(fewer_parcels, base);
        assert_ne!(
            digest_of("7", &["0,0", "1,0"], &without("main.crdt")).unwrap(),
            base
        );

        let mut bumped = inputs(&scene("7", &["0,0", "1,0"], &owned(BASE)))
            .unwrap()
            .unwrap();
        bumped.generation = "999".to_string();
        assert_ne!(inputs_digest(&bumped), base);
    }

    #[test]
    fn only_sdk7_scenes_with_code_have_inputs() {
        assert!(inputs(&scene("6", &["0,0"], &owned(BASE)))
            .unwrap()
            .is_none());
        let mut no_runtime = scene("7", &["0,0"], &owned(BASE));
        no_runtime
            .metadata
            .as_object_mut()
            .unwrap()
            .remove("runtimeVersion");
        assert!(inputs(&no_runtime).unwrap().is_none());
        let mut main_missing = scene("7", &["0,0"], &owned(BASE));
        main_missing.metadata["main"] = serde_json::json!("bin/game.js");
        assert!(
            inputs(&main_missing).is_err(),
            "a main the deployment lacks is an error, not a guess"
        );
    }

    fn record() -> ReuseRecord {
        let s = scene("7", &["0,0", "1,0"], &owned(BASE));
        let ins = inputs(&s).unwrap();
        let state = serde_json::json!({"generation": "1", "descriptor": {}, "primitives": []});
        ReuseRecord {
            built_by: "bafkprevious".into(),
            generation: super::super::LOD_GENERATION.to_string(),
            levels: vec![1],
            platforms: vec!["windows".into(), "mac".into()],
            keys: vec![
                "LOD/1/bafkprevious_1_windows".into(),
                "LOD/1/bafkprevious_1_mac".into(),
            ],
            dependencies: [
                ("bin/index.js", "bafkcode"),
                ("main.crdt", "bafkcrdt"),
                ("models/tree.glb", "bafktree"),
                ("models/tree.png", "bafkbark"),
            ]
            .into_iter()
            .map(|(f, h)| (f.to_string(), h.to_string()))
            .collect(),
            unresolved: vec!["models/Missing.glb".into()],
            inputs_digest: ins.as_ref().map(inputs_digest),
            inputs: ins,
            state_digest: super::super::state_digest(&state),
            state,
        }
    }

    fn listing(content: &[(String, String)]) -> HashMap<String, String> {
        content
            .iter()
            .map(|(f, h)| (f.to_lowercase(), h.clone()))
            .collect()
    }

    #[test]
    fn a_record_accepts_the_deployment_it_describes_and_harmless_changes() {
        let r = record();
        let ins = r.inputs.clone().unwrap();
        let plats = vec!["windows".to_string(), "mac".to_string()];
        let same = owned(BASE);
        assert_eq!(
            r.accepts_inputs(&ins, &listing(&same), &[1], &plats),
            Ok(())
        );
        let thumb = with("scene-thumbnail.png", "bafknew");
        assert_eq!(
            r.accepts_inputs(&ins, &listing(&thumb), &[1], &plats),
            Ok(())
        );
        let extra = with("models/unused.glb", "bafkunused");
        assert_eq!(
            r.accepts_inputs(&ins, &listing(&extra), &[1], &plats),
            Ok(())
        );
        // One platform of two is covered.
        assert_eq!(
            r.accepts_inputs(&ins, &listing(&same), &[1], &plats[..1]),
            Ok(())
        );
    }

    #[test]
    fn a_dependency_served_differently_rejects_even_when_inputs_match() {
        let r = record();
        let ins = r.inputs.clone().unwrap();
        let plats = vec!["windows".to_string()];
        // The texture the glTF references was re-uploaded: same descriptor, different bundle.
        let retextured = with("models/tree.png", "bafknew");
        assert_eq!(
            r.accepts_inputs(&ins, &listing(&retextured), &[1], &plats),
            Err("models/tree.png changed".to_string())
        );
        assert_eq!(
            r.accepts_inputs(&ins, &listing(&without("models/tree.png")), &[1], &plats),
            Err("models/tree.png is gone".to_string())
        );
        // A source the build could not resolve now ships: the geometry gained an asset.
        let now_shipped = with("models/missing.glb", "bafkfound");
        assert_eq!(
            r.accepts_inputs(&ins, &listing(&now_shipped), &[1], &plats),
            Err("models/Missing.glb is now shipped".to_string())
        );
    }

    #[test]
    fn coverage_generation_and_document_equality_all_gate_reuse() {
        let r = record();
        let ins = r.inputs.clone().unwrap();
        let same = listing(&owned(BASE));
        let plats = vec!["windows".to_string()];
        assert!(
            r.accepts_inputs(&ins, &same, &[0, 1], &plats).is_err(),
            "level 0 was not built"
        );
        assert!(
            r.accepts_inputs(&ins, &same, &[1], &["linux".to_string()])
                .is_err(),
            "linux was not built"
        );
        let mut other = ins.clone();
        other.code.insert("bin/index.js".into(), "bafkcode2".into());
        assert_eq!(
            r.accepts_inputs(&other, &same, &[1], &plats),
            Err("inputs differ".to_string())
        );
        let mut old = r.clone();
        old.generation = "0".to_string();
        assert!(old.accepts_inputs(&ins, &same, &[1], &plats).is_err());

        assert_eq!(r.accepts_state(&r.state, &same, &[1], &plats), Ok(()));
        let moved =
            serde_json::json!({"generation": "1", "descriptor": {"assets": [1]}, "primitives": []});
        assert_eq!(
            r.accepts_state(&moved, &same, &[1], &plats),
            Err("state differs".to_string())
        );
    }

    #[test]
    fn state_equality_ignores_key_order_but_not_values() {
        let mut r = record();
        r.state = serde_json::json!({"a": 1, "b": [1.5, 2]});
        let same_other_order: serde_json::Value =
            serde_json::from_str(r#"{"b": [1.5, 2], "a": 1}"#).unwrap();
        let content = listing(&owned(BASE));
        assert_eq!(
            r.accepts_state(&same_other_order, &content, &[1], &["windows".to_string()]),
            Ok(())
        );
    }

    #[test]
    fn records_round_trip_and_tolerate_older_shapes() {
        let r = record();
        let text = serde_json::to_string_pretty(&r).unwrap();
        assert!(
            text.contains("\"builtBy\""),
            "camelCase on the wire: {text}"
        );
        let back: ReuseRecord = serde_json::from_str(&text).unwrap();
        assert_eq!(back, r);

        // A record without the optional fields still parses; one missing a required
        // field does not, and the caller treats that as a miss.
        let older = serde_json::json!({
            "builtBy": "bafkold", "generation": "1", "levels": [1], "platforms": ["windows"],
            "keys": [], "dependencies": {}, "state": {}, "stateDigest": "d"
        });
        let parsed: ReuseRecord = serde_json::from_value(older).unwrap();
        assert!(parsed.unresolved.is_empty());
        assert!(parsed.inputs.is_none());
        assert!(parsed.inputs_digest.is_none());
        let truncated = serde_json::json!({"builtBy": "bafkold"});
        assert!(serde_json::from_value::<ReuseRecord>(truncated).is_err());
    }

    #[test]
    fn index_keys_live_under_the_no_cache_prefix() {
        assert_eq!(inputs_index_key("abc"), "lod-reuse/by-inputs/abc.json");
        assert_eq!(state_index_key("abc"), "lod-reuse/by-state/abc.json");
        assert_eq!(
            crate::space::object_headers(&inputs_index_key("abc")).cache_control,
            "private, max-age=0, no-cache"
        );
    }
}

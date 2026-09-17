//! Per-asset-type converter cache keys.
//!
//! `AB_VERSION` prefixes every CDN key, so bumping it orphans every bundle of every type at
//! once. That is the right hammer for a change to the bundle container itself, and much too
//! big for the usual fix, which changes how one kind of asset is built and leaves the rest
//! byte-identical. Baking skinned-renderer bounds over the animation clips (#119) rewrote
//! nothing but animated rigs, yet an `AB_VERSION` bump behind it would have rebuilt every
//! texture in the world.
//!
//! A recipe is a generation counter scoped to one build-affecting behaviour. Each one folds
//! into the content digest that names the bundles it can move, so bumping it renames exactly
//! those bundles: they miss the CDN probe and rebuild, while every bundle whose recipes did
//! not move keeps its name, stays where it is, and is reused. It is the same device
//! [`crate::lodgen::LOD_GENERATION`] already applies to the LOD lane, cut finer.
//!
//! ## Bumping one
//!
//! Change the number in [`Recipe::generation`], by exactly one, in the same commit as the
//! fix. Pick the narrowest recipe whose *output* the fix changes — not the code it touches.
//! Never renumber and never reuse a value: a generation that comes back around makes stale
//! bundles look fresh again.
//!
//! The mapping from an asset to its recipes ([`gltf_recipes`], [`image_recipes`]) is a
//! deliberate over-approximation: it reads what a glTF *contains*, not what a given fix
//! actually changed, so a bump may rebuild a few assets whose bytes were already correct.
//! It must never be the other way round, which is why the mapping keys off structure the
//! source file declares rather than anything the converter decides later.
//!
//! ## What a bump does not cover
//!
//! Only the digest-named lane carries recipes — scenes, where a bundle is
//! `{hash}_{digest}_{platform}`. Wearables and emotes are named `{hash}_{platform}`, with
//! nowhere to put a generation, so a fix to those still needs `AB_VERSION`. So does anything
//! that changes the bundle container, the manifest shape, or the client's side of the
//! contract.
//!
//! ## The baseline
//!
//! Generation `0` folds in as nothing at all: the digest payload of an asset whose recipes
//! are all at `0` is byte-for-byte what it was before this module existed, and so is its
//! manifest. Adopting recipes therefore costs no rebuild. The first bump is what starts
//! charging — see [`crate::manifest`] for how the conversion gate reads the recorded
//! generations back.

use std::collections::BTreeMap;

/// A generation of `0` contributes nothing, so a recipe nobody has bumped leaves every
/// digest exactly where it was.
pub const BASELINE: u32 = 0;

/// One build-affecting behaviour of the converter, versioned independently of the others.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum Recipe {
    /// Vertex data and its layout: positions, normals, tangents, UVs, index buffers,
    /// Draco decode, mesh compression, bounds of unskinned renderers.
    Mesh,
    /// Skinning: bind poses, bone weights, the skeleton, and the bounds a
    /// `SkinnedMeshRenderer` ships. Applies to any glTF that declares a skin, including
    /// one with no clips of its own — a fix here can move its rest-pose box too.
    Skin,
    /// Animation clips: curve sampling, interpolation, and the Mecanim tracks built from
    /// them.
    Animation,
    /// Material and shader authoring: property values, keywords, render queue.
    Material,
    /// Texture decode and GPU encode — the BC7/DXT/crunch lanes, mip generation, resize,
    /// colour space. Applies to standalone image bundles and to any glTF carrying images,
    /// since those are encoded into the GLB's own bundle.
    Texture,
    /// The normal-map lane specifically: BC5, channel packing, and the reconstruction the
    /// shader expects.
    NormalMap,
}

impl Recipe {
    pub const ALL: [Recipe; 6] = [
        Recipe::Mesh,
        Recipe::Skin,
        Recipe::Animation,
        Recipe::Material,
        Recipe::Texture,
        Recipe::NormalMap,
    ];

    /// Stable key this recipe is recorded under, in digests and in manifests. Renaming one
    /// invalidates every bundle that folded it in, so these are as fixed as the numbers.
    pub const fn name(self) -> &'static str {
        match self {
            Recipe::Mesh => "mesh",
            Recipe::Skin => "skin",
            Recipe::Animation => "animation",
            Recipe::Material => "material",
            Recipe::Texture => "texture",
            Recipe::NormalMap => "normalMap",
        }
    }

    /// The current generation. Bump by one, in the commit that changes the output.
    pub const fn generation(self) -> u32 {
        match self {
            Recipe::Mesh => 0,
            Recipe::Skin => 0,
            Recipe::Animation => 0,
            Recipe::Material => 0,
            Recipe::Texture => 0,
            Recipe::NormalMap => 0,
        }
    }

    pub fn from_name(name: &str) -> Option<Recipe> {
        Recipe::ALL.into_iter().find(|r| r.name() == name)
    }
}

/// The recorded generations of `recipes`, dropping everything still at [`BASELINE`].
///
/// A [`BTreeMap`] so the JSON it serializes to is key-ordered and a digest over it is
/// stable, and so an empty result serializes to `{}` — the shape a caller folds in as
/// "nothing to say".
pub fn generations(recipes: &[Recipe]) -> BTreeMap<&'static str, u32> {
    recipes
        .iter()
        .filter(|r| r.generation() != BASELINE)
        .map(|r| (r.name(), r.generation()))
        .collect()
}

/// Whether any recipe at all has been bumped off [`BASELINE`].
///
/// While this is false the whole mechanism is inert: digests and manifests are byte-for-byte
/// what they were before it existed. It is what lets a manifest omit its `recipes` block
/// entirely rather than write an empty one, and what tells the conversion gate that a
/// manifest without the block is current rather than merely old.
pub fn any_bumped() -> bool {
    Recipe::ALL.iter().any(|r| r.generation() != BASELINE)
}

fn has_entries(doc: &serde_json::Value, key: &str) -> bool {
    doc.get(key)
        .and_then(|v| v.as_array())
        .is_some_and(|a| !a.is_empty())
}

/// The recipes a glTF's bundle bytes are a function of, read off the document's own
/// structure.
///
/// Deliberately coarse: presence of a `skins` array earns [`Recipe::Skin`] whether or not
/// any clip poses it, presence of `images` earns [`Recipe::Texture`] whether the image is
/// embedded or referenced. Over-approximating costs a rebuild that was not strictly needed;
/// under-approximating ships stale bytes.
pub fn gltf_recipes(doc: &serde_json::Value) -> Vec<Recipe> {
    let mut out: Vec<Recipe> = Vec::new();
    if has_entries(doc, "meshes") {
        out.push(Recipe::Mesh);
    }
    if has_entries(doc, "skins") {
        out.push(Recipe::Skin);
    }
    if has_entries(doc, "animations") {
        out.push(Recipe::Animation);
    }
    if has_entries(doc, "materials") {
        out.push(Recipe::Material);
    }
    if has_entries(doc, "images") {
        out.push(Recipe::Texture);
    }
    let normal_mapped = doc
        .get("materials")
        .and_then(|v| v.as_array())
        .is_some_and(|mats| mats.iter().any(|m| m.get("normalTexture").is_some()));
    if normal_mapped {
        out.push(Recipe::NormalMap);
    }
    out
}

/// The recipes a standalone image bundle's bytes are a function of. `normal` is the
/// normal-map classification the bundle is already named for
/// ([`crate::naming::image_class_digest`]).
pub fn image_recipes(normal: bool) -> Vec<Recipe> {
    let mut out = vec![Recipe::Texture];
    if normal {
        out.push(Recipe::NormalMap);
    }
    out
}

/// Fold `right` into `left`, for building the per-entity union a manifest records.
pub fn merge_into(left: &mut BTreeMap<&'static str, u32>, right: &BTreeMap<&'static str, u32>) {
    for (k, v) in right {
        left.insert(*k, *v);
    }
}

/// Whether the generations a past conversion recorded are still the ones in force.
///
/// `recorded` is a manifest's `recipes` block, or `None` when it has none. A manifest
/// without the block predates the first bump: current exactly while nothing has been bumped
/// since. A manifest with one is current when every generation it names still matches —
/// recipes it does not name are ones its entity does not use, and bumping those cannot have
/// moved its bytes.
///
/// An unreadable block (not an object, or a value that is not a number) is treated as stale
/// rather than guessed at: a needless reconversion is a cost, a skipped one is a bug.
pub fn recorded_is_current(recorded: Option<&serde_json::Value>) -> bool {
    let Some(value) = recorded else {
        return !any_bumped();
    };
    let Some(obj) = value.as_object() else {
        return false;
    };
    obj.iter().all(|(name, gen)| {
        match (Recipe::from_name(name), gen.as_u64()) {
            (Some(recipe), Some(g)) => u64::from(recipe.generation()) == g,
            // A name this build does not know is a generation it cannot vouch for.
            _ => false,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn baseline_generations_fold_in_as_nothing() {
        // A recipe at baseline never reaches a digest, so the assets it covers keep the
        // names they were published under. Stated against the live table so it keeps
        // holding as recipes get bumped, rather than pinning the table to all-zero.
        let bumped: Vec<Recipe> = Recipe::ALL
            .into_iter()
            .filter(|r| r.generation() != BASELINE)
            .collect();
        let folded = generations(&Recipe::ALL);
        assert_eq!(folded.len(), bumped.len());
        for r in bumped {
            assert_eq!(folded.get(r.name()), Some(&r.generation()));
        }
        assert_eq!(any_bumped(), !folded.is_empty());
    }

    #[test]
    fn names_are_unique_and_round_trip() {
        let mut seen: Vec<&str> = Recipe::ALL.iter().map(|r| r.name()).collect();
        let total = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), total);
        for r in Recipe::ALL {
            assert_eq!(Recipe::from_name(r.name()), Some(r));
        }
        assert_eq!(Recipe::from_name("nosuchrecipe"), None);
    }

    #[test]
    fn gltf_recipes_read_the_documents_structure() {
        assert!(gltf_recipes(&json!({})).is_empty());
        assert!(gltf_recipes(&json!({"meshes": [], "skins": []})).is_empty());

        let animated_rig = json!({
            "meshes": [{}],
            "skins": [{}],
            "animations": [{}],
            "materials": [{"pbrMetallicRoughness": {}}],
        });
        assert_eq!(
            gltf_recipes(&animated_rig),
            vec![Recipe::Mesh, Recipe::Skin, Recipe::Animation, Recipe::Material]
        );

        let textured = json!({
            "meshes": [{}],
            "materials": [{"normalTexture": {"index": 0}}],
            "images": [{"uri": "n.png"}],
        });
        assert_eq!(
            gltf_recipes(&textured),
            vec![
                Recipe::Mesh,
                Recipe::Material,
                Recipe::Texture,
                Recipe::NormalMap
            ]
        );
    }

    #[test]
    fn image_recipes_add_the_normal_lane_only_for_normal_maps() {
        assert_eq!(image_recipes(false), vec![Recipe::Texture]);
        assert_eq!(image_recipes(true), vec![Recipe::Texture, Recipe::NormalMap]);
    }

    #[test]
    fn generations_drop_the_baseline_and_sort_by_name() {
        // Table-independent: exercises the shape with generations supplied directly, so the
        // test keeps working whatever the live numbers are.
        let map: BTreeMap<&str, u32> = [("texture", 2u32), ("mesh", 1), ("skin", 0)]
            .into_iter()
            .filter(|(_, g)| *g != BASELINE)
            .collect();
        assert_eq!(
            serde_json::to_string(&map).unwrap(),
            r#"{"mesh":1,"texture":2}"#
        );
    }

    #[test]
    fn merge_keeps_the_union() {
        let mut left: BTreeMap<&'static str, u32> = [("mesh", 1u32)].into_iter().collect();
        let right: BTreeMap<&'static str, u32> = [("texture", 3u32)].into_iter().collect();
        merge_into(&mut left, &right);
        assert_eq!(left.len(), 2);
        assert_eq!(left["mesh"], 1);
        assert_eq!(left["texture"], 3);
    }

    #[test]
    fn a_manifest_without_recipes_is_current_only_while_nothing_is_bumped() {
        assert_eq!(recorded_is_current(None), !any_bumped());
    }

    #[test]
    fn recorded_generations_are_compared_name_by_name() {
        // Every recipe at its own current generation: current, whatever those are.
        let all: serde_json::Value = serde_json::to_value(
            Recipe::ALL
                .iter()
                .map(|r| (r.name(), r.generation()))
                .collect::<BTreeMap<_, _>>(),
        )
        .unwrap();
        assert!(recorded_is_current(Some(&all)));

        // An empty block names nothing, so nothing can disagree with it.
        assert!(recorded_is_current(Some(&json!({}))));

        // One recipe recorded a generation ahead of this build's table.
        let mut ahead = serde_json::Map::new();
        ahead.insert(
            Recipe::Mesh.name().to_string(),
            serde_json::Value::from(u64::from(Recipe::Mesh.generation()) + 1),
        );
        assert!(!recorded_is_current(Some(&serde_json::Value::Object(ahead))));

        // Names this build does not know, and values that are not generations.
        assert!(!recorded_is_current(Some(&json!({"nosuchrecipe": 0}))));
        assert!(!recorded_is_current(Some(&json!({"mesh": "1"}))));
        assert!(!recorded_is_current(Some(&json!([]))));
    }
}

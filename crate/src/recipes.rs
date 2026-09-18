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
///
/// There are only two kinds of bundle in the digest-named lane, so there are only two base
/// recipes: [`Recipe::Glb`] and [`Recipe::Texture`]. A bundle is rebuilt whole or not at all
/// — a GLB bundle carries its meshes, skeleton, clips, materials *and* its resolved textures
/// in one artifact — so a counter per sub-asset would be false precision: bumping a `mesh`
/// counter would rebuild every GLB that has meshes, which is all of them.
///
/// [`Recipe::Skin`] and [`Recipe::Animation`] earn their place by being *rare*: only a
/// minority of glTFs declare a skin or carry clips, so a fix gated on one of those rebuilds
/// a small slice instead of the whole GLB lane. Reach for one only when the fix has a gate
/// you can point at — #119 is the model, where a glTF with no clips serialized byte for byte
/// as before. Otherwise bump [`Recipe::Glb`]: too broad costs a rebuild you were going to
/// pay anyway, too narrow ships stale bundles and says nothing.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum Recipe {
    /// Anything that changes what a GLB's bundle serializes to: vertex data and layout,
    /// Draco, the node graph, materials and shader authoring, renderer bounds. The default
    /// for a fix in the glTF lane, and the honest one — nearly every glTF has meshes and
    /// materials, so a counter that split them would rebuild the same set.
    Glb,
    /// Texture decode and GPU encode — the BC7/DXT/BC5/crunch lanes, mip generation, resize,
    /// colour space, normal-map packing. Covers standalone image bundles *and* every glTF
    /// carrying images, because a GLB bundle embeds the textures it resolves
    /// (`builder::build_bundle` pulls them in through `opts.resolve`), so a texture fix has
    /// to reach both.
    Texture,
    /// Skinning: bind poses, bone weights, the skeleton, and the bounds a
    /// `SkinnedMeshRenderer` ships. Applies to a glTF that declares a skin.
    Skin,
    /// Animation clips: curve sampling, interpolation, and the Mecanim tracks built from
    /// them. Applies to a glTF that carries clips.
    Animation,
}

impl Recipe {
    pub const ALL: [Recipe; 4] = [
        Recipe::Glb,
        Recipe::Texture,
        Recipe::Skin,
        Recipe::Animation,
    ];

    /// Stable key this recipe is recorded under, in digests and in manifests. Renaming one
    /// invalidates every bundle that folded it in, so these are as fixed as the numbers.
    pub const fn name(self) -> &'static str {
        match self {
            Recipe::Glb => "glb",
            Recipe::Texture => "texture",
            Recipe::Skin => "skin",
            Recipe::Animation => "animation",
        }
    }

    /// The current generation. Bump by one, in the commit that changes the output.
    ///
    /// History, newest first — a bump is only legible next to the change it names:
    /// - `Animation` 1: skinned renderer bounds are baked over every animation clip
    ///   instead of the bind pose (#119). The fix is gated on a glTF that declares a skin
    ///   *and* carries clips, so either trait's recipe is a safe superset of it;
    ///   `Animation` carries it because a rig with no clips is exactly the case the fix
    ///   leaves serializing byte for byte as before.
    pub const fn generation(self) -> u32 {
        match self {
            Recipe::Glb => 0,
            Recipe::Texture => 0,
            Recipe::Skin => 0,
            Recipe::Animation => 1,
        }
    }

    pub fn from_name(name: &str) -> Option<Recipe> {
        Recipe::ALL.into_iter().find(|r| r.name() == name)
    }
}

/// The generations a *digest* folds in: the applicable recipes, dropping everything still at
/// [`BASELINE`].
///
/// Dropping the baselines is what keeps names stable — an asset no bump has touched hashes
/// the payload it always hashed and keeps the name it is already published under. The
/// manifest deliberately does the opposite; see [`recorded_generations`].
///
/// A [`BTreeMap`] so the JSON it serializes to is key-ordered and a digest over it is
/// stable, and so an empty result serializes to `{}` — the shape a caller folds in as
/// "nothing to say".
pub fn digest_generations(recipes: &[Recipe]) -> BTreeMap<&'static str, u32> {
    recipes
        .iter()
        .filter(|r| r.generation() != BASELINE)
        .map(|r| (r.name(), r.generation()))
        .collect()
}

/// The generations a *manifest* records: every applicable recipe, **including those at
/// [`BASELINE`]**.
///
/// The baselines are the whole point here, and this is the one place they must not be
/// dropped. A manifest is read back by [`recorded_is_current`], which asks whether every
/// generation the manifest names still matches. A recipe the manifest does not name is
/// treated as one the entity does not use — so if a recipe sitting at `0` were omitted, the
/// bump that takes it to `1` would have nothing to disagree with and the entity would be
/// skipped with stale bundles.
///
/// Recording `{"glb":0,"texture":0}` says something a digest never needs to: *these are the
/// recipes that govern my bytes, and here is where each stood when I was written.* That is
/// what makes every bump after the first precise instead of merely safe.
pub fn recorded_generations(recipes: &[Recipe]) -> BTreeMap<&'static str, u32> {
    recipes.iter().map(|r| (r.name(), r.generation())).collect()
}

/// Whether any recipe at all has been bumped off [`BASELINE`].
///
/// Only the conversion gate's reading of a *missing* block depends on this: a manifest with
/// no block at all predates recipes entirely, and is current exactly while nothing has been
/// bumped since.
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
/// [`Recipe::Glb`] always applies — the bundle is a GLB. The other two are the narrowing
/// ones, and they are read coarsely on purpose: a `skins` array earns [`Recipe::Skin`]
/// whether or not any clip poses it, an `images` array earns [`Recipe::Texture`] whether the
/// image is embedded or referenced. Over-approximating costs a rebuild that was not strictly
/// needed; under-approximating ships stale bytes.
///
/// A fix gated on two traits at once (#119 needed skins *and* clips) may bump either of
/// them: each covers a superset of the assets the fix touched, so both are safe.
pub fn gltf_recipes(doc: &serde_json::Value) -> Vec<Recipe> {
    let mut out: Vec<Recipe> = vec![Recipe::Glb];
    if has_entries(doc, "images") {
        out.push(Recipe::Texture);
    }
    if has_entries(doc, "skins") {
        out.push(Recipe::Skin);
    }
    if has_entries(doc, "animations") {
        out.push(Recipe::Animation);
    }
    out
}

/// The recipes a standalone image bundle's bytes are a function of.
///
/// Just the one: normal maps are encoded by the same lane, and "is this encoder fix
/// normal-only?" is exactly the ambiguous question a recipe must not ask.
pub fn image_recipes() -> Vec<Recipe> {
    vec![Recipe::Texture]
}

/// Fold `recipes` into the per-entity set a manifest records.
pub fn merge_into(set: &mut std::collections::BTreeSet<Recipe>, recipes: &[Recipe]) {
    set.extend(recipes.iter().copied());
}

/// Whether the generations a past conversion recorded are still the ones in force.
///
/// `recorded` is a manifest's `recipes` block, or `None` when it has none. A manifest
/// without the block predates recipes entirely: current exactly while nothing has been
/// bumped since. A manifest with one is current when every generation it names still matches
/// — recipes it does not name are ones its entity does not use, and bumping those cannot
/// have moved its bytes.
///
/// This only holds because [`recorded_generations`] writes the baselines too. A block that
/// named only the bumped recipes would read as current after any later `0 -> 1` bump, and
/// the entity would be skipped holding stale bundles.
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
    fn a_digest_drops_the_baselines_so_published_names_hold() {
        // A recipe at baseline never reaches a digest, so the assets it covers keep the
        // names they were published under. Stated against the live table so it keeps holding
        // as recipes get bumped, rather than pinning the table to all-zero.
        let bumped: Vec<Recipe> = Recipe::ALL
            .into_iter()
            .filter(|r| r.generation() != BASELINE)
            .collect();
        let folded = digest_generations(&Recipe::ALL);
        assert_eq!(folded.len(), bumped.len());
        for r in bumped {
            assert_eq!(folded.get(r.name()), Some(&r.generation()));
        }
        assert_eq!(any_bumped(), !folded.is_empty());
    }

    #[test]
    fn a_manifest_keeps_the_baselines_so_a_later_bump_is_detectable() {
        // The bug this guards: record only the bumped recipes and a later 0 -> 1 bump has
        // nothing in the block to disagree with, so the entity is skipped holding stale
        // bundles. Every applicable recipe is named, whatever generation it stands at.
        let recorded = recorded_generations(&Recipe::ALL);
        assert_eq!(recorded.len(), Recipe::ALL.len());
        for r in Recipe::ALL {
            assert_eq!(recorded.get(r.name()), Some(&r.generation()));
        }
        assert!(recorded_is_current(Some(
            &serde_json::to_value(&recorded).unwrap()
        )));

        // And the detection itself: whatever generation a recipe stands at now, the block
        // written before a bump of it stops reading as current.
        for r in Recipe::ALL {
            let mut stale = serde_json::Map::new();
            stale.insert(
                r.name().to_string(),
                serde_json::Value::from(u64::from(r.generation()) + 1),
            );
            assert!(
                !recorded_is_current(Some(&serde_json::Value::Object(stale))),
                "a bumped {} must invalidate",
                r.name()
            );
        }
    }

    #[test]
    fn an_entity_is_only_governed_by_the_recipes_it_records() {
        // A scene of static props records glb+texture and nothing else, so the live
        // `animation` bump leaves it current — that is the saving — while a `glb` bump
        // would not.
        let props = recorded_generations(&[Recipe::Glb, Recipe::Texture]);
        assert_eq!(props.len(), 2);
        assert!(!props.contains_key(Recipe::Skin.name()));
        assert!(recorded_is_current(Some(
            &serde_json::to_value(&props).unwrap()
        )));
    }

    #[test]
    fn gltf_recipes_read_the_documents_structure() {
        // Glb always applies: the bundle is a GLB whatever the document holds.
        assert_eq!(gltf_recipes(&json!({})), vec![Recipe::Glb]);
        assert_eq!(
            gltf_recipes(&json!({"meshes": [{}], "materials": [{}], "skins": []})),
            vec![Recipe::Glb]
        );

        let animated_rig = json!({
            "meshes": [{}], "skins": [{}], "animations": [{}],
            "materials": [{"pbrMetallicRoughness": {}}],
        });
        assert_eq!(
            gltf_recipes(&animated_rig),
            vec![Recipe::Glb, Recipe::Skin, Recipe::Animation]
        );

        // A GLB embeds the textures it resolves, so a texture fix has to reach it too.
        let textured = json!({"meshes": [{}], "images": [{"uri": "n.png"}]});
        assert_eq!(gltf_recipes(&textured), vec![Recipe::Glb, Recipe::Texture]);

        // Clips without a skeleton (transform animation) are the Animation lane alone.
        assert_eq!(
            gltf_recipes(&json!({"animations": [{}]})),
            vec![Recipe::Glb, Recipe::Animation]
        );
    }

    #[test]
    fn a_standalone_image_is_governed_by_the_texture_recipe_alone() {
        assert_eq!(image_recipes(), vec![Recipe::Texture]);
    }

    #[test]
    fn generations_drop_the_baseline_and_sort_by_name() {
        // Table-independent: exercises the shape with generations supplied directly, so the
        // test keeps working whatever the live numbers are.
        let map: BTreeMap<&str, u32> = [("texture", 2u32), ("glb", 1), ("skin", 0)]
            .into_iter()
            .filter(|(_, g)| *g != BASELINE)
            .collect();
        assert_eq!(
            serde_json::to_string(&map).unwrap(),
            r#"{"glb":1,"texture":2}"#
        );
        // Key order is the map's, so a recorded block is stable across writers too.
        let all: BTreeMap<&str, u32> = [("texture", 0u32), ("glb", 0), ("skin", 1)]
            .into_iter()
            .collect();
        assert_eq!(
            serde_json::to_string(&all).unwrap(),
            r#"{"glb":0,"skin":1,"texture":0}"#
        );
    }

    #[test]
    fn merge_keeps_the_union() {
        let mut set = std::collections::BTreeSet::new();
        merge_into(&mut set, &[Recipe::Glb, Recipe::Texture]);
        merge_into(&mut set, &[Recipe::Glb, Recipe::Skin]);
        assert_eq!(
            set.into_iter().collect::<Vec<_>>(),
            vec![Recipe::Glb, Recipe::Texture, Recipe::Skin]
        );
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
            Recipe::Glb.name().to_string(),
            serde_json::Value::from(u64::from(Recipe::Glb.generation()) + 1),
        );
        assert!(!recorded_is_current(Some(&serde_json::Value::Object(
            ahead
        ))));

        // Names this build does not know, and values that are not generations.
        assert!(!recorded_is_current(Some(&json!({"nosuchrecipe": 0}))));
        assert!(!recorded_is_current(Some(&json!({"glb": "1"}))));
        assert!(!recorded_is_current(Some(&json!([]))));
    }
}

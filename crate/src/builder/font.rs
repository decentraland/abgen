//! Font bundles: one scene `.ttf` in, one bundle holding the explorer's two font assets
//! for it out.
//!
//! The bundle carries a `Font` holding the rebuilt font (`fontgen::rebuild`, never the uploaded
//! file; FreeType keeps adding glyphs from it at runtime), a
//! dynamic `TMP_FontAsset` for `TextShape`, and a dynamic UI Toolkit `FontAsset` for scene UI,
//! each with its own [`fontgen`]-baked atlas. Type trees, the `TMP_FontAsset` `MonoScript` and
//! the built-in shader and script references come from [`FONT_TYPES_TEMPLATE`], a bundle Unity
//! built itself; the container is the same format-22 file every other bundle is written into.
//!
//! The TMP asset ships without a material: its shader lives in the explorer build, not in a
//! built-in resource a bundle can point at, and the explorer assigns its own `TMP_SDF-Mobile`
//! material to every scene font anyway. The UI Toolkit material's shader is built in, so that one
//! is kept.
//!
//! The explorer finds the assets by name, so [`FONT_ASSET_NAME`], [`TMP_ASSET_NAME`] and
//! [`UITK_ASSET_NAME`] are part of the client contract.

use super::finalize::{commit_objects, ExternalsPolicy};
use super::templates::{read_template_bundle, ALL_TYPES_TEMPLATE, FONT_TYPES_TEMPLATE};
use super::*;
use crate::fontgen::{self, BakedFont, GlyphRect};
use crate::unity::serialized_file::FileIdentifier;

pub const FONT_ASSET_NAME: &str = "font";
pub const TMP_ASSET_NAME: &str = "tmp";
pub const UITK_ASSET_NAME: &str = "uitk";

const FONT_LOCAL_ID: i64 = 12800000;
const FONT_TEXTURE_LOCAL_ID: i64 = 2800000;
const FONT_MATERIAL_LOCAL_ID: i64 = 2100000;
const TMP_LOCAL_ID: i64 = 11400000;
const TMP_ATLAS_LOCAL_ID: i64 = 2800002;
const UITK_LOCAL_ID: i64 = 11400002;
const UITK_ATLAS_LOCAL_ID: i64 = 2800004;
const UITK_MATERIAL_LOCAL_ID: i64 = 2100002;

/// What the font lane takes from [`FONT_TYPES_TEMPLATE`].
struct FontTypes {
    proto: HashMap<String, SerializedType>,
    font: Value,
    font_texture: Value,
    font_material: Value,
    mono_script: Value,
    mono_script_pid: i64,
    tmp: Value,
    uitk: Value,
    atlas: Value,
    uitk_material: Value,
    /// `Library/unity default resources` and `Resources/unity_builtin_extra`, in the template's
    /// order: the base values' `m_FileID`s index into it.
    externals: Vec<FileIdentifier>,
    script_types: Vec<(i32, i64)>,
}

fn load_font_types() -> Result<FontTypes> {
    let bundle = read_template_bundle(FONT_TYPES_TEMPLATE).map_err(|e| anyhow!("{e}"))?;
    let sf = bundle
        .serialized()
        .ok_or_else(|| anyhow!("{FONT_TYPES_TEMPLATE} has no serialized file"))?;
    let (_, all_proto, _) = load_template()?;

    let mut proto: HashMap<String, SerializedType> = HashMap::new();
    for key in ["AssetBundle", "TextAsset"] {
        let st = all_proto
            .get(key)
            .ok_or_else(|| anyhow!("{ALL_TYPES_TEMPLATE} has no {key} type"))?;
        proto.insert(key.to_string(), st.clone());
    }

    let mut named: HashMap<String, Value> = HashMap::new();
    let mut mono_script_pid = None;
    for obj in &sf.objects {
        let tree = sf.read_typetree(obj)?;
        let name = tree
            .get("m_Name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let type_key = match (obj.class_id, name.as_str()) {
            (128, _) => "Font",
            (115, _) => {
                mono_script_pid = Some(obj.path_id);
                "MonoScript"
            }
            (114, "tmp") => "TMP_FontAsset",
            (114, "uitk") => "TextCoreFontAsset",
            (28, _) => "Texture2D",
            (21, _) => "Material",
            _ => continue,
        };
        proto
            .entry(type_key.to_string())
            .or_insert_with(|| sf.types[obj.type_id as usize].clone());
        named.insert(name, tree);
    }
    let mut take = |name: &str| {
        named
            .remove(name)
            .ok_or_else(|| anyhow!("{FONT_TYPES_TEMPLATE} has no object named {name:?}"))
    };
    Ok(FontTypes {
        font: take("font")?,
        font_texture: take("Font Texture")?,
        font_material: take("Font Material")?,
        mono_script: take("TMP_FontAsset")?,
        tmp: take("tmp")?,
        uitk: take("uitk")?,
        atlas: take("tmp atlas")?,
        uitk_material: take("uitk material")?,
        mono_script_pid: mono_script_pid
            .ok_or_else(|| anyhow!("{FONT_TYPES_TEMPLATE} has no MonoScript"))?,
        proto,
        externals: sf.externals.clone(),
        script_types: sf.script_types.clone(),
    })
}

fn font_types() -> Result<&'static FontTypes> {
    use std::sync::OnceLock;
    static CACHE: OnceLock<std::result::Result<FontTypes, String>> = OnceLock::new();
    CACHE
        .get_or_init(|| load_font_types().map_err(|e| format!("{e:#}")))
        .as_ref()
        .map_err(|e| anyhow!("{e}"))
}

/// Sets a numeric field in the representation the template already holds there: the type tree
/// writer reads ints and floats back through different accessors.
fn set_num(v: &mut Value, key: &'static str, n: f64) {
    let as_int = matches!(v.get(key), Some(Value::Int(_)));
    if as_int {
        v.insert(key, n.round() as i64);
    } else {
        v.insert(key, n);
    }
}

fn rect_value(r: &GlyphRect) -> Value {
    map! {
        "m_X" => r.x as i64,
        "m_Y" => r.y as i64,
        "m_Width" => r.width as i64,
        "m_Height" => r.height as i64,
    }
}

fn set_main_texture(material: &mut Value, texture_pid: i64) {
    let Some(envs) = material
        .get_mut("m_SavedProperties")
        .and_then(|p| p.get_mut("m_TexEnvs"))
        .and_then(|e| e.as_array_mut())
    else {
        return;
    };
    for env in envs.iter_mut() {
        let Some(pair) = env.as_array_mut() else {
            continue;
        };
        if pair.first().and_then(|k| k.as_str()) == Some("_MainTex") {
            if let Some(slot) = pair.get_mut(1) {
                slot.insert("m_Texture", crate::value::pptr(0, texture_pid));
            }
        }
    }
}

struct Pids {
    font: i64,
    font_texture: i64,
    font_material: i64,
    mono_script: i64,
    tmp: i64,
    tmp_atlas: i64,
    uitk: i64,
    uitk_atlas: i64,
    uitk_material: i64,
    metadata: i64,
}

impl Pids {
    fn new(root_hash: &str, mono_script: i64) -> Self {
        let guid = pathids::asset_guid(root_hash);
        let pid =
            |local| pathids::prefab_packed_path_id(&guid, local, pathids::FILE_TYPE_META_ASSET);
        let meta_guid = pathids::asset_guid(&format!("{root_hash}/metadata"));
        Pids {
            font: pid(FONT_LOCAL_ID),
            font_texture: pid(FONT_TEXTURE_LOCAL_ID),
            font_material: pid(FONT_MATERIAL_LOCAL_ID),
            mono_script,
            tmp: pid(TMP_LOCAL_ID),
            tmp_atlas: pid(TMP_ATLAS_LOCAL_ID),
            uitk: pid(UITK_LOCAL_ID),
            uitk_atlas: pid(UITK_ATLAS_LOCAL_ID),
            uitk_material: pid(UITK_MATERIAL_LOCAL_ID),
            metadata: pathids::prefab_packed_path_id(&meta_guid, 4900000, META_FILE_TYPE),
        }
    }
}

/// One container path: its assets (main asset first) and every object loading it touches.
struct ContainerEntry {
    key: String,
    assets: Vec<i64>,
    preload: Vec<sbp_order::Obj>,
}

/// Unity's layout for a hand-built bundle: entries by path, one preload run per entry holding
/// its external references first, and one container slot per asset sharing that run.
fn preload_and_container(mut entries: Vec<ContainerEntry>) -> (Value, Value) {
    entries.sort_by(|a, b| a.key.cmp(&b.key));
    let mut preload: Vec<sbp_order::Obj> = Vec::new();
    let mut container: Vec<(String, sbp_order::ContainerSlot)> = Vec::new();
    for mut e in entries {
        e.preload
            .sort_by_key(|o| (std::cmp::Reverse(o.file_id), o.path_id));
        e.preload.dedup();
        let start = preload.len();
        let size = e.preload.len();
        preload.extend(e.preload);
        for pid in e.assets {
            container.push((
                e.key.clone(),
                sbp_order::ContainerSlot {
                    preload_index: start,
                    preload_size: size,
                    asset: sbp_order::Obj::new(0, pid),
                },
            ));
        }
    }
    sbp_order::to_values(&preload, &container)
}

pub(super) struct FontBuilder {
    objects: BTreeMap<i64, (String, Value)>,
    types: &'static FontTypes,
}

impl FontBuilder {
    pub(super) fn new(
        bytes: &[u8],
        root_hash: &str,
        toggles: Toggles,
        target: &str,
    ) -> Result<Self> {
        let types = font_types()?;
        let baked = fontgen::bake(bytes)?;
        let pids = Pids::new(root_hash, types.mono_script_pid);
        let mut objects: BTreeMap<i64, (String, Value)> = BTreeMap::new();

        let mut font = types.font.clone();
        font.insert("m_Name", FONT_ASSET_NAME);
        // The rebuilt font, never the uploaded bytes: FreeType parses this at runtime.
        font.insert("m_FontData", Value::Bytes(baked.font_data.clone()));
        let family = if baked.face.family_name.is_empty() {
            FONT_ASSET_NAME.to_string()
        } else {
            baked.face.family_name.clone()
        };
        font.insert("m_FontNames", Value::Array(vec![Value::Str(family)]));
        set_num(&mut font, "m_Ascent", baked.legacy.ascent);
        set_num(&mut font, "m_Descent", baked.legacy.descent);
        set_num(&mut font, "m_LineSpacing", baked.legacy.line_spacing);
        font.insert("m_Texture", crate::value::pptr(0, pids.font_texture));
        font.insert(
            "m_DefaultMaterial",
            crate::value::pptr(0, pids.font_material),
        );
        objects.insert(pids.font, ("Font".into(), font));

        objects.insert(
            pids.font_texture,
            ("Texture2D".into(), types.font_texture.clone()),
        );
        let mut font_material = types.font_material.clone();
        set_main_texture(&mut font_material, pids.font_texture);
        objects.insert(pids.font_material, ("Material".into(), font_material));

        objects.insert(
            pids.mono_script,
            ("MonoScript".into(), types.mono_script.clone()),
        );

        let atlas = |name: &str| {
            let mut t = types.atlas.clone();
            t.insert("m_Name", name);
            t.insert("m_Width", fontgen::ATLAS_SIZE as i64);
            t.insert("m_Height", fontgen::ATLAS_SIZE as i64);
            t.insert("m_CompleteImageSize", baked.atlas.len() as i64);
            t.insert("m_IsReadable", true);
            t.insert("image data", Value::Bytes(baked.atlas.clone()));
            t.insert(
                "m_StreamData",
                map! {"offset" => 0, "size" => 0, "path" => ""},
            );
            t
        };
        objects.insert(pids.tmp_atlas, ("Texture2D".into(), atlas("tmp atlas")));
        objects.insert(pids.uitk_atlas, ("Texture2D".into(), atlas("uitk atlas")));

        let mut uitk_material = types.uitk_material.clone();
        set_main_texture(&mut uitk_material, pids.uitk_atlas);
        objects.insert(pids.uitk_material, ("Material".into(), uitk_material));

        let mut tmp = font_asset(&types.tmp, &baked, TMP_ASSET_NAME, &pids, pids.tmp_atlas);
        tmp.insert("m_Material", crate::value::pptr(0, 0));
        objects.insert(pids.tmp, ("TMP_FontAsset".into(), tmp));

        let mut uitk = font_asset(&types.uitk, &baked, UITK_ASSET_NAME, &pids, pids.uitk_atlas);
        uitk.insert("m_Material", crate::value::pptr(0, pids.uitk_material));
        objects.insert(pids.uitk, ("TextCoreFontAsset".into(), uitk));

        let default_resources = 1;
        let builtin_extra = 2;
        let external = |file_id: i64, of: &Value, key: &str| {
            of.get(key)
                .and_then(|p| p.get("m_PathID"))
                .and_then(|v| v.as_i64())
                .map(|pid| sbp_order::Obj::new(file_id, pid))
        };
        let font_shader = external(default_resources, &types.font_material, "m_Shader");
        let uitk_shader = external(builtin_extra, &types.uitk_material, "m_Shader");
        let uitk_script = external(default_resources, &types.uitk, "m_Script");
        let font_objects =
            [pids.font, pids.font_texture, pids.font_material].map(|p| sbp_order::Obj::new(0, p));

        let lower = root_hash.to_ascii_lowercase();
        let ext = crate::naming::FONT_KEY_EXTENSION;
        let mut entries = vec![
            ContainerEntry {
                key: format!("{lower}{ext}"),
                assets: vec![pids.font, pids.font_material, pids.font_texture],
                preload: font_objects.iter().copied().chain(font_shader).collect(),
            },
            ContainerEntry {
                key: format!("{lower}_{TMP_ASSET_NAME}.asset"),
                assets: vec![pids.tmp, pids.tmp_atlas],
                preload: font_objects
                    .iter()
                    .copied()
                    .chain(font_shader)
                    .chain(
                        [pids.tmp, pids.tmp_atlas, pids.mono_script]
                            .map(|p| sbp_order::Obj::new(0, p)),
                    )
                    .collect(),
            },
            ContainerEntry {
                key: format!("{lower}_{UITK_ASSET_NAME}.asset"),
                assets: vec![pids.uitk, pids.uitk_atlas, pids.uitk_material],
                preload: font_objects
                    .iter()
                    .copied()
                    .chain(font_shader)
                    .chain(uitk_shader)
                    .chain(uitk_script)
                    .chain(
                        [pids.uitk, pids.uitk_atlas, pids.uitk_material]
                            .map(|p| sbp_order::Obj::new(0, p)),
                    )
                    .collect(),
            },
        ];

        if emits_metadata_textasset(root_hash, toggles.v38_compat) {
            let (_, _, all_base) = load_template()?;
            let mut meta = all_base.get("TextAsset").cloned().unwrap_or(Value::Null);
            meta.insert("m_Name", "metadata");
            let version = metadata_version_for_target(target, toggles.v38_compat);
            let ts = metadata_timestamp(toggles);
            meta.insert(
                "m_Script",
                format!(
                    r#"{{"timestamp":{ts},"version":"{version}","dependencies":[],"mainAsset":""}}"#
                ),
            );
            objects.insert(pids.metadata, ("TextAsset".into(), meta));
            entries.push(ContainerEntry {
                key: "metadata.json".into(),
                assets: vec![pids.metadata],
                preload: vec![sbp_order::Obj::new(0, pids.metadata)],
            });
        }

        let (_, _, all_base) = load_template()?;
        let mut ab = all_base.get("AssetBundle").cloned().unwrap_or(Value::Null);
        let (preload, container) = preload_and_container(entries);
        ab.insert("m_PreloadTable", preload);
        ab.insert("m_Container", container);
        ab.insert("m_MainAsset", sbp_order::empty_main_asset());
        ab.insert("m_Dependencies", Value::Array(vec![]));
        objects.insert(1, ("AssetBundle".into(), ab));

        Ok(FontBuilder { objects, types })
    }

    /// Serializes the bundle under `bundle_name`. The objects do not depend on the platform, so
    /// every sibling of a multi-platform build reuses them and only renames the container.
    pub(super) fn write(
        &mut self,
        bundle_name: &str,
        memo: Option<&mut unity::bundle_file::ChunkMemo>,
    ) -> Result<BundleArtifact> {
        let (mut bundle, _, _) = load_template()?;
        let target = target_from_bundle_name(bundle_name);
        if let Some((_, ab)) = self.objects.get_mut(&1) {
            let lower = bundle_name.to_ascii_lowercase();
            ab.insert("m_Name", lower.clone());
            ab.insert("m_AssetBundleName", lower);
        }
        commit_objects(
            &mut bundle,
            bundle_name,
            target,
            &self.types.proto,
            &self.objects,
            &[],
            &HashSet::new(),
            ExternalsPolicy::Clear,
        )?;
        let sf = bundle
            .serialized_mut()
            .ok_or_else(|| anyhow!("bundle has no serialized file"))?;
        sf.externals = self.types.externals.clone();
        sf.script_types = self.types.script_types.clone();
        let data = bundle.save_lz4_memo(memo)?;
        Ok(BundleArtifact {
            data,
            image_uri: Vec::new(),
            bundle,
        })
    }
}

/// The TMP and UI Toolkit font assets share their dynamic-data layout field for field.
fn font_asset(base: &Value, baked: &BakedFont, name: &str, pids: &Pids, atlas_pid: i64) -> Value {
    let mut a = base.clone();
    a.insert("m_Name", name);
    a.insert("m_SourceFontFileGUID", "");
    a.insert("m_SourceFontFile", crate::value::pptr(0, pids.font));
    a.insert("m_AtlasPopulationMode", 1);
    a.insert("m_ClearDynamicDataOnBuild", 0);
    a.insert("m_IsMultiAtlasTexturesEnabled", 1);
    set_num(&mut a, "m_AtlasWidth", fontgen::ATLAS_SIZE as f64);
    set_num(&mut a, "m_AtlasHeight", fontgen::ATLAS_SIZE as f64);
    set_num(&mut a, "m_AtlasPadding", fontgen::ATLAS_PADDING as f64);
    a.insert("m_AtlasRenderMode", fontgen::RENDER_MODE);

    if let Some(face) = a.get_mut("m_FaceInfo") {
        let f = &baked.face;
        face.insert("m_FaceIndex", 0);
        face.insert("m_FamilyName", f.family_name.clone());
        face.insert("m_StyleName", f.style_name.clone());
        set_num(face, "m_PointSize", fontgen::SAMPLING_POINT_SIZE as f64);
        set_num(face, "m_Scale", 1.0);
        set_num(face, "m_UnitsPerEM", f.units_per_em as f64);
        set_num(face, "m_LineHeight", f.line_height);
        set_num(face, "m_AscentLine", f.ascent_line);
        set_num(face, "m_CapLine", f.cap_line);
        set_num(face, "m_MeanLine", f.mean_line);
        set_num(face, "m_Baseline", 0.0);
        set_num(face, "m_DescentLine", f.descent_line);
        set_num(face, "m_SuperscriptOffset", f.ascent_line);
        set_num(face, "m_SubscriptOffset", f.descent_line);
        set_num(face, "m_UnderlineOffset", f.underline_offset);
        set_num(face, "m_UnderlineThickness", f.underline_thickness);
        set_num(face, "m_StrikethroughOffset", f.strikethrough_offset);
        set_num(face, "m_StrikethroughThickness", f.underline_thickness);
        set_num(face, "m_TabWidth", f.tab_width);
    }

    let glyphs = baked
        .glyphs
        .iter()
        .map(|g| {
            map! {
                "m_Index" => g.index as i64,
                "m_Metrics" => map! {
                    "m_Width" => g.width,
                    "m_Height" => g.height,
                    "m_HorizontalBearingX" => g.bearing_x,
                    "m_HorizontalBearingY" => g.bearing_y,
                    "m_HorizontalAdvance" => g.advance,
                },
                "m_GlyphRect" => rect_value(&g.rect),
                "m_Scale" => 1.0,
                "m_AtlasIndex" => 0,
                "m_ClassDefinitionType" => 0,
            }
        })
        .collect();
    a.insert("m_GlyphTable", Value::Array(glyphs));
    let characters = baked
        .characters
        .iter()
        .map(|c| {
            map! {
                "m_ElementType" => 1,
                "m_Unicode" => c.unicode as i64,
                "m_GlyphIndex" => c.glyph_index as i64,
                "m_Scale" => 1.0,
            }
        })
        .collect();
    a.insert("m_CharacterTable", Value::Array(characters));
    a.insert(
        "m_AtlasTextures",
        Value::Array(vec![crate::value::pptr(0, atlas_pid)]),
    );
    a.insert("m_AtlasTextureIndex", 0);
    a.insert(
        "m_UsedGlyphRects",
        Value::Array(baked.used_rects.iter().map(rect_value).collect()),
    );
    a.insert(
        "m_FreeGlyphRects",
        Value::Array(baked.free_rects.iter().map(rect_value).collect()),
    );
    if let Some(features) = a.get_mut("m_FontFeatureTable") {
        features.insert("m_GlyphPairAdjustmentRecords", pair_records(baked));
    }
    a
}

/// The kerning between pre-filled glyphs as TextCore stores what it reads from GPOS: the
/// first glyph's advance scaled to the sampling point size, every other value as read.
fn pair_records(baked: &BakedFont) -> Value {
    let em_scale = fontgen::SAMPLING_POINT_SIZE as f64 / baked.face.units_per_em as f64;
    let value = |x_advance: f64| {
        map! {
            "m_XPlacement" => 0.0,
            "m_YPlacement" => 0.0,
            "m_XAdvance" => x_advance,
            "m_YAdvance" => 0.0,
        }
    };
    let records = baked
        .kerning
        .iter()
        .map(|p| {
            map! {
                "m_FirstAdjustmentRecord" => map! {
                    "m_GlyphIndex" => p.first as i64,
                    "m_GlyphValueRecord" => value(p.x_advance as f64 * em_scale),
                },
                "m_SecondAdjustmentRecord" => map! {
                    "m_GlyphIndex" => p.second as i64,
                    "m_GlyphValueRecord" => value(0.0),
                },
                "m_FeatureLookupFlags" => 0,
            }
        })
        .collect();
    Value::Array(records)
}

/// Builds one font bundle per name in `bundle_names`, baking the font once.
pub(super) fn build_font_bundles(
    bytes: &[u8],
    bundle_names: &[String],
    root_hash: &str,
    opts: &BuildOpts<'_>,
) -> Result<Vec<BundleArtifact>> {
    let first = bundle_names
        .first()
        .ok_or_else(|| anyhow!("no bundle name to build the font into"))?;
    let mut b = FontBuilder::new(
        bytes,
        root_hash,
        Toggles::from_opts(opts),
        target_from_bundle_name(first),
    )?;
    let mut memo = unity::bundle_file::ChunkMemo::default();
    bundle_names
        .iter()
        .map(|name| b.write(name, Some(&mut memo)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unity::bundle_file::Bundle as ReadBundle;

    /// Azeret Mono Medium (OFL), the font the template was built from.
    fn template_font() -> Vec<u8> {
        font_types()
            .unwrap()
            .font
            .get("m_FontData")
            .and_then(|v| v.as_bytes())
            .expect("template font carries its data")
            .to_vec()
    }

    #[test]
    fn baked_metrics_match_what_unity_bakes_for_the_same_font() {
        let baked = fontgen::bake(&template_font()).unwrap();
        let f = &baked.face;
        assert_eq!(
            (f.family_name.as_str(), f.style_name.as_str()),
            ("Azeret Mono", "Medium")
        );
        assert!((f.line_height - 105.03).abs() < 1e-9);
        assert!((f.ascent_line - 84.33).abs() < 1e-9);
        assert!((f.descent_line + 20.7).abs() < 1e-9);
        assert_eq!((f.cap_line, f.mean_line, f.tab_width), (63.0, 50.0, 59.0));
        assert_eq!((f.underline_offset, f.underline_thickness), (-11.25, 4.5));
        assert_eq!(f.strikethrough_offset, 20.0);

        let bang = baked
            .characters
            .iter()
            .find(|c| c.unicode == '!' as u32)
            .unwrap();
        let g = baked
            .glyphs
            .iter()
            .find(|g| g.index == bang.glyph_index)
            .unwrap();
        assert_eq!(
            (g.width, g.height, g.bearing_x, g.bearing_y, g.advance),
            (12.875, 62.8125, 22.765625, 62.8125, 58.5)
        );
        assert_eq!((g.rect.width, g.rect.height), (14, 63));
    }

    #[test]
    fn a_font_bundle_carries_both_font_assets_wired_to_one_font() {
        let names = vec![
            "bafkreifonttest_windows".to_string(),
            "bafkreifonttest_mac".to_string(),
        ];
        let opts = BuildOpts {
            source_file: Some("fonts/Azeret.ttf"),
            ..BuildOpts::default()
        };
        let out = build_font_bundles(&template_font(), &names, "bafkreifonttest", &opts).unwrap();
        assert_eq!(out.len(), 2);
        for (artifact, name) in out.iter().zip(&names) {
            let bundle = ReadBundle::load_bytes(&artifact.data).unwrap();
            let sf = bundle.serialized().unwrap();
            assert_eq!(sf.externals.len(), 2);
            assert_eq!(sf.script_types.len(), 2);

            let mut by_name: HashMap<String, (i32, i64, Value)> = HashMap::new();
            for o in &sf.objects {
                let v = sf.read_typetree(o).unwrap();
                let n = v
                    .get("m_Name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                by_name.insert(n, (o.class_id, o.path_id, v));
            }
            let pid = |n: &str| by_name[n].1;
            let pptr = |v: &Value, k: &str| {
                v.get(k)
                    .and_then(|p| p.get("m_PathID"))
                    .and_then(|p| p.as_i64())
            };

            assert_eq!(by_name[FONT_ASSET_NAME].0, 128);
            for (asset, atlas) in [
                (TMP_ASSET_NAME, "tmp atlas"),
                (UITK_ASSET_NAME, "uitk atlas"),
            ] {
                let (class, _, v) = &by_name[asset];
                assert_eq!(*class, 114);
                assert_eq!(pptr(v, "m_SourceFontFile"), Some(pid(FONT_ASSET_NAME)));
                let atlases = v.get("m_AtlasTextures").and_then(|a| a.as_array()).unwrap();
                assert_eq!(atlases.len(), 1);
                assert_eq!(
                    atlases[0].get("m_PathID").and_then(|p| p.as_i64()),
                    Some(pid(atlas))
                );
                let glyphs = v.get("m_GlyphTable").and_then(|a| a.as_array()).unwrap();
                assert!(glyphs.len() > 95, "{asset}: {} glyphs", glyphs.len());
                assert_eq!(
                    by_name[atlas].2.get("m_Width").and_then(|w| w.as_i64()),
                    Some(1024)
                );
            }
            assert_eq!(pptr(&by_name[TMP_ASSET_NAME].2, "m_Material"), Some(0));
            assert_eq!(
                pptr(&by_name[UITK_ASSET_NAME].2, "m_Material"),
                Some(pid("uitk material"))
            );

            let ab = &by_name[name.as_str()].2;
            assert_eq!(
                ab.get("m_AssetBundleName").and_then(|n| n.as_str()),
                Some(name.as_str())
            );
            let keys: Vec<&str> = ab
                .get("m_Container")
                .and_then(|c| c.as_array())
                .unwrap()
                .iter()
                .filter_map(|kv| {
                    kv.as_array()
                        .and_then(|p| p.first())
                        .and_then(|k| k.as_str())
                })
                .collect();
            assert!(keys.contains(&"bafkreifonttest.ttf"));
            assert!(keys.contains(&"bafkreifonttest_tmp.asset"));
        }
    }

    /// Every point `ttf-parser` emits for a glyph, in order.
    #[derive(Default, PartialEq, Debug)]
    struct Points(Vec<(char, f32, f32)>);

    impl ttf_parser::OutlineBuilder for Points {
        fn move_to(&mut self, x: f32, y: f32) {
            self.0.push(('M', x, y));
        }
        fn line_to(&mut self, x: f32, y: f32) {
            self.0.push(('L', x, y));
        }
        fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
            self.0.push(('q', x1, y1));
            self.0.push(('Q', x, y));
        }
        fn curve_to(&mut self, _: f32, _: f32, _: f32, _: f32, x: f32, y: f32) {
            self.0.push(('C', x, y));
        }
        fn close(&mut self) {
            self.0.push(('Z', 0.0, 0.0));
        }
    }

    #[test]
    fn the_rebuilt_font_draws_and_measures_like_the_original() {
        use ttf_parser::{Face, GlyphId};
        let original_bytes = template_font();
        let original = Face::parse(&original_bytes, 0).unwrap();
        let (rebuilt_bytes, _) = fontgen::rebuild::rebuild(&original, &[]).unwrap();
        let rebuilt = Face::parse(&rebuilt_bytes, 0).unwrap();

        assert_eq!(rebuilt.number_of_glyphs(), original.number_of_glyphs());
        assert_eq!(rebuilt.units_per_em(), original.units_per_em());
        assert_eq!(
            (rebuilt.ascender(), rebuilt.descender(), rebuilt.line_gap()),
            (
                original.ascender(),
                original.descender(),
                original.line_gap()
            )
        );
        assert_eq!(rebuilt.underline_metrics(), original.underline_metrics());
        assert_eq!(rebuilt.capital_height(), original.capital_height());
        assert_eq!(rebuilt.x_height(), original.x_height());
        for c in fontgen::priority_characters() {
            assert_eq!(rebuilt.glyph_index(c), original.glyph_index(c), "{c:?}");
        }
        for gid in 0..original.number_of_glyphs() {
            let gid = GlyphId(gid);
            assert_eq!(
                rebuilt.glyph_hor_advance(gid),
                original.glyph_hor_advance(gid)
            );
            let (mut a, mut b) = (Points::default(), Points::default());
            original.outline_glyph(gid, &mut a);
            rebuilt.outline_glyph(gid, &mut b);
            // The same path; scaled composites land on whole units, as TrueType stores them.
            let ops = |p: &Points| p.0.iter().map(|q| q.0).collect::<Vec<_>>();
            assert_eq!(ops(&b), ops(&a), "glyph {}", gid.0);
            for (o, r) in a.0.iter().zip(&b.0) {
                assert!(
                    (o.1 - r.1).abs() <= 0.5 && (o.2 - r.2).abs() <= 0.5,
                    "glyph {}: {o:?} became {r:?}",
                    gid.0
                );
            }
        }
        // Nothing the rebuild does not write survives it.
        for tag in [
            b"fpgm", b"prep", b"cvt ", b"gasp", b"GSUB", b"GDEF", b"DSIG",
        ] {
            let tag = ttf_parser::Tag::from_bytes(tag);
            assert!(rebuilt.raw_face().table(tag).is_none(), "{tag:?}");
        }
    }

    #[test]
    fn kerning_survives_the_rebuild() {
        use fontgen::kerning::{self, KernPair};
        use ttf_parser::Face;
        let original_bytes = template_font();
        let original = Face::parse(&original_bytes, 0).unwrap();
        let gid = |c| original.glyph_index(c).unwrap();
        let mut pairs = vec![
            KernPair {
                first: gid('A').0,
                second: gid('V').0,
                x_advance: -60,
            },
            KernPair {
                first: gid('T').0,
                second: gid('o').0,
                x_advance: -40,
            },
            KernPair {
                first: gid('V').0,
                second: gid('A').0,
                x_advance: -55,
            },
        ];
        pairs.sort_by_key(|p| (p.first, p.second));
        let (rebuilt_bytes, kept) = fontgen::rebuild::rebuild(&original, &pairs).unwrap();
        assert_eq!(
            kept, pairs,
            "every pair fit, so the assets and the GPOS hold the same"
        );
        let rebuilt = Face::parse(&rebuilt_bytes, 0).unwrap();
        let glyphs = ['A', 'V', 'T', 'o'].map(gid);
        let mut read = kerning::pairs(&rebuilt, &glyphs);
        read.sort_by_key(|p| (p.first, p.second));
        assert_eq!(read, pairs);
    }

    #[test]
    fn a_bundle_embeds_the_rebuilt_font_not_the_upload() {
        let upload = template_font();
        let opts = BuildOpts::default();
        let names = vec!["bafkreifonttest_mac".to_string()];
        let out = build_font_bundles(&upload, &names, "bafkreifonttest", &opts).unwrap();
        let bundle = ReadBundle::load_bytes(&out[0].data).unwrap();
        let sf = bundle.serialized().unwrap();
        let embedded = sf
            .objects
            .iter()
            .find(|o| o.class_id == 128)
            .map(|o| sf.read_typetree(o).unwrap())
            .and_then(|v| {
                v.get("m_FontData")
                    .and_then(|d| d.as_bytes())
                    .map(<[u8]>::to_vec)
            })
            .unwrap();
        assert_ne!(embedded, upload);
        assert!(fontgen::is_supported(&embedded));
    }

    #[test]
    fn the_font_template_round_trips_through_the_format_23_writer() {
        use crate::unity::serialized_file::SerializedFile;
        let bundle = read_template_bundle(FONT_TYPES_TEMPLATE).unwrap();
        let sf = bundle.serialized().unwrap();
        assert_eq!(sf.version, 23);
        let again = SerializedFile::parse(&sf.save()).unwrap();
        assert_eq!(again.version, 23);
        assert_eq!(again.unity_version, sf.unity_version);
        assert_eq!(again.types.len(), sf.types.len());
        for (a, b) in again.types.iter().zip(&sf.types) {
            assert_eq!(a.class_id, b.class_id);
            assert_eq!(a.script_id, b.script_id);
            assert_eq!(a.old_type_hash, b.old_type_hash);
            assert_eq!(a.type_tree_hash, b.type_tree_hash);
            assert_eq!(a.type_tree_version, b.type_tree_version);
            assert_eq!(a.node, b.node);
        }
        let by_pid = |sf: &SerializedFile| -> std::collections::BTreeMap<i64, (i32, Vec<u8>)> {
            sf.objects
                .iter()
                .map(|o| (o.path_id, (o.type_id, o.data.clone())))
                .collect()
        };
        assert_eq!(by_pid(&again), by_pid(sf));
        assert_eq!(again.script_types, sf.script_types);
        assert_eq!(again.externals.len(), sf.externals.len());
    }

    #[test]
    fn a_refusal_reaches_the_caller_typed_through_build_bundle() {
        use crate::fontgen::rebuild::tests::{cmap_format12, composite_glyph, fixture};
        // A composite cycle: refused by the rebuild, and the error keeps its type through the
        // builder so the conversion can tolerate it.
        let bomb = fixture(
            &[composite_glyph(&[1]), composite_glyph(&[0])],
            cmap_format12(&[(0x41, 0x41, 0)]),
            Vec::new(),
        );
        let opts = BuildOpts {
            source_file: Some("fonts/bomb.ttf"),
            ..BuildOpts::default()
        };
        let err = build_bundle(&bomb, "bafkreibomb_mac", "bafkreibomb", &opts).unwrap_err();
        assert!(err.downcast_ref::<fontgen::Refused>().is_some(), "{err:#}");

        // Not a font at all under a .ttf path: refused, never a texture bundle.
        let err =
            build_bundle(b"wOF2 not a font", "bafkreiwoff_mac", "bafkreiwoff", &opts).unwrap_err();
        assert!(err.downcast_ref::<fontgen::Refused>().is_some(), "{err:#}");
    }

    #[test]
    fn only_sfnt_fonts_take_the_font_lane() {
        assert!(fontgen::is_supported(&template_font()));
        assert!(!fontgen::is_supported(b"wOF2 not an sfnt"));
        assert!(!fontgen::is_supported(&[0, 1, 0, 0, 0, 0]));
    }
}

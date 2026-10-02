using System.IO;
using TMPro;
using UnityEditor;
using UnityEngine;
using UnityEngine.TextCore.LowLevel;
using TextCoreFontAsset = UnityEngine.TextCore.Text.FontAsset;

// Builds template/font-types.mac.bundle: one Font, one dynamic TMP_FontAsset and one UI Toolkit
// FontAsset, no glyphs, and no TMP material (its shader would be copied into the bundle). abgen
// reads only their type trees, the MonoScript and the built-in references from it.
//
// Regenerate with the explorer's Unity version and ugui package, from a project holding this file
// under Assets/Editor, TextMesh Pro's essential resources, and any OFL font at FONT_SRC:
//   DONOR_OUT=<dir> FONT_SRC=<font.ttf> Unity -batchmode -quit -nographics -projectPath <project> \
//     -executeMethod FontTypesTemplate.Build
// then copy <dir>/slim/font-types over template/font-types.mac.bundle and update its sha256 in
// scripts/bootstrap-runtime.sh. The committed copy was built by 6000.5.9f1 with Azeret Mono.
public static class FontTypesTemplate
{
    private const string DIR = "Assets/Slim";

    private static void NoClear(Object o)
    {
        var so = new SerializedObject(o);
        so.FindProperty("m_ClearDynamicDataOnBuild").boolValue = false;
        so.ApplyModifiedPropertiesWithoutUndo();
    }

    public static void Build()
    {
        string outDir = System.Environment.GetEnvironmentVariable("DONOR_OUT");

        // CreateFontAsset reads TMP_Settings, which a bare project does not have.
        if (!File.Exists("Assets/Resources/TMP Settings.asset"))
        {
            Directory.CreateDirectory("Assets/Resources");
            AssetDatabase.CreateAsset(ScriptableObject.CreateInstance<TMP_Settings>(), "Assets/Resources/TMP Settings.asset");
            AssetDatabase.SaveAssets();
        }

        Directory.CreateDirectory(DIR);
        string ttfPath = DIR + "/font.ttf";
        File.Copy(System.Environment.GetEnvironmentVariable("FONT_SRC"), ttfPath, true);
        AssetDatabase.ImportAsset(ttfPath);
        var importer = (TrueTypeFontImporter)AssetImporter.GetAtPath(ttfPath);
        importer.includeFontData = true;
        importer.fontTextureCase = FontTextureCase.Dynamic;
        importer.SaveAndReimport();
        Font font = AssetDatabase.LoadAssetAtPath<Font>(ttfPath);

        TMP_FontAsset tmp = TMP_FontAsset.CreateFontAsset(font, 90, 9, GlyphRenderMode.SDFAA, 1024, 1024, AtlasPopulationMode.Dynamic, true);
        tmp.name = "tmp";
        Material tmpMaterial = tmp.material;
        tmp.material = null;
        Object.DestroyImmediate(tmpMaterial);
        AssetDatabase.CreateAsset(tmp, DIR + "/tmp.asset");
        tmp.atlasTexture.name = "tmp atlas";
        AssetDatabase.AddObjectToAsset(tmp.atlasTexture, tmp);
        NoClear(tmp);

        TextCoreFontAsset uitk = TextCoreFontAsset.CreateFontAsset(font, 90, 9, GlyphRenderMode.SDFAA, 1024, 1024, UnityEngine.TextCore.Text.AtlasPopulationMode.Dynamic, true);
        uitk.name = "uitk";
        AssetDatabase.CreateAsset(uitk, DIR + "/uitk.asset");
        uitk.atlasTextures[0].name = "uitk atlas";
        AssetDatabase.AddObjectToAsset(uitk.atlasTextures[0], uitk);
        uitk.material.name = "uitk material";
        AssetDatabase.AddObjectToAsset(uitk.material, uitk);
        NoClear(uitk);
        AssetDatabase.SaveAssets();

        var builds = new[] { new AssetBundleBuild { assetBundleName = "font-types", assetNames = new[] { ttfPath, DIR + "/tmp.asset", DIR + "/uitk.asset" } } };
        string dir = Path.Combine(outDir, "slim");
        Directory.CreateDirectory(dir);
        BuildPipeline.BuildAssetBundles(dir, builds, BuildAssetBundleOptions.ChunkBasedCompression, BuildTarget.StandaloneOSX);
        Debug.Log("SLIM done");
    }
}

// abgen render harness — one-shot project configuration.
// Creates the URP pipeline asset programmatically and switches the project to the exact
// configuration the harness renders were calibrated against (URP active, linear color
// space, MSAA off). Programmatic creation keeps the template portable across Unity 6000.x
// (hand-serialized URP .asset YAML drifts between URP versions; this does not).
//
// Run once after creating/opening the project:
//   Unity -batchmode -quit -projectPath <project> -executeMethod AbProjectSetup.Apply -logFile <log>
//
// What the harness requires and why:
//   - URP as the active render pipeline: the DCL/Scene shader bundle
//     (scene_ignore_<platform>) contains URP-compiled shaders; under the built-in
//     pipeline every material falls back to Hidden/InternalErrorShader (magenta) and
//     inventories report errorShader > 0. Also enables the RenderPipeline.StandardRequest
//     render path in the capture scripts.
//   - Linear color space: the production converter and explorer render linear; gamma
//     renders diverge everywhere and poison pixel metrics.
//   - MSAA 1 / shadows off / flat ambient: determinism — captures must be pixel-stable
//     across runs (scene light shadows are already disabled per-shot by the scripts).
using System;
using System.IO;
using UnityEditor;
using UnityEngine;
using UnityEngine.Rendering;
using UnityEngine.Rendering.Universal;

public static class AbProjectSetup
{
    public static void Apply()
    {
        int rc = 0;
        try { ApplyInner(); }
        catch (Exception e)
        {
            Debug.LogError("ABSETUP FATAL: " + e);
            rc = 2;
        }
        EditorApplication.Exit(rc);
    }

    static void ApplyInner()
    {
        PlayerSettings.colorSpace = ColorSpace.Linear;
        Debug.Log("ABSETUP: colorSpace=Linear");

        Directory.CreateDirectory("Assets/Settings");

        // Mirror URP's own (internal) CreateRendererAsset path: a raw CreateInstance'd
        // UniversalRendererData has null shader/post-process resources; the default
        // PostProcessData asset + ResourceReloader fill them in (verified against URP 17.4).
        var rendererData = ScriptableObject.CreateInstance<UniversalRendererData>();
        rendererData.postProcessData = AssetDatabase.LoadAssetAtPath<PostProcessData>(
            UniversalRenderPipelineAsset.packagePath + "/Runtime/Data/PostProcessData.asset");
        AssetDatabase.CreateAsset(rendererData, "Assets/Settings/AbUrpRenderer.asset");
        ResourceReloader.ReloadAllNullIn(rendererData, UniversalRenderPipelineAsset.packagePath);

        var pipeline = UniversalRenderPipelineAsset.Create(rendererData);
        pipeline.msaaSampleCount = 1; // determinism
        pipeline.supportsHDR = true;
        pipeline.colorGradingMode = ColorGradingMode.HighDynamicRange;
        AssetDatabase.CreateAsset(pipeline, "Assets/Settings/AbUrp.asset");

        GraphicsSettings.defaultRenderPipeline = pipeline;
        // Quality levels with an explicit override would shadow the default — point them
        // all at our asset. QualitySettings has no public per-index setter across 6000.x
        // (SetRenderPipelineAssetAt does not exist there; first real-editor compile of
        // this file caught it) — the portable pattern is: visit each level and write the
        // current-level override via QualitySettings.renderPipeline.
        int prevLevel = QualitySettings.GetQualityLevel();
        for (int i = 0; i < QualitySettings.count; i++)
        {
            QualitySettings.SetQualityLevel(i, false);
            QualitySettings.renderPipeline = pipeline;
        }
        QualitySettings.SetQualityLevel(prevLevel, false);

        AssetDatabase.SaveAssets();

        RenderPipelineAsset active = GraphicsSettings.currentRenderPipeline;
        if (active == null)
            throw new Exception("URP asset assigned but currentRenderPipeline is still null");
        Debug.Log("ABSETUP: pipeline=" + active.name + " OK");
    }
}

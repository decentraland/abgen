// abgen render harness — animation frame-series capture for motion-parity testing.
// Companion to AbVisualCompare.cs (self-contained: either script works alone; drop both
// into Assets/Editor/). Renders N frames spanning each animated bundle's clips so the
// pipeline can assemble side-by-side GIFs and run motion-parity metrics
// (per-frame MAE / motion-energy profiles) instead of judging a single mid-pose.
//
// Run: Unity -batchmode -quit -projectPath <project> -executeMethod AbAnimCapture.Run -logFile <log>
//
// Inputs (env) — shared names with AbVisualCompare where meaning is identical:
//   AB_ROOT       staging root (default /tmp/ab-compat).
//   AB_JOBS       jobs file name/path relative to AB_ROOT (default jobs.txt).
//                 Only kind=animated lines are processed; other kinds are skipped.
//   AB_PLATFORM   mac | windows | linux | webgl (default from the editor OS).
//   AB_SHADER     shader bundle path, relative to AB_ROOT unless absolute
//                 (default shader/scene_ignore_<AB_PLATFORM>).
//   AB_FRAMES     frames per job (default 16).
//   AB_ANIM_SIZE  square render size in px (default 512 — the campaign GIF size).
//   AB_AZIMUTHS   camera azimuth list; only the FIRST entry is used here so the frame
//                 series matches AbVisualCompare's -a0 / -anim view (default 35,155,275).
//
// Outputs to $AB_ROOT/out/:
//   <label>-f00.png … -f<N-1>.png   frame k sampled at t = clip.length * k / N per clip
//                                   (loop-friendly: the wrap frame is not repeated)
//   <label>.anim.json               { label, frames, size, azimuth, clips:[{name,length}],
//                                     boundsRest, boundsUnion }
//   <label>.ANIMFAILED.txt          exception text when the job failed
// Appends to $AB_ROOT/harness-anim.log.
//
// THE CAPTURE PATTERN (deliberate, do not "optimize"):
//   per frame: fresh-instantiate -> sample every clip once at t_k -> render -> destroy.
// AnimationClip.SampleAnimation only writes properties the clip animates at that time;
// re-sampling a live instance leaves stale values from earlier samples (and scripts are
// not running in batch capture, so nothing resets state). A fresh instantiate guarantees
// every frame is exactly rest-pose + clip(t_k).
//   framing: rest bounds ∪ sampled bounds — one fixed camera for the whole series,
// computed in a bounds pre-pass (same fresh-instantiate discipline) over the rest pose
// and every sampled frame, so motion never leaves the frame and framing is identical
// across both sides of a pair.
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Linq;
using System.Text;
using UnityEditor;
using UnityEngine;
using UnityEngine.Rendering;
using Debug = UnityEngine.Debug;

public static class AbAnimCapture
{
    [Serializable]
    class Meta
    {
        public long timestamp = -1;
        public string version = "1.0";
        public List<string> dependencies = new List<string>();
        public string mainAsset = "";
    }

    static StreamWriter s_log;
    static string s_platform;
    static int s_frames, s_size;
    static float s_azimuth;

    static void L(string s)
    {
        Debug.Log("ABANIM: " + s);
        if (s_log != null) s_log.WriteLine(DateTime.UtcNow.ToString("HH:mm:ss ") + s);
    }

    static string Env(string name, string dflt)
    {
        string v = Environment.GetEnvironmentVariable(name);
        return string.IsNullOrEmpty(v) ? dflt : v;
    }

    static string DefaultPlatform()
    {
        switch (Application.platform)
        {
            case RuntimePlatform.OSXEditor: return "mac";
            case RuntimePlatform.WindowsEditor: return "windows";
            default: return "linux";
        }
    }

    public static void Run()
    {
        int rc = 0;
        try { RunInner(); }
        catch (Exception e)
        {
            Debug.LogError("ABANIM FATAL: " + e);
            if (s_log != null) s_log.WriteLine("FATAL: " + e);
            rc = 2;
        }
        finally { if (s_log != null) s_log.Flush(); }
        EditorApplication.Exit(rc);
    }

    static void RunInner()
    {
        string root = Env("AB_ROOT", "/tmp/ab-compat");
        s_platform = Env("AB_PLATFORM", DefaultPlatform());
        s_frames = int.Parse(Env("AB_FRAMES", "16"));
        s_size = int.Parse(Env("AB_ANIM_SIZE", "512"));
        s_azimuth = float.Parse(Env("AB_AZIMUTHS", "35,155,275").Split(',')[0].Trim(),
            System.Globalization.CultureInfo.InvariantCulture);
        string outDir = Path.Combine(root, "out");
        Directory.CreateDirectory(outDir);
        s_log = new StreamWriter(Path.Combine(root, "harness-anim.log"), true) { AutoFlush = true };
        L("=== anim run start, platform=" + s_platform + " frames=" + s_frames + " size=" + s_size +
          " azimuth=" + s_azimuth +
          " pipeline=" + (GraphicsSettings.currentRenderPipeline != null ? GraphicsSettings.currentRenderPipeline.name : "builtin"));

        string shaderPath = Env("AB_SHADER", Path.Combine("shader", "scene_ignore_" + s_platform));
        if (!Path.IsPathRooted(shaderPath)) shaderPath = Path.Combine(root, shaderPath);
        AssetBundle shaderAb = AssetBundle.LoadFromFile(shaderPath);
        if (shaderAb == null) throw new Exception("shader bundle failed to load: " + shaderPath);
        shaderAb.LoadAllAssets();
        L("shader bundle loaded");

        string jobsPath = Env("AB_JOBS", "jobs.txt");
        if (!Path.IsPathRooted(jobsPath)) jobsPath = Path.Combine(root, jobsPath);
        foreach (string line in File.ReadAllLines(jobsPath))
        {
            string t = line.Trim();
            if (t.Length == 0 || t.StartsWith("#")) continue;
            string[] parts = t.Split('|');
            if (parts.Length < 4) continue; // legacy 3-field lines are kind=glb -> not ours
            string label = parts[0], kind = parts[1], bundlePath = parts[2], depsDir = parts[3];
            if (kind != "animated") continue;
            L("JOB " + label + " " + bundlePath);
            var sw = Stopwatch.StartNew();
            try { CaptureJob(label, bundlePath, depsDir, outDir); }
            catch (Exception e)
            {
                L("JOB FAIL " + label + ": " + e.Message);
                File.WriteAllText(Path.Combine(outDir, label + ".ANIMFAILED.txt"), e.ToString());
            }
            L("TIME " + label + " ms=" + sw.ElapsedMilliseconds);
        }
        L("=== anim run end");
    }

    static string J(string s)
    {
        if (s == null) return "";
        var sb = new StringBuilder(s.Length + 8);
        foreach (char c in s)
        {
            if (c == '"' || c == '\\') sb.Append('\\').Append(c);
            else if (c == '\n') sb.Append("\\n");
            else if (c == '\r') sb.Append("\\r");
            else if (c == '\t') sb.Append("\\t");
            else if (c < 0x20) sb.Append("\\u").Append(((int)c).ToString("x4"));
            else sb.Append(c);
        }
        return sb.ToString();
    }

    static string F(float v)
    {
        return v.ToString("R", System.Globalization.CultureInfo.InvariantCulture);
    }

    static void CaptureJob(string label, string bundlePath, string depsDir, string outDir)
    {
        var loaded = new List<AssetBundle>();
        try
        {
            AssetBundle ab = AssetBundle.LoadFromFile(bundlePath);
            if (ab == null) throw new Exception("bundle load failed: " + bundlePath);
            loaded.Add(ab);

            var meta = new Meta();
            bool hasMeta = false;
            TextAsset metaTa = ab.LoadAsset<TextAsset>("metadata.json");
            if (metaTa != null) { JsonUtility.FromJsonOverwrite(metaTa.text, meta); hasMeta = true; }
            if (hasMeta)
            {
                var visited = new HashSet<string>();
                foreach (string d in meta.dependencies) LoadDep(d, depsDir, loaded, visited);
            }
            else
            {
                SameDirScan(bundlePath, depsDir, loaded);
            }

            UnityEngine.Object[] all = ab.LoadAllAssets();

            // prefab set: mainAsset when named, else every GameObject asset
            UnityEngine.Object[] assets;
            if (!string.IsNullOrEmpty(meta.mainAsset))
            {
                UnityEngine.Object main = ab.LoadAsset(meta.mainAsset);
                assets = main != null ? new[] { main } : all;
            }
            else assets = all;
            var prefabs = assets.OfType<GameObject>().ToList();
            if (prefabs.Count == 0) throw new Exception("no GameObject assets in bundle");

            // clip set: top-level + AnimatorController sub-assets (emote bundles), name-sorted.
            // Sample EVERY clip: emote bundles carry an _Avatar clip (invisible skeleton) plus
            // prop clips — sampling only one can leave the visible meshes at rest pose.
            var clipSet = new Dictionary<int, AnimationClip>();
            foreach (AnimationClip c in all.OfType<AnimationClip>()) clipSet[c.GetInstanceID()] = c;
            foreach (RuntimeAnimatorController rac in all.OfType<RuntimeAnimatorController>())
                foreach (AnimationClip c in rac.animationClips)
                    if (c != null) clipSet[c.GetInstanceID()] = c;
            var clips = clipSet.Values.OrderBy(c => c.name, StringComparer.Ordinal).ToList();
            if (clips.Count == 0) throw new Exception("animated job but bundle has no AnimationClip");
            L(label + " prefabs=" + prefabs.Count + " clips=" + clips.Count +
              " [" + string.Join(",", clips.Select(c => c.name + ":" + F(c.length))) + "]");

            // ---- bounds pre-pass: rest bounds ∪ every sampled frame's bounds ----
            bool haveBounds = false;
            Bounds union = default, rest = default;
            for (int k = -1; k < s_frames; k++)  // k == -1 -> rest pose (no sampling)
            {
                Bounds fb;
                if (InstantiateSampleMeasure(prefabs, clips, k, out fb))
                {
                    if (!haveBounds) { union = fb; haveBounds = true; }
                    else union.Encapsulate(fb);
                    if (k == -1) rest = fb;
                }
            }
            if (!haveBounds) throw new Exception("no renderers in bundle");
            L(label + " rest c=" + rest.center.ToString("F2") + " e=" + rest.extents.ToString("F2") +
              " union c=" + union.center.ToString("F2") + " e=" + union.extents.ToString("F2"));

            // ---- render pass: fresh-instantiate -> sample once -> render -> destroy per frame ----
            var camGo = new GameObject("ABANIM_CAM");
            var lightGo = new GameObject("ABANIM_LIGHT");
            RenderTexture rt = null;
            Texture2D tex = null;
            AmbientMode oldAmb = RenderSettings.ambientMode;
            Color oldAmbColor = RenderSettings.ambientLight;
            try
            {
                // deterministic scene + framing math identical to AbVisualCompare.Shoot
                Camera cam = camGo.AddComponent<Camera>();
                cam.clearFlags = CameraClearFlags.SolidColor;
                cam.backgroundColor = new Color(0.15f, 0.15f, 0.18f, 1f);
                cam.fieldOfView = 50f;

                Light light = lightGo.AddComponent<Light>();
                light.type = LightType.Directional;
                light.intensity = 1.3f;
                light.color = Color.white;
                light.shadows = LightShadows.None;
                lightGo.transform.rotation = Quaternion.Euler(45f, -30f, 0f);

                RenderSettings.ambientMode = AmbientMode.Flat;
                RenderSettings.ambientLight = new Color(0.35f, 0.35f, 0.35f, 1f);

                float radius = Mathf.Max(union.extents.magnitude, 0.5f);
                float dist = radius * 2.0f;
                cam.nearClipPlane = Mathf.Max(dist / 1000f, 0.01f);
                cam.farClipPlane = dist * 20f;
                Vector3 dir = Quaternion.Euler(28f, s_azimuth, 0f) * Vector3.forward;
                camGo.transform.position = union.center - dir * dist;
                camGo.transform.LookAt(union.center);

                rt = new RenderTexture(s_size, s_size, 24, RenderTextureFormat.ARGB32) { antiAliasing = 1 };
                tex = new Texture2D(s_size, s_size, TextureFormat.RGBA32, false);

                for (int k = 0; k < s_frames; k++)
                {
                    GameObject frameRoot = InstantiateAndSample(prefabs, clips, k);
                    try
                    {
                        bool rendered = false;
                        var req = new RenderPipeline.StandardRequest { destination = rt };
                        if (RenderPipeline.SupportsRenderRequest(cam, req))
                        {
                            RenderPipeline.SubmitRenderRequest(cam, req);
                            rendered = true;
                        }
                        if (!rendered)
                        {
                            cam.targetTexture = rt;
                            cam.Render();
                            cam.targetTexture = null;
                        }
                        RenderTexture.active = rt;
                        tex.ReadPixels(new Rect(0, 0, s_size, s_size), 0, 0);
                        tex.Apply();
                        RenderTexture.active = null;
                        File.WriteAllBytes(Path.Combine(outDir, label + "-f" + k.ToString("00") + ".png"), tex.EncodeToPNG());
                    }
                    finally
                    {
                        UnityEngine.Object.DestroyImmediate(frameRoot);
                    }
                }
                L(label + " wrote " + s_frames + " frames");
            }
            finally
            {
                RenderSettings.ambientMode = oldAmb;
                RenderSettings.ambientLight = oldAmbColor;
                if (tex != null) UnityEngine.Object.DestroyImmediate(tex);
                if (rt != null) { rt.Release(); UnityEngine.Object.DestroyImmediate(rt); }
                UnityEngine.Object.DestroyImmediate(camGo);
                UnityEngine.Object.DestroyImmediate(lightGo);
            }

            // ---- sidecar ----
            var sb = new StringBuilder(512);
            sb.Append("{\"label\":\"").Append(J(label)).Append("\",");
            sb.Append("\"frames\":").Append(s_frames).Append(",");
            sb.Append("\"size\":").Append(s_size).Append(",");
            sb.Append("\"azimuth\":").Append(F(s_azimuth)).Append(",");
            sb.Append("\"clips\":[").Append(string.Join(",", clips.Select(c =>
                "{\"name\":\"" + J(c.name) + "\",\"length\":" + F(c.length) + "}"))).Append("],");
            sb.Append("\"boundsRest\":").Append(BJson(rest)).Append(",");
            sb.Append("\"boundsUnion\":").Append(BJson(union));
            sb.Append("}");
            File.WriteAllText(Path.Combine(outDir, label + ".anim.json"), sb.ToString());
        }
        finally
        {
            foreach (AssetBundle abx in loaded)
                if (abx != null) abx.Unload(true);
        }
    }

    static string BJson(Bounds b)
    {
        return "{\"center\":[" + F(b.center.x) + "," + F(b.center.y) + "," + F(b.center.z) +
               "],\"extents\":[" + F(b.extents.x) + "," + F(b.extents.y) + "," + F(b.extents.z) + "]}";
    }

    // Fresh-instantiate all prefabs and sample every clip at t_k = length * k / frames.
    // k < 0 means rest pose: instantiate only, no sampling.
    static GameObject InstantiateAndSample(List<GameObject> prefabs, List<AnimationClip> clips, int k)
    {
        var root = new GameObject("ABANIM_ROOT");
        var instances = new List<GameObject>();
        foreach (GameObject prefab in prefabs)
        {
            GameObject inst = UnityEngine.Object.Instantiate(prefab, root.transform);
            inst.name = prefab.name;
            instances.Add(inst);
        }
        if (k >= 0)
        {
            foreach (AnimationClip clip in clips)
            {
                float t = clip.length * k / s_frames;
                foreach (GameObject inst in instances) clip.SampleAnimation(inst, t);
            }
        }
        return root;
    }

    static bool InstantiateSampleMeasure(List<GameObject> prefabs, List<AnimationClip> clips, int k, out Bounds b)
    {
        GameObject root = InstantiateAndSample(prefabs, clips, k);
        try
        {
            Renderer[] rends = root.GetComponentsInChildren<Renderer>(true);
            if (rends.Length == 0) { b = default; return false; }
            b = rends[0].bounds;
            foreach (Renderer r in rends) b.Encapsulate(r.bounds);
            return true;
        }
        finally
        {
            UnityEngine.Object.DestroyImmediate(root);
        }
    }

    // ---- dep loading: identical rules to AbVisualCompare (kept in sync manually) ----
    static void LoadDep(string dep, string depsDir, List<AssetBundle> loaded, HashSet<string> visited)
    {
        if (dep.StartsWith("dcl/")) return; // shader bundle, pre-loaded
        if (!visited.Add(dep)) return;
        string p = Path.Combine(depsDir, dep);
        if (!File.Exists(p)) p = Path.Combine(depsDir, dep + "_" + s_platform);
        if (!File.Exists(p))
        {
            string want1 = dep.ToLowerInvariant(), want2 = (dep + "_" + s_platform).ToLowerInvariant();
            foreach (string f in Directory.GetFiles(depsDir))
            {
                string n = Path.GetFileName(f).ToLowerInvariant();
                if (n == want1 || n == want2) { p = f; break; }
            }
        }
        if (!File.Exists(p)) { L("  dep MISSING on disk: " + dep); return; }
        AssetBundle ab = AssetBundle.LoadFromFile(p);
        if (ab == null) { L("  dep load FAILED: " + dep); return; }
        loaded.Add(ab);
        TextAsset metaTa = ab.LoadAsset<TextAsset>("metadata.json");
        if (metaTa != null)
        {
            var m = new Meta();
            JsonUtility.FromJsonOverwrite(metaTa.text, m);
            foreach (string d in m.dependencies) LoadDep(d, depsDir, loaded, visited);
        }
        ab.LoadAllAssets();
        L("  dep loaded: " + dep);
    }

    static int SameDirScan(string bundlePath, string depsDir, List<AssetBundle> loaded)
    {
        int n = 0;
        string self = Path.GetFullPath(bundlePath);
        var files = Directory.GetFiles(depsDir).OrderBy(f => f, StringComparer.Ordinal).ToList();
        foreach (string f in files)
        {
            if (Path.GetFullPath(f) == self) continue;
            string name = Path.GetFileName(f);
            if (name.StartsWith(".") || name.EndsWith(".json") || name.EndsWith(".txt") || name.EndsWith(".log")) continue;
            AssetBundle ab = null;
            try { ab = AssetBundle.LoadFromFile(f); } catch { }
            if (ab == null) { L("  scan skip (not a bundle / dup CAB): " + name); continue; }
            loaded.Add(ab);
            try { ab.LoadAllAssets(); } catch (Exception e) { L("  scan LoadAllAssets fail " + name + ": " + e.Message); }
            n++;
        }
        L("  same-dir scan loaded " + n + " sibling bundles");
        return n;
    }
}

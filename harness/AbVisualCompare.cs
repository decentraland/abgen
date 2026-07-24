// abgen render harness — still-image + inventory capture for asset-bundle parity testing
// (abgen output vs upstream ab-cdn bundles). Drop into Assets/Editor/ of any Unity 6000.x
// project configured per harness/README.md (URP active, linear color space).
//
// Run: Unity -batchmode -quit -projectPath <project> -executeMethod AbVisualCompare.Run -logFile <log>
//
// Inputs  (env):
//   AB_ROOT      staging root (default /tmp/ab-compat). Layout: jobs.txt, shader/, out/.
//   AB_JOBS      jobs file name/path relative to AB_ROOT (default jobs.txt).
//   AB_PLATFORM  mac | windows | linux | webgl (default from the editor OS).
//   AB_SHADER    shader bundle path, relative to AB_ROOT unless absolute
//                (default shader/scene_ignore_<AB_PLATFORM>).
//   AB_AZIMUTHS  comma-separated camera azimuths in degrees (default 35,155,275).
//   AB_SIZE      square render size in px (default 1024).
//
// jobs file line: <label>|<kind>|<abs bundle path>|<abs deps dir>   kind: glb | animated | texture
//   (legacy 3-field lines <label>|<bundle>|<deps> are treated as kind=glb)
//
// Outputs to $AB_ROOT/out/:
//   <label>-a<i>.png        one per azimuth (i = 0..N-1)
//   <label>-anim.png        animated jobs only: every clip (name-sorted) sampled at its own
//                           t=length/2, rendered with the same framing as -a0
//   <label>-t<i>.png        texture jobs only: mip0 blit per Texture2D, native size, sRGB
//   <label>.inventory.json  assets/types, renderer/material/mesh/vert counts, errorShader,
//                           bounds, AnimationClip list w/ curve counts, texture list
//   <label>.FAILED.txt      exception text when the job failed
// Appends a line log to $AB_ROOT/harness.log. Exit code 0 = run completed (individual job
// failures are recorded per-label), 2 = fatal (e.g. shader bundle missing).
//
// Vintage tolerances kept from the parity campaign: missing metadata.json -> same-dir CAB
// scan; dep filename case drift (Qm... lowercased upstream); `_<platform>` dep suffix.
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Linq;
using System.Text;
using UnityEditor;
using UnityEngine;
using UnityEngine.Rendering;
using UnityEngine.Rendering.Universal;
using Debug = UnityEngine.Debug;

public static class AbVisualCompare
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
    static float[] s_azimuths;
    static int s_size;
    static UnityEngine.Object[] s_shaderAssets;

    static void L(string s)
    {
        Debug.Log("ABVIS: " + s);
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
            Debug.LogError("ABVIS FATAL: " + e);
            if (s_log != null) s_log.WriteLine("FATAL: " + e);
            rc = 2;
        }
        finally { if (s_log != null) s_log.Flush(); }
        // EditorApplication.Exit runs Unity's managed shutdown, which hangs
        // ("abort_threads: Failed aborting … mono_thread_manage") when a plugin
        // background thread refuses to abort. All outputs are already flushed to
        // disk, so terminate the process outright to guarantee a bounded exit.
        if (s_log != null) s_log.Close();
        System.Console.Out.Flush();
        System.Diagnostics.Process.GetCurrentProcess().Kill();
    }

    static void RunInner()
    {
        string root = Env("AB_ROOT", "/tmp/ab-compat");
        s_platform = Env("AB_PLATFORM", DefaultPlatform());
        s_azimuths = Env("AB_AZIMUTHS", "35,155,275")
            .Split(',')
            .Select(x => float.Parse(x.Trim(), System.Globalization.CultureInfo.InvariantCulture))
            .ToArray();
        s_size = int.Parse(Env("AB_SIZE", "1024"));
        string outDir = Path.Combine(root, "out");
        Directory.CreateDirectory(outDir);
        s_log = new StreamWriter(Path.Combine(root, "harness.log"), true) { AutoFlush = true };
        L("=== run start, platform=" + s_platform + " size=" + s_size +
          " azimuths=" + string.Join("/", s_azimuths.Select(F)) +
          " pipeline=" + (GraphicsSettings.currentRenderPipeline != null ? GraphicsSettings.currentRenderPipeline.name : "builtin"));

        string shaderPath = Env("AB_SHADER", Path.Combine("shader", "scene_ignore_" + s_platform));
        if (!Path.IsPathRooted(shaderPath)) shaderPath = Path.Combine(root, shaderPath);
        AssetBundle shaderAb = AssetBundle.LoadFromFile(shaderPath);
        if (shaderAb == null) throw new Exception("shader bundle failed to load: " + shaderPath);
        s_shaderAssets = shaderAb.LoadAllAssets();
        L("shader bundle loaded, assets=" + s_shaderAssets.Length +
          " shaders=[" + string.Join(",", s_shaderAssets.OfType<Shader>().Select(sh => sh.name)) + "]");

        string jobsPath = Env("AB_JOBS", "jobs.txt");
        if (!Path.IsPathRooted(jobsPath)) jobsPath = Path.Combine(root, jobsPath);
        foreach (string line in File.ReadAllLines(jobsPath))
        {
            string t = line.Trim();
            if (t.Length == 0 || t.StartsWith("#")) continue;
            string[] parts = t.Split('|');
            string label, kind, bundlePath, depsDir;
            if (parts.Length >= 4) { label = parts[0]; kind = parts[1]; bundlePath = parts[2]; depsDir = parts[3]; }
            else { label = parts[0]; kind = "glb"; bundlePath = parts[1]; depsDir = parts[2]; }
            L("JOB " + label + " kind=" + kind + " " + bundlePath);
            var sw = Stopwatch.StartNew();
            try
            {
                if (kind == "rawglb") RenderRawGlb(label, bundlePath, outDir);
                else RenderJob(label, kind, bundlePath, depsDir, outDir);
            }
            catch (Exception e)
            {
                L("JOB FAIL " + label + ": " + e.Message);
                File.WriteAllText(Path.Combine(outDir, label + ".FAILED.txt"), e.ToString());
            }
            L("TIME " + label + " kind=" + kind + " ms=" + sw.ElapsedMilliseconds);
        }
        try { RenderEnvProbe(outDir); }
        catch (Exception e) { L("envprobe FAIL: " + e.Message); }
        L("=== run end");
    }

    static void RenderEnvProbe(string outDir)
    {
        Shader lit = Shader.Find("Universal Render Pipeline/Lit");
        if (lit == null) { L("envprobe skipped: URP/Lit not found"); return; }
        var chrome = GameObject.CreatePrimitive(PrimitiveType.Sphere);
        var diffuse = GameObject.CreatePrimitive(PrimitiveType.Sphere);
        Material mc = null, md = null;
        try
        {
            chrome.transform.position = new Vector3(-0.7f, 0f, 0f);
            diffuse.transform.position = new Vector3(0.7f, 0f, 0f);
            mc = new Material(lit) { color = Color.white };
            mc.SetFloat("_Metallic", 1f);
            mc.SetFloat("_Smoothness", 0.95f);
            md = new Material(lit) { color = new Color(0.5f, 0.5f, 0.5f) };
            md.SetFloat("_Metallic", 0f);
            md.SetFloat("_Smoothness", 0.5f);
            chrome.GetComponent<Renderer>().sharedMaterial = mc;
            diffuse.GetComponent<Renderer>().sharedMaterial = md;
            var b = new Bounds(Vector3.zero, new Vector3(2.8f, 1.4f, 1.4f));
            Shoot("envprobe", b, outDir, new float[] { 35f }, new[] { "-a0" });
        }
        finally
        {
            UnityEngine.Object.DestroyImmediate(chrome);
            UnityEngine.Object.DestroyImmediate(diffuse);
            if (mc != null) UnityEngine.Object.DestroyImmediate(mc);
            if (md != null) UnityEngine.Object.DestroyImmediate(md);
        }
    }

    // ---------- inventory ----------
    class Inv
    {
        public string label, kind, bundle, depPath = "none", mainAsset = "", error = "";
        public int deps, instantiated, renderers, skinnedRenderers, materials, errorShader, meshAssets, gameObjectAssets;
        public long vertexTotal;
        public Vector3 boundsCenter, boundsExtents;
        public bool hasBounds;
        public List<string[]> assets = new List<string[]>();          // [name, type]
        public SortedDictionary<string, int> typeCounts = new SortedDictionary<string, int>(StringComparer.Ordinal);
        public List<string> clips = new List<string>();               // pre-rendered json fragments
        public List<string> textures = new List<string>();            // pre-rendered json fragments
        public List<string> texPngs = new List<string>();
        public List<string> sampledClips = new List<string>(); // pre-rendered json fragments

        public void Write(string path)
        {
            var sb = new StringBuilder(4096);
            sb.Append("{");
            sb.Append("\"label\":\"").Append(J(label)).Append("\",");
            sb.Append("\"kind\":\"").Append(J(kind)).Append("\",");
            sb.Append("\"bundle\":\"").Append(J(bundle)).Append("\",");
            sb.Append("\"depPath\":\"").Append(J(depPath)).Append("\",");
            sb.Append("\"mainAsset\":\"").Append(J(mainAsset)).Append("\",");
            sb.Append("\"deps\":").Append(deps).Append(",");
            sb.Append("\"assetCount\":").Append(assets.Count).Append(",");
            sb.Append("\"assetTypeCounts\":{");
            bool first = true;
            foreach (var kv in typeCounts)
            {
                if (!first) sb.Append(",");
                first = false;
                sb.Append("\"").Append(J(kv.Key)).Append("\":").Append(kv.Value);
            }
            sb.Append("},");
            sb.Append("\"assets\":[");
            for (int i = 0; i < assets.Count; i++)
            {
                if (i > 0) sb.Append(",");
                sb.Append("{\"name\":\"").Append(J(assets[i][0])).Append("\",\"type\":\"").Append(J(assets[i][1])).Append("\"}");
            }
            sb.Append("],");
            sb.Append("\"gameObjectAssets\":").Append(gameObjectAssets).Append(",");
            sb.Append("\"instantiated\":").Append(instantiated).Append(",");
            sb.Append("\"renderers\":").Append(renderers).Append(",");
            sb.Append("\"skinnedRenderers\":").Append(skinnedRenderers).Append(",");
            sb.Append("\"materials\":").Append(materials).Append(",");
            sb.Append("\"errorShader\":").Append(errorShader).Append(",");
            sb.Append("\"meshAssets\":").Append(meshAssets).Append(",");
            sb.Append("\"vertexTotal\":").Append(vertexTotal).Append(",");
            if (hasBounds)
                sb.Append("\"bounds\":{\"center\":[").Append(F(boundsCenter.x)).Append(",").Append(F(boundsCenter.y)).Append(",").Append(F(boundsCenter.z))
                  .Append("],\"extents\":[").Append(F(boundsExtents.x)).Append(",").Append(F(boundsExtents.y)).Append(",").Append(F(boundsExtents.z)).Append("]},");
            else
                sb.Append("\"bounds\":null,");
            sb.Append("\"animationClips\":[").Append(string.Join(",", clips)).Append("],");
            sb.Append("\"textures\":[").Append(string.Join(",", textures)).Append("],");
            sb.Append("\"texPngs\":[").Append(string.Join(",", texPngs.Select(p => "\"" + J(p) + "\""))).Append("],");
            sb.Append("\"sampledClips\":[").Append(string.Join(",", sampledClips)).Append("],");
            sb.Append("\"error\":\"").Append(J(error)).Append("\"");
            sb.Append("}");
            File.WriteAllText(path, sb.ToString());
        }
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

    // ---------- job ----------
    static void RenderJob(string label, string kind, string bundlePath, string depsDir, string outDir)
    {
        var loaded = new List<AssetBundle>();
        var createdTex = new List<UnityEngine.Object>();
        GameObject rootGo = null;
        var inv = new Inv { label = label, kind = kind, bundle = bundlePath };
        try
        {
            AssetBundle ab = AssetBundle.LoadFromFile(bundlePath);
            if (ab == null) throw new Exception("bundle load failed: " + bundlePath);
            loaded.Add(ab);

            var meta = new Meta();
            bool hasMeta = false;
            TextAsset metaTa = ab.LoadAsset<TextAsset>("metadata.json");
            if (metaTa != null) { JsonUtility.FromJsonOverwrite(metaTa.text, meta); hasMeta = true; }

            if (kind != "texture")
            {
                if (hasMeta)
                {
                    inv.depPath = "metadata";
                    var visited = new HashSet<string>();
                    foreach (string d in meta.dependencies) LoadDep(d, depsDir, loaded, visited);
                    inv.deps = meta.dependencies.Count;
                }
                else
                {
                    // VINTAGE fallback: no metadata.json packed in the bundle (old converter
                    // versions). Load every sibling bundle in depsDir so any internal CAB
                    // dependency resolves.
                    inv.depPath = "same-dir-scan";
                    inv.deps = SameDirScan(bundlePath, depsDir, loaded);
                }
                L(label + " depPath=" + inv.depPath + " deps=" + inv.deps + (hasMeta ? " mainAsset='" + meta.mainAsset + "'" : " (no metadata.json)"));
            }
            else
            {
                inv.depPath = "none";
                L(label + " texture job, no dep loading" + (hasMeta ? " (metadata.json present)" : " (no metadata.json)"));
            }
            inv.mainAsset = hasMeta ? meta.mainAsset : "";

            UnityEngine.Object[] all = ab.LoadAllAssets();
            foreach (UnityEngine.Object a in all)
            {
                string tn = a == null ? "null" : a.GetType().Name;
                string nm = a == null ? "" : a.name;
                inv.assets.Add(new[] { nm, tn });
                int c; inv.typeCounts.TryGetValue(tn, out c); inv.typeCounts[tn] = c + 1;
                if (a is GameObject) inv.gameObjectAssets++;
                var mesh = a as Mesh;
                if (mesh != null) { inv.meshAssets++; inv.vertexTotal += mesh.vertexCount; }
            }
            inv.assets.Sort((x, y) => string.CompareOrdinal(x[0] + "|" + x[1], y[0] + "|" + y[1]));
            L(label + " assets=" + all.Length);

            // texture inventory (all kinds)
            var texList = all.OfType<Texture2D>().OrderBy(t => t.name, StringComparer.Ordinal).ToList();
            foreach (Texture2D t in texList)
                inv.textures.Add("{\"name\":\"" + J(t.name) + "\",\"width\":" + t.width + ",\"height\":" + t.height +
                                 ",\"format\":\"" + t.format + "\",\"mips\":" + t.mipmapCount + "}");

            // AnimationClip inventory (all kinds). Emote bundles pack clips as sub-assets of an
            // AnimatorController (not top-level), so also pull RuntimeAnimatorController.animationClips.
            // reference-keyed dedup: GetInstanceID() is obsolete-as-error on
            // Unity 6000.5+, and same instance <=> same id anyway
            var clipSet = new HashSet<AnimationClip>();
            foreach (AnimationClip c0 in all.OfType<AnimationClip>()) clipSet.Add(c0);
            foreach (RuntimeAnimatorController rac in all.OfType<RuntimeAnimatorController>())
                foreach (AnimationClip c0 in rac.animationClips)
                    if (c0 != null) clipSet.Add(c0);
            var clipList = clipSet.OrderBy(c2 => c2.name, StringComparer.Ordinal).ToList();
            foreach (AnimationClip c2 in clipList)
            {
                // GetCurveBindings returns [] for bundle-loaded (runtime-optimized) clips, so read
                // real curve counts from the internal AnimationUtility.GetAnimationClipStats.
                int curves = 0, objCurves = 0;
                try { curves = AnimationUtility.GetCurveBindings(c2).Length; } catch { }
                try { objCurves = AnimationUtility.GetObjectReferenceCurveBindings(c2).Length; } catch { }
                int totalCurves = -1, statSize = -1;
                try
                {
                    var mi = typeof(AnimationUtility).GetMethod("GetAnimationClipStats",
                        System.Reflection.BindingFlags.NonPublic | System.Reflection.BindingFlags.Static);
                    object stats = mi.Invoke(null, new object[] { c2 });
                    var ft = stats.GetType();
                    var fTot = ft.GetField("totalCurves"); if (fTot != null) totalCurves = (int)fTot.GetValue(stats);
                    var fSize = ft.GetField("size"); if (fSize != null) statSize = (int)fSize.GetValue(stats);
                }
                catch { }
                inv.clips.Add("{\"name\":\"" + J(c2.name) + "\",\"length\":" + F(c2.length) + ",\"curves\":" + curves +
                              ",\"objectRefCurves\":" + objCurves + ",\"totalCurves\":" + totalCurves +
                              ",\"statSize\":" + statSize + ",\"frameRate\":" + F(c2.frameRate) +
                              ",\"legacy\":" + (c2.legacy ? "true" : "false") + ",\"looping\":" + (c2.isLooping ? "true" : "false") +
                              ",\"humanMotion\":" + (c2.humanMotion ? "true" : "false") +
                              ",\"wrapMode\":\"" + c2.wrapMode + "\"}");
            }
            if (clipList.Count > 0) L(label + " clips=" + clipList.Count + " first=" + clipList[0].name + " len=" + clipList[0].length);

            if (kind == "texture")
            {
                if (texList.Count == 0) throw new Exception("no Texture2D assets in texture bundle");
                int ti = 0;
                foreach (Texture2D t in texList)
                {
                    string png = label + "-t" + ti + ".png";
                    BlitTexToPng(t, Path.Combine(outDir, png));
                    inv.texPngs.Add(png);
                    L(label + " wrote " + png + " " + t.width + "x" + t.height + " " + t.format);
                    ti++;
                }
                return; // inventory written in finally
            }

            // --- glb / animated: instantiate + render ---
            UnityEngine.Object[] assets;
            if (!string.IsNullOrEmpty(meta.mainAsset))
            {
                UnityEngine.Object main = ab.LoadAsset(meta.mainAsset);
                assets = main != null ? new[] { main } : all;
            }
            else assets = all;

            rootGo = new GameObject("ABVIS_ROOT");
            var instances = new List<GameObject>();
            foreach (UnityEngine.Object a in assets)
            {
                var prefab = a as GameObject;
                if (prefab == null) continue;
                GameObject inst = UnityEngine.Object.Instantiate(prefab, rootGo.transform);
                inst.name = prefab.name;
                inst.SetActive(true);
                foreach (Transform tr in inst.GetComponentsInChildren<Transform>(true))
                    tr.gameObject.SetActive(true);
                instances.Add(inst);
            }
            inv.instantiated = instances.Count;
            L(label + " instantiated=" + instances.Count);
            if (instances.Count == 0) throw new Exception("no GameObject assets in bundle");

            Renderer[] rends = rootGo.GetComponentsInChildren<Renderer>(true);
            inv.renderers = rends.Length;
            inv.skinnedRenderers = rootGo.GetComponentsInChildren<SkinnedMeshRenderer>(true).Length;
            foreach (Renderer r in rends)
                foreach (Material m in r.sharedMaterials)
                {
                    inv.materials++;
                    if (m == null || m.shader == null || m.shader.name == "Hidden/InternalErrorShader") inv.errorShader++;
                }
            int activeRends = rends.Count(r => r.enabled && r.gameObject.activeInHierarchy);
            L(label + " renderers=" + rends.Length + " active=" + activeRends + " materials=" + inv.materials + " errorShader=" + inv.errorShader);

            long triTotal = 0, vertTotal = 0;
            foreach (var mf in rootGo.GetComponentsInChildren<MeshFilter>(true))
            {
                Mesh gm = mf.sharedMesh; if (gm == null) continue;
                uint idx = 0; for (int s = 0; s < gm.subMeshCount; s++) idx += gm.GetIndexCount(s);
                triTotal += idx / 3; vertTotal += gm.vertexCount;
            }
            foreach (var smr in rootGo.GetComponentsInChildren<SkinnedMeshRenderer>(true))
            {
                Mesh gm = smr.sharedMesh; if (gm == null) continue;
                uint idx = 0; for (int s = 0; s < gm.subMeshCount; s++) idx += gm.GetIndexCount(s);
                triTotal += idx / 3; vertTotal += gm.vertexCount;
            }
            L(label + " geometry tris=" + triTotal + " verts=" + vertTotal);

            if (rends.Length == 0)
            {
                L(label + " no renderers -> inventory only, skip render");
                return;
            }

            string omat = Environment.GetEnvironmentVariable("AB_OVERRIDE_MAT");
            if (omat == "1" || omat == "tex")
            {
                Shader lit = Shader.Find("Universal Render Pipeline/Lit");
                if (lit != null)
                {
                    bool textured = omat == "tex";
                    var dumped = new HashSet<Texture>();
                    foreach (Renderer r in rends)
                    {
                        var orig = r.sharedMaterials;
                        var mats = new Material[orig.Length];
                        for (int mi = 0; mi < mats.Length; mi++)
                        {
                            var lm = new Material(lit) { color = new Color(0.72f, 0.72f, 0.74f) };
                            if (textured)
                            {
                                Texture bt = FindBaseTexture(orig[mi]);
                                if (bt != null)
                                {
                                    lm.SetTexture("_BaseMap", bt);
                                    lm.color = Color.white;
                                    string on = orig[mi] != null ? orig[mi].name.ToLowerInvariant() : "";
                                    if (on.Contains("texturebakeresult") && (on.Contains("transparent") || on.Contains("cutout")))
                                    {
                                        lm.SetFloat("_Surface", 1f);
                                        lm.SetFloat("_SrcBlend", 5f);
                                        lm.SetFloat("_DstBlend", 10f);
                                        lm.SetFloat("_ZWrite", 0f);
                                        lm.EnableKeyword("_SURFACE_TYPE_TRANSPARENT");
                                        lm.renderQueue = 3000;
                                    }
                                    var bt2 = bt as Texture2D;
                                    if (bt2 != null && dumped.Add(bt))
                                    {
                                        string tp = Path.Combine(outDir, label + "-basemap-" + dumped.Count + ".png");
                                        try { BlitTexToPng(bt2, tp); L(label + " dumped basemap '" + bt2.name + "' -> " + Path.GetFileName(tp)); }
                                        catch (Exception te) { L(label + " basemap dump failed: " + te.Message); }
                                    }
                                }
                                if (orig[mi] != null && orig[mi].HasProperty("_EmissionMap"))
                                {
                                    Texture et = orig[mi].GetTexture("_EmissionMap");
                                    if (et != null)
                                    {
                                        Color ec = orig[mi].HasProperty("_EmissionColor") ? orig[mi].GetColor("_EmissionColor") : Color.white;
                                        lm.SetTexture("_EmissionMap", et);
                                        lm.SetColor("_EmissionColor", ec);
                                        lm.EnableKeyword("_EMISSION");
                                        lm.globalIlluminationFlags = MaterialGlobalIlluminationFlags.None;
                                        L(label + " emission map carried onto override mat " + mi + " color=" + ec);
                                    }
                                }
                                if (orig[mi] != null && orig[mi].HasProperty("_Metallic") && orig[mi].HasProperty("_Smoothness"))
                                {
                                    float met = orig[mi].GetFloat("_Metallic");
                                    float smo = orig[mi].GetFloat("_Smoothness");
                                    if (met != 0f || smo != 0f)
                                    {
                                        lm.SetFloat("_Metallic", met);
                                        lm.SetFloat("_Smoothness", smo);
                                        L(label + " metallic floats carried onto override mat " + mi + " m=" + met + " s=" + smo);
                                    }
                                }
                            }
                            mats[mi] = lm;
                        }
                        r.sharedMaterials = mats;
                    }
                    L(label + " material override -> URP Lit" + (textured ? " +baseMap (textures visible)" : " (geometry-visible)"));
                }
                else L(label + " material override requested but URP/Lit shader not found");
            }
            else RealShaderBind(label, rootGo, rends, createdTex);

            Bounds b = rends[0].bounds;
            foreach (Renderer r in rends) b.Encapsulate(r.bounds);
            inv.hasBounds = true; inv.boundsCenter = b.center; inv.boundsExtents = b.extents;
            L(label + " bounds c=" + b.center.ToString("F2") + " e=" + b.extents.ToString("F2"));

            string[] suffixes = Enumerable.Range(0, s_azimuths.Length).Select(i => "-a" + i).ToArray();
            Shoot(label, b, outDir, s_azimuths, suffixes);

            if (Env("AB_ABLATE", "") == "1" && omat != "1" && omat != "tex")
            {
                var mats = new List<Material>();
                var seenM = new HashSet<Material>();
                foreach (Renderer r in rends)
                    foreach (Material m in r.sharedMaterials)
                        if (m != null && seenM.Add(m)) mats.Add(m);

                var savedTex = new List<KeyValuePair<Material, KeyValuePair<string, Texture>>>();
                foreach (Material m in mats)
                    foreach (string prop in new[] { "_EmissionMap", "_MetallicGlossMap" })
                        if (m.HasProperty(prop) && m.GetTexture(prop) != null)
                        {
                            savedTex.Add(new KeyValuePair<Material, KeyValuePair<string, Texture>>(
                                m, new KeyValuePair<string, Texture>(prop, m.GetTexture(prop))));
                            m.SetTexture(prop, null);
                        }
                if (savedTex.Count > 0)
                {
                    Shoot(label, b, outDir, new float[] { s_azimuths[0] }, new[] { "-abl-planes" });
                    foreach (var kv in savedTex) kv.Key.SetTexture(kv.Value.Key, kv.Value.Value);
                }

                var savedFl = new List<KeyValuePair<Material, Vector2>>();
                foreach (Material m in mats)
                    if (m.HasProperty("_Metallic") && m.HasProperty("_Smoothness") &&
                        (m.GetFloat("_Metallic") != 0f || m.GetFloat("_Smoothness") != 0f))
                    {
                        savedFl.Add(new KeyValuePair<Material, Vector2>(
                            m, new Vector2(m.GetFloat("_Metallic"), m.GetFloat("_Smoothness"))));
                        m.SetFloat("_Metallic", 0f);
                        m.SetFloat("_Smoothness", 0f);
                    }
                if (savedFl.Count > 0)
                {
                    Shoot(label, b, outDir, new float[] { s_azimuths[0] }, new[] { "-abl-floats" });
                    foreach (var kv in savedFl)
                    {
                        kv.Key.SetFloat("_Metallic", kv.Value.x);
                        kv.Key.SetFloat("_Smoothness", kv.Value.y);
                    }
                }
                L(label + " ablation: planes=" + savedTex.Count + " floats=" + savedFl.Count);
            }

            if (kind == "animated")
            {
                if (clipList.Count == 0) throw new Exception("animated job but bundle has no AnimationClip");
                // Sample EVERY clip (name-sorted, each at its own t=length/2): emote bundles carry
                // an _Avatar clip (invisible skeleton) plus prop clips — sampling only the first
                // alphabetical clip can leave the visible meshes at rest pose.
                foreach (AnimationClip clip in clipList)
                {
                    float t = clip.length * 0.5f;
                    foreach (GameObject inst in instances) clip.SampleAnimation(inst, t);
                    inv.sampledClips.Add("{\"name\":\"" + J(clip.name) + "\",\"t\":" + F(t) + "}");
                    L(label + " sampled clip '" + clip.name + "' at t=" + t);
                }
                // same framing as rest pose a0 so pairs stay comparable
                Shoot(label, b, outDir, new float[] { s_azimuths[0] }, new[] { "-anim" });
            }
        }
        catch (Exception e)
        {
            inv.error = e.Message;
            throw;
        }
        finally
        {
            try { inv.Write(Path.Combine(outDir, label + ".inventory.json")); }
            catch (Exception we) { L(label + " inventory write FAILED: " + we.Message); }
            if (rootGo != null) UnityEngine.Object.DestroyImmediate(rootGo);
            foreach (AssetBundle abx in loaded)
                if (abx != null) abx.Unload(true);
            foreach (UnityEngine.Object o in createdTex)
                if (o != null) UnityEngine.Object.DestroyImmediate(o);
        }
    }

    static void RealShaderBind(string label, GameObject rootGo, Renderer[] rends, List<UnityEngine.Object> created)
    {
        Shader texarr = null;
        if (s_shaderAssets != null)
            texarr = s_shaderAssets.OfType<Shader>().FirstOrDefault(sh => sh.name == "DCL/Scene_TexArray");
        var seen = new HashSet<Material>();
        int pptr = 0, post = 0, unresolved = 0, rebound = 0;
        bool placed = false;
        Vector3 rootPos = Vector3.zero;
        foreach (Renderer r in rends)
            foreach (Material m in r.sharedMaterials)
            {
                if (m == null || !seen.Add(m)) continue;
                bool broken = m.shader == null || m.shader.name == "Hidden/InternalErrorShader";
                if (broken && texarr != null) { m.shader = texarr; post++; broken = false; }
                else if (!broken) pptr++;
                if (broken)
                {
                    unresolved++;
                    L(label + " mat '" + m.name + "' shader UNRESOLVED");
                    continue;
                }
                if (m.shader.name == "DCL/Scene_TexArray" && m.HasProperty("_BaseMapArr"))
                {
                    if (m.GetTexture("_BaseMapArr") == null)
                    {
                        var bt = (m.HasProperty("_BaseMap") ? m.GetTexture("_BaseMap") : null) as Texture2D;
                        if (bt != null)
                        {
                            var arr = new Texture2DArray(bt.width, bt.height, 1, bt.format, bt.mipmapCount > 1);
                            Graphics.CopyTexture(bt, 0, arr, 0);
                            m.SetTexture("_BaseMapArr", arr);
                            m.SetInteger("_BaseMapArr_ID", 0);
                            created.Add(arr);
                            rebound++;
                        }
                    }
                    if (!placed && m.HasProperty("_PlaneClipping"))
                    {
                        Vector4 pc = m.GetVector("_PlaneClipping");
                        if (pc.y > pc.x)
                        {
                            rootPos = new Vector3(pc.x + 0.05f, 0f, pc.z + 0.05f);
                            rootGo.transform.position = rootPos;
                            placed = true;
                        }
                    }
                }
                L(label + " mat '" + m.name + "' shader='" + m.shader.name + "'" +
                  " kw=[" + string.Join(",", m.shaderKeywords) + "]" +
                  " metallic=" + (m.HasProperty("_Metallic") ? F(m.GetFloat("_Metallic")) : "-") +
                  " smooth=" + (m.HasProperty("_Smoothness") ? F(m.GetFloat("_Smoothness")) : "-") +
                  " planeClip=" + (m.HasProperty("_PlaneClipping") ? m.GetVector("_PlaneClipping").ToString("F2") : "-"));
            }
        L(label + " real-shader bind: pptr=" + pptr + " postassign=" + post +
          " unresolved=" + unresolved + " texarrayRebind=" + rebound +
          " rootAt=" + rootPos.ToString("F2"));
    }

    static void RenderRawGlb(string label, string assetPath, string outDir)
    {
        var inv = new Inv { label = label, kind = "rawglb", bundle = assetPath };
        GameObject rootGo = null;
        try
        {
            GameObject prefab = AssetDatabase.LoadAssetAtPath<GameObject>(assetPath);
            if (prefab == null)
            {
                AssetDatabase.ImportAsset(assetPath, ImportAssetOptions.ForceSynchronousImport);
                prefab = AssetDatabase.LoadAssetAtPath<GameObject>(assetPath);
            }
            if (prefab == null) throw new Exception("glb import produced no GameObject: " + assetPath);

            rootGo = new GameObject("ABVIS_ROOT");
            GameObject inst = UnityEngine.Object.Instantiate(prefab, rootGo.transform);
            inst.name = prefab.name;
            inst.SetActive(true);
            foreach (Transform tr in inst.GetComponentsInChildren<Transform>(true))
                tr.gameObject.SetActive(true);
            inv.instantiated = 1;

            Renderer[] rends = rootGo.GetComponentsInChildren<Renderer>(true);
            inv.renderers = rends.Length;
            foreach (Renderer r in rends)
                foreach (Material m in r.sharedMaterials)
                {
                    inv.materials++;
                    if (m == null || m.shader == null || m.shader.name == "Hidden/InternalErrorShader") inv.errorShader++;
                }

            long triTotal = 0;
            foreach (var mf in rootGo.GetComponentsInChildren<MeshFilter>(true))
            {
                Mesh gm = mf.sharedMesh; if (gm == null) continue;
                uint idx = 0; for (int s = 0; s < gm.subMeshCount; s++) idx += gm.GetIndexCount(s);
                triTotal += idx / 3; inv.vertexTotal += gm.vertexCount;
            }
            L(label + " rawglb renderers=" + rends.Length + " materials=" + inv.materials +
              " errorShader=" + inv.errorShader + " tris=" + triTotal + " verts=" + inv.vertexTotal);
            if (rends.Length == 0) throw new Exception("no renderers in imported glb");

            Bounds b = rends[0].bounds;
            foreach (Renderer r in rends) b.Encapsulate(r.bounds);
            inv.hasBounds = true; inv.boundsCenter = b.center; inv.boundsExtents = b.extents;
            L(label + " bounds c=" + b.center.ToString("F2") + " e=" + b.extents.ToString("F2"));

            string[] suffixes = Enumerable.Range(0, s_azimuths.Length).Select(i => "-a" + i).ToArray();
            Shoot(label, b, outDir, s_azimuths, suffixes);
        }
        catch (Exception e)
        {
            inv.error = e.Message;
            throw;
        }
        finally
        {
            try { inv.Write(Path.Combine(outDir, label + ".inventory.json")); }
            catch (Exception we) { L(label + " inventory write FAILED: " + we.Message); }
            if (rootGo != null) UnityEngine.Object.DestroyImmediate(rootGo);
        }
    }

    static void LoadDep(string dep, string depsDir, List<AssetBundle> loaded, HashSet<string> visited)
    {
        if (dep.StartsWith("dcl/")) return; // shader bundle, pre-loaded
        if (!visited.Add(dep)) return;
        string p = Path.Combine(depsDir, dep);
        if (!File.Exists(p)) p = Path.Combine(depsDir, dep + "_" + s_platform);
        if (!File.Exists(p))
        {
            // vintage tolerance: manifests/deps may differ in hash case (Qm... lowercased)
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

    // Load every sibling bundle in depsDir (except the job bundle itself) so internal
    // CAB references resolve when metadata.json is absent (vintage upstream bundles).
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

    static Texture FindBaseTexture(Material m)
    {
        if (m == null) return null;
        if (m.mainTexture != null) return m.mainTexture;
        Shader sh = m.shader;
        if (sh == null) return null;
        int pc = UnityEditor.ShaderUtil.GetPropertyCount(sh);
        for (int pi = 0; pi < pc; pi++)
        {
            if (UnityEditor.ShaderUtil.GetPropertyType(sh, pi) == UnityEditor.ShaderUtil.ShaderPropertyType.TexEnv)
            {
                Texture t = m.GetTexture(UnityEditor.ShaderUtil.GetPropertyName(sh, pi));
                if (t is Texture2D) return t;
            }
        }
        return null;
    }

    static int ReadBack(RenderTexture rt, Texture2D tex)
    {
        RenderTexture prev = RenderTexture.active;
        RenderTexture.active = rt;
        tex.ReadPixels(new Rect(0, 0, tex.width, tex.height), 0, 0);
        tex.Apply();
        RenderTexture.active = prev;
        Color32[] scan = tex.GetPixels32();
        if (scan.Length == 0) return 0;
        Color32 bg = scan[0];
        int nc = 0;
        for (int p = 0; p < scan.Length; p++)
            if (scan[p].r != bg.r || scan[p].g != bg.g || scan[p].b != bg.b) nc++;
        return nc;
    }

    static void BlitTexToPng(Texture2D t, string path)
    {
        RenderTexture rt = RenderTexture.GetTemporary(t.width, t.height, 0, RenderTextureFormat.ARGB32, RenderTextureReadWrite.sRGB);
        Texture2D outTex = null;
        RenderTexture prev = RenderTexture.active;
        try
        {
            Graphics.Blit(t, rt); // samples mip0 at 1:1
            RenderTexture.active = rt;
            outTex = new Texture2D(t.width, t.height, TextureFormat.RGBA32, false);
            outTex.ReadPixels(new Rect(0, 0, t.width, t.height), 0, 0);
            outTex.Apply();
            File.WriteAllBytes(path, outTex.EncodeToPNG());
        }
        finally
        {
            RenderTexture.active = prev;
            if (outTex != null) UnityEngine.Object.DestroyImmediate(outTex);
            RenderTexture.ReleaseTemporary(rt);
        }
    }

    static void Shoot(string label, Bounds b, string outDir, float[] azimuths, string[] suffixes)
    {
        var camGo = new GameObject("ABVIS_CAM");
        var lightGo = new GameObject("ABVIS_LIGHT");
        var volGo = new GameObject("ABVIS_VOLUME");
        RenderTexture rt = null;
        Texture2D tex = null;
        Material skyMat = null;
        Cubemap skyCube = null;
        VolumeProfile volProfile = null;
        AmbientMode oldAmb = RenderSettings.ambientMode;
        Color oldAmbColor = RenderSettings.ambientLight;
        Color oldAmbSky = RenderSettings.ambientSkyColor;
        Color oldAmbEq = RenderSettings.ambientEquatorColor;
        Color oldAmbGnd = RenderSettings.ambientGroundColor;
        Material oldSkybox = RenderSettings.skybox;
        DefaultReflectionMode oldRefl = RenderSettings.defaultReflectionMode;
        Texture oldCustomRefl = RenderSettings.customReflectionTexture;
        try
        {
            Camera cam = camGo.AddComponent<Camera>();
            cam.clearFlags = CameraClearFlags.SolidColor;
            cam.backgroundColor = new Color(0.15f, 0.15f, 0.18f, 1f);
            cam.fieldOfView = 50f;
            cam.cullingMask = ~0;
            cam.allowHDR = true;
            var camData = cam.GetUniversalAdditionalCameraData();
            if (camData != null) camData.renderPostProcessing = true;

            Light light = lightGo.AddComponent<Light>();
            light.type = LightType.Directional;
            light.intensity = 1.3f;
            light.color = Color.white;
            light.shadows = LightShadows.None; // deterministic
            lightGo.transform.rotation = Quaternion.Euler(45f, -30f, 0f);

            skyMat = new Material(Shader.Find("Skybox/Procedural"));
            RenderSettings.skybox = skyMat;
            RenderSettings.ambientMode = AmbientMode.Skybox;
            DynamicGI.UpdateEnvironment();
            var shp = RenderSettings.ambientProbe;
            float shMag = Mathf.Abs(shp[0, 0]) + Mathf.Abs(shp[1, 0]) + Mathf.Abs(shp[2, 0]);
            if (shMag < 1e-3f)
            {
                RenderSettings.ambientMode = AmbientMode.Trilight;
                RenderSettings.ambientSkyColor = new Color(0.52f, 0.62f, 0.78f);
                RenderSettings.ambientEquatorColor = new Color(0.42f, 0.43f, 0.47f);
                RenderSettings.ambientGroundColor = new Color(0.24f, 0.22f, 0.21f);
            }
            skyCube = BuildSkyCubemap(-lightGo.transform.forward);
            RenderSettings.defaultReflectionMode = DefaultReflectionMode.Custom;
            RenderSettings.customReflectionTexture = skyCube;

            volProfile = ScriptableObject.CreateInstance<VolumeProfile>();
            var tm = volProfile.Add<Tonemapping>(true);
            tm.mode.Override(TonemappingMode.ACES);
            var vol = volGo.AddComponent<Volume>();
            vol.isGlobal = true;
            vol.priority = 100f;
            vol.sharedProfile = volProfile;
            Texture defRefl = ReflectionProbe.defaultTexture;
            L(label + " env=skybox+aces hdr=" + cam.allowHDR + " postfx=" + (camData != null && camData.renderPostProcessing) +
              " ambientSH=" + F(shMag) + " ambientMode=" + RenderSettings.ambientMode +
              " defaultRefl=" + (defRefl != null ? defRefl.name + "#" + defRefl.width : "NULL"));

            float radius = Mathf.Max(b.extents.magnitude, 0.5f);
            float dist = radius * 2.0f;
            cam.nearClipPlane = Mathf.Max(dist / 1000f, 0.01f);
            cam.farClipPlane = dist * 20f;

            rt = new RenderTexture(s_size, s_size, 24, RenderTextureFormat.ARGB32) { antiAliasing = 1 };
            tex = new Texture2D(s_size, s_size, TextureFormat.RGBA32, false);

            for (int i = 0; i < azimuths.Length; i++)
            {
                Vector3 dir = Quaternion.Euler(28f, azimuths[i], 0f) * Vector3.forward;
                camGo.transform.position = b.center - dir * dist;
                camGo.transform.LookAt(b.center);

                string path = "none";
                int nonClear = 0;
                var req = new RenderPipeline.StandardRequest { destination = rt };
                if (RenderPipeline.SupportsRenderRequest(cam, req))
                {
                    RenderPipeline.SubmitRenderRequest(cam, req);
                    nonClear = ReadBack(rt, tex);
                    path = "request";
                }
                if (nonClear == 0)
                {
                    cam.targetTexture = rt;
                    cam.Render();
                    cam.targetTexture = null;
                    nonClear = ReadBack(rt, tex);
                    path = "camRender";
                }
                File.WriteAllBytes(Path.Combine(outDir, label + suffixes[i] + ".png"), tex.EncodeToPNG());
                L(label + " wrote " + suffixes[i] + " path=" + path + " nonclear=" + nonClear);
            }
        }
        finally
        {
            RenderSettings.ambientMode = oldAmb;
            RenderSettings.ambientLight = oldAmbColor;
            RenderSettings.ambientSkyColor = oldAmbSky;
            RenderSettings.ambientEquatorColor = oldAmbEq;
            RenderSettings.ambientGroundColor = oldAmbGnd;
            RenderSettings.skybox = oldSkybox;
            RenderSettings.defaultReflectionMode = oldRefl;
            RenderSettings.customReflectionTexture = oldCustomRefl;
            if (tex != null) UnityEngine.Object.DestroyImmediate(tex);
            if (rt != null) { rt.Release(); UnityEngine.Object.DestroyImmediate(rt); }
            UnityEngine.Object.DestroyImmediate(camGo);
            UnityEngine.Object.DestroyImmediate(lightGo);
            UnityEngine.Object.DestroyImmediate(volGo);
            if (volProfile != null) UnityEngine.Object.DestroyImmediate(volProfile);
            if (skyMat != null) UnityEngine.Object.DestroyImmediate(skyMat);
            if (skyCube != null) UnityEngine.Object.DestroyImmediate(skyCube);
        }
    }

    static Cubemap BuildSkyCubemap(Vector3 sunDir)
    {
        const int N = 64;
        var cube = new Cubemap(N, TextureFormat.RGBAHalf, true) { name = "abvis-sky" };
        Color zen = new Color(0.35f, 0.52f, 0.85f);
        Color hor = new Color(0.78f, 0.82f, 0.90f);
        Color gnd = new Color(0.24f, 0.22f, 0.21f);
        Color sun = new Color(3.5f, 3.2f, 2.8f);
        sunDir.Normalize();
        var px = new Color[N * N];
        for (int f = 0; f < 6; f++)
        {
            for (int y = 0; y < N; y++)
            {
                float v = 2f * (y + 0.5f) / N - 1f;
                for (int x = 0; x < N; x++)
                {
                    float u = 2f * (x + 0.5f) / N - 1f;
                    Vector3 d;
                    switch (f)
                    {
                        case 0: d = new Vector3(1f, -v, -u); break;
                        case 1: d = new Vector3(-1f, -v, u); break;
                        case 2: d = new Vector3(u, 1f, v); break;
                        case 3: d = new Vector3(u, -1f, -v); break;
                        case 4: d = new Vector3(u, -v, 1f); break;
                        default: d = new Vector3(-u, -v, -1f); break;
                    }
                    d.Normalize();
                    Color c = d.y >= 0f
                        ? Color.Lerp(hor, zen, Mathf.Pow(d.y, 0.6f))
                        : Color.Lerp(hor, gnd, Mathf.Pow(-d.y, 0.5f));
                    c += sun * Mathf.Pow(Mathf.Max(0f, Vector3.Dot(d, sunDir)), 48f);
                    c.a = 1f;
                    px[y * N + x] = c;
                }
            }
            cube.SetPixels(px, (CubemapFace)f);
        }
        cube.Apply(true, false);
        return cube;
    }
}

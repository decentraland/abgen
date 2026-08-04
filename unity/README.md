# abgen in Unity

Asset-bundle conversion **inside the client** — glTF/GLB and textures in, Unity
AssetBundles out, with no Editor spawn, no sidecar process and no HTTP hop.

The library is Rust behind a plain C ABI (`crate/abgen-native`); the C# here is
a thin binding over it. Nothing engine-specific is baked into the library, so
the same binary backs the `abgen` server and the node addon.

## Layout

```
unity/
  build.sh                              build + deploy + codesign
  Packages/org.decentraland.abgen/
    package.json                        UPM manifest
    Runtime/
      Decentraland.Abgen.asmdef
      NativeMethods.cs                  raw DllImports
      AbgenRequest.cs                   request-blob builder
      AbgenConverter.cs                 in-process API
      AbgenHostProcess.cs               out-of-process API
      Plugins/
        macOS/libabgen.dylib
        Windows/x86_64/abgen.dll
        Linux/x86_64/libabgen.so
```

The `abgen-host` helper ships beside the library in the same release archive.

On Windows the `Plugins/Windows/x86_64/` directory needs the MinGW runtime
beside `abgen.dll` — `libstdc++-6.dll`, `libgcc_s_seh-1.dll` and
`libwinpthread-1.dll`. The library links draco's C++, so it imports them; a
host without them fails at `LoadLibrary` with nothing naming the cause. The
`abgen-native-*-x86_64-pc-windows-gnu` archive ships all four in `lib/`.

The managed assembly is `Decentraland.Abgen`, not `Abgen`, because
`DllImport("abgen")` resolves `abgen.dll` and Windows filenames are
case-insensitive: an `Abgen.dll` next to the native library is the same name,
and the runtime can bind the managed assembly instead of the native one.

## Platform floor

On Linux `libabgen.so` requires **glibc 2.34 or newer** — Ubuntu 22.04 LTS,
Debian 12, RHEL 9, Amazon Linux 2023 and the Steam Runtime 3 sniper container
all clear it, so the library is not stricter than Unity 6's own stated Linux
minimum. `ci/check-glibc-floor.sh` asserts this on every build; if you are
vendoring the library, keep that check in your pipeline, because the floor
drifts upward silently when a newer build host's headers redirect a libc call
to a newer entry point (see `crate/compat/isoc23_shim.c`).

The floor applies to the library specifically. `abgen-host` bundles its own
loader and glibc, so it has no host requirement — that is a thing an
out-of-process helper can do and a library dlopen'd into Unity cannot.

The `.meta` files pin each binary's platform in the plugin importer, so the
three never collide on "Any Platform". They are committed; the binaries are not
— populate them with `build.sh` or from a tagged release.

## Using it

```csharp
using Decentraland.Abgen;

if (!AbgenConverter.IsAbiCompatible())
    throw new Exception("abgen plugin ABI mismatch — rebuild the native library");

// Once, before the first conversion: left alone the native pool takes every
// core and competes with the render thread.
AbgenConverter.SetMaxThreads(4);

var request = new AbgenRequest { Platform = "windows", Mode = AbgenMode.Convert }
    .AddFile("model.glb", glbBytes)
    .AddFile("texture.png", pngBytes);

// Blocking and CPU-heavy — never call this on the main thread.
AbgenResult result = await Task.Run(() => AbgenConverter.Convert(request));

foreach (AbgenArtifact bundle in result.Artifacts)
    File.WriteAllBytes(Path.Combine(outDir, bundle.Name), bundle.Data);
```

`result.Events` carries the JSON progress stream (`file-start`, `file-done`,
`validate`, `lod-*`, …) and `result.Manifest` the final job manifest.

**A failed asset is not a failed run.** `AbgenStatus.Ok` means *the run*
completed; individual models can still have failed, appearing as `file-error`
entries in `Events` and a non-zero `exitCode` in `Manifest`. `result.Succeeded`
covers run-level success only.

## Building the plugin

```bash
unity/build.sh                        # host platform
unity/build.sh aarch64-apple-darwin   # explicit target
```

Builds `abgen-native` in release, drops the artifact in the matching `Plugins/`
folder, and on macOS ad-hoc code-signs it (mandatory on arm64 — the copy
invalidates the build's signature).

Cross-compiling needs the Rust target and a linker for it. The release CI
matrix provisions all six (`linux`, `windows`, `darwin` × `x86_64`/`aarch64`),
so pulling from a tagged release is usually easier.

There is **no accompanying runtime library set**: abgen links everything it
needs (Draco, libjpeg, crunch, texture codecs) statically, so the single file
is the whole plugin. `build.sh` verifies that per build rather than leaving a
player build to discover it.

## Two boundaries, pick per call

`AbgenConverter` converts **in this process**; `AbgenHostProcess` converts in a
**spawned child**, over the same core and returning the same `AbgenResult`:

```csharp
AbgenResult result = AbgenHostProcess.Convert(
    helperPath, request,
    maxMemoryMb: 2048,
    timeout: TimeSpan.FromMinutes(5));
```

Use the child for content you did not author: a glb that corrupts memory or
runs away with the heap kills the helper and returns a failed conversion, where
in-process it is your crash. Use `AbgenConverter` for trusted input — it saves
a spawn (single-digit ms against a conversion measured in seconds) and one copy
each way.

`maxMemoryMb` binds by different means per platform:

- **Linux** sets `RLIMIT_AS` then re-executes, so the cap precedes the
  allocator's reservations. Without the re-exec it is decorative — measured, an
  8 MB in-process cap still converts a glb, mimalloc having already taken its
  arenas by the time `main` runs.
- **Windows** assigns the process to a job object with
  `JOB_OBJECT_LIMIT_PROCESS_MEMORY`, which binds without the re-exec Windows
  cannot do anyway.
- **macOS** has no working mechanism, so the helper *refuses* the flag rather
  than capping nothing — measured, `RLIMIT_AS`/`DATA`/`RSS` set even from the
  parent at 256 MB all let a full conversion finish.

**The same number means very different things per platform.** A job object
counts *committed* memory; `RLIMIT_AS` counts *reserved address space*, which
an allocator inflates enormously. Measured on one small scene, Windows binds at
1-4 MB and passes from 8 MB up, while Linux needs gigabytes for identical work.
Do not carry a threshold across both. Exceeding the cap aborts the child, which
is the isolation working, and surfaces as a failed conversion.

Setting the cap also bounds the child's worker pool, since thread stacks count
against the same limit: rayon defaults to one worker per core, and on a
192-core machine those stacks alone exhaust an otherwise generous 4 GB.

## GPU

The BC7 encode path arms itself on first conversion and is used only when the
capability test passes — no call to make, no flag. It honours
`ABGEN_GPU_BACKEND=off`, applies abgen's measured macOS default (integrated
Metal loses to the CPU at BC7), and falls back to the CPU on a missing,
software, or divergent adapter.

In-process, that means the plugin opens its **own** wgpu device alongside
Unity's rather than sharing it. If the contention matters, convert through
`AbgenHostProcess` — the child gets its own device — or set
`ABGEN_GPU_BACKEND=off`.

## Safety posture

The in-process path runs untrusted content inside the client, so:

- **No panic crosses the ABI.** Every native entry point catches unwinds and
  reports `AbgenStatus.Panic`. A malformed glb produces a `file-error` event,
  not a crash.
- **No implicit panic sites** in the boundary crate: `unwrap`, `expect`,
  `panic!`, slice indexing and unchecked arithmetic are denied at compile time.
- **The managed callback cannot unwind either** — `AbgenConverter.OnEmit`
  catches everything, throwing into a native frame being undefined behaviour.
- **Threading is yours.** Nothing spawns a thread behind your back;
  `SetMaxThreads` bounds the only pool that exists.

What that is *not* is isolation. In-process, the converter shares the client's
address space, so a memory-safety bug in the Rust — or in the C libraries it
vendors (Draco, libjpeg, crunch), outside Rust's guarantees — is a bug in your
process. That is what `AbgenHostProcess` is for.

## IL2CPP

The native-to-managed callback is a static method carrying
`[MonoPInvokeCallback]`, and per-call state travels as a `GCHandle` through the
ABI's `user_data` rather than a closure. Under IL2CPP an instance delegate or a
captured lambda cannot be marshalled.

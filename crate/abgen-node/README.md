# @dcl/abgen-node

[abgen](https://github.com/decentraland/abgen)'s asset-bundle conversion as a
Node native addon: glTF/GLB and textures in, Unity AssetBundles out, in-process
— no sidecar, no port, no proxy.

```js
const { convert } = require('@dcl/abgen-node')

const { code, bundles, events, manifest } = await convert({
  files: [{ name: 'model.glb', data: glbBuffer }],
  platform: 'windows'
})
```

Conversion runs on the blocking pool rather than the libuv event loop; it is
CPU-bound for seconds at a time. A model that fails to convert is **not** a
rejected promise — it comes back as a `file-error` entry in `events` and a
non-zero `exitCode` in `manifest`, with `code` still 0.

`platform` is `"windows"`, `"mac"`, `"linux"` or `"webgl"`.

Prebuilt for linux x64/arm64, darwin x64/arm64 and win32 x64. Apache-2.0.

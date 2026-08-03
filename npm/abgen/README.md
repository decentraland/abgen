# @dcl/abgen

Prebuilt binaries for [abgen](https://github.com/decentraland/abgen): the standalone Decentraland
asset-bundle converter + ab-cdn-compatible JIT server. Installing this package pulls the binary for
your platform through an `optionalDependency`:

| Platform | Package |
|---|---|
| Linux x64 (glibc >= 2.35) | `@dcl/abgen-linux-x64` |
| Linux arm64 (glibc >= 2.35) | `@dcl/abgen-linux-arm64` |
| Windows x64 | `@dcl/abgen-win32-x64` |
| Windows arm64 | `@dcl/abgen-win32-arm64` |
| macOS arm64 (Apple Silicon) | `@dcl/abgen-darwin-arm64` |
| macOS x64 (Intel) | `@dcl/abgen-darwin-x64` |

Each platform package carries the binary plus the `template/` and `shader/` runtime assets it
resolves from its own directory - no configuration needed. The binaries are the same reproducible
artifacts attached to the GitHub release of the matching tag.

## Run the server

```bash
npx @dcl/abgen             # ab-cdn-compatible server on 127.0.0.1:5147
ABGEN_CATALYST_URL=https://peer.decentraland.org/content npx @dcl/abgen
```

All configuration is environment variables - see the
[repository README](https://github.com/decentraland/abgen#environment-variables).

## Embed in a tool

```js
const { binPath } = require('@dcl/abgen')
// spawn binPath() with the ABGEN_* environment you need
```

`binPath()` returns the absolute path of the platform binary, and throws with an actionable
message on unsupported platforms or when the optional platform package was omitted at install
time. `@dcl/sdk-commands` uses this to boot the asset-bundle preview sidecar.

## License

Apache-2.0 OR AGPL-3.0-or-later, at your option. Full texts and vendored third-party notices in the
[repository](https://github.com/decentraland/abgen).

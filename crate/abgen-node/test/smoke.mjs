import { createRequire } from 'node:module'
import { readFileSync, existsSync, copyFileSync, readdirSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { dirname, join } from 'node:path'

const require = createRequire(import.meta.url)
const here = dirname(fileURLToPath(import.meta.url))
const pkg = join(here, '..')

// `napi build --platform` drops a finished addon at the package root; a plain
// `cargo build` leaves a bare cdylib under target/. Prefer the former: it is the
// artifact we publish and users load, so the smoke exercises the real thing.
function resolveAddon(explicit) {
  if (explicit) return explicit
  const looked = []

  looked.push(pkg)
  const built = readdirSync(pkg).filter((f) => f.endsWith('.node')).sort()
  if (built.length) return join(pkg, built[0])

  // --target puts the cdylib under target/<triple>/release instead of target/release.
  const target = join(pkg, 'target')
  const relDirs = [join(target, 'release')]
  if (existsSync(target)) {
    for (const e of readdirSync(target, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
      if (e.isDirectory() && e.name !== 'release') relDirs.push(join(target, e.name, 'release'))
    }
  }
  const candidates = ['libabgen_node.so', 'libabgen_node.dylib', 'abgen_node.dll']
  for (const rel of relDirs) {
    looked.push(rel)
    for (const c of candidates) {
      const src = join(rel, c)
      if (existsSync(src)) {
        const dst = join(rel, 'abgen_node.node')
        copyFileSync(src, dst)
        return dst
      }
    }
  }

  throw new Error(`no abgen-node addon found — looked in:\n  ${looked.join('\n  ')}\nrun \`napi build --platform --release\` or \`cargo build --release\` first`)
}

const addon = require(resolveAddon(process.argv[2]))

console.log('exports:', Object.keys(addon).sort().join(', '))
console.log('version:', addon.version())

const glb = readFileSync(process.argv[3] ||
  join(here, '..', '..', 'abgen-wasm', 'test', 'fixtures', 'normal-quad.glb'))
const t0 = Date.now()
const res = await addon.convert({
  files: [{ name: 'model.glb', data: glb }],
  platform: 'windows'
})
const ms = Date.now() - t0

let fail = 0
const check = (c, what) => { console.log(`${c ? '  ok  ' : '  FAIL'} ${what}`); if (!c) fail++ }

check(res.code === 0, 'convert returned code 0')
check(res.errors.length === 0, `no fatal errors (${JSON.stringify(res.errors)})`)
check(res.bundles.length === 1, `one bundle (got ${res.bundles.length})`)
check(res.bundles[0]?.data?.length > 0, 'bundle carries bytes')
check(Buffer.isBuffer(res.bundles[0]?.data), 'bundle data is a Buffer')
check(res.events.length > 0, `progress events (${res.events.length})`)
check(/"exitCode":0/.test(res.manifest || ''), 'manifest exitCode 0')
check(/v-abgen-node/.test(res.manifest || ''), 'manifest identifies the node host')
console.log(`  bundle: ${res.bundles[0]?.name} (${res.bundles[0]?.data?.length} bytes) in ${ms}ms`)

const scan = await addon.convert({ files: [{ name: 'a.glb', data: glb }], platform: 'windows', mode: 1 })
check(scan.code === 0 && scan.bundles.length === 0, 'scan mode produces no bundles')

const bad = await addon.convert({ files: [{ name: 'evil.glb', data: Buffer.from([0xde, 0xad, 0xbe, 0xef]) }], platform: 'windows' })
check(bad.code === 0 && bad.bundles.length === 0, 'corrupt glb is reported, not thrown')

console.log(fail ? `\nFAILED (${fail})` : '\nPASS')
process.exit(fail ? 1 : 0)

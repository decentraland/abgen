import { createServer } from 'node:http';
import { spawn } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const generated = resolve(here, '../abgen-wasm/dist/gpu');
const jsName = 'abgen_wasm_gpu.js';
const wasmName = 'abgen_wasm_gpu_bg.wasm';
for (const name of [jsName, wasmName]) {
  if (!existsSync(join(generated, name))) {
    throw new Error(`missing ${join(generated, name)}; run crate/abgen-wasm/build.sh first`);
  }
}

let finish;
const result = new Promise((resolveResult) => { finish = resolveResult; });
const page = `<!doctype html><script type="module">
import init, { gpu_init, gpu_qualify_full } from '/${jsName}';
const progress = (phase) => fetch('/progress', {method: 'POST', body: phase});
await progress('module-loaded');
const started = performance.now();
try {
  await init();
  const adapter = await gpu_init();
  await progress('startup-qualified');
  const startupMs = performance.now() - started;
  const broadStarted = performance.now();
  await progress('broad-matrix-started');
  await gpu_qualify_full();
  await progress('broad-matrix-qualified');
  await fetch('/result', {method: 'POST', body: JSON.stringify({
    ok: true, qualified: true, failClosed: false, adapter, startupMs, broadMs: performance.now() - broadStarted
  })});
} catch (error) {
  const message = String(error && (error.stack || error.message) || error);
  const refusedSoftware = message.includes('software WebGPU adapter refused:');
  await fetch('/result', {method: 'POST', body: JSON.stringify({
    ok: refusedSoftware, qualified: false, failClosed: refusedSoftware, error: message
  })});
}
</script>`;

const server = createServer((request, response) => {
  if (request.method === 'POST' && request.url === '/progress') {
    const chunks = [];
    request.on('data', (chunk) => chunks.push(chunk));
    request.on('end', () => {
      console.error(`wasm-gpu: ${Buffer.concat(chunks).toString('utf8')}`);
      response.writeHead(204).end();
    });
    return;
  }
  if (request.method === 'POST' && request.url === '/result') {
    const chunks = [];
    request.on('data', (chunk) => chunks.push(chunk));
    request.on('end', () => {
      try {
        finish(JSON.parse(Buffer.concat(chunks).toString('utf8')));
        response.writeHead(204).end();
      } catch (error) {
        finish({ ok: false, error: String(error) });
        response.writeHead(400).end();
      }
    });
    return;
  }
  if (request.url === '/') {
    response.writeHead(200, {'Content-Type': 'text/html', 'Cache-Control': 'no-store'}).end(page);
    return;
  }
  const name = request.url?.slice(1);
  if (name === jsName || name === wasmName) {
    const type = name.endsWith('.wasm') ? 'application/wasm' : 'text/javascript';
    response.writeHead(200, {'Content-Type': type, 'Cache-Control': 'no-store'})
      .end(readFileSync(join(generated, name)));
    return;
  }
  response.writeHead(404).end();
});

await new Promise((resolveListen) => server.listen(0, '127.0.0.1', resolveListen));
const port = server.address().port;
const profile = mkdtempSync(join(tmpdir(), 'abgen-wasm-gpu-'));
const chromium = process.env.ABGEN_CHROMIUM || 'chromium';
const softwareFlags = process.env.ABGEN_WEBGPU_SOFTWARE === '1'
  ? ['--enable-features=Vulkan', '--use-vulkan=swiftshader']
  : [];
const child = spawn(chromium, [
  '--headless=new',
  '--no-sandbox',
  '--disable-dev-shm-usage',
  '--disable-background-networking',
  '--enable-unsafe-webgpu',
  ...softwareFlags,
  `--user-data-dir=${profile}`,
  `http://127.0.0.1:${port}/`,
], { stdio: ['ignore', 'ignore', 'pipe'] });
let stderr = '';
child.stderr.on('data', (chunk) => {
  stderr = (stderr + chunk.toString('utf8')).slice(-8192);
});
child.on('error', (error) => finish({ ok: false, error: `launch ${chromium}: ${error}` }));
child.on('exit', (code, signal) => {
  if (code && code !== 0) finish({ ok: false, error: `Chromium exited ${code}/${signal}\n${stderr}` });
});

const timeout = new Promise((resolveTimeout) => setTimeout(() => resolveTimeout({
  ok: false,
  error: `WebGPU qualification timed out\n${stderr}`,
}), Number(process.env.ABGEN_WASM_GPU_TIMEOUT_MS || 180000)));
const report = await Promise.race([result, timeout]);
if (!report.ok && stderr) report.browserStderr = stderr;
const waitForExit = (milliseconds) => new Promise((resolveExit) => {
  if (child.exitCode !== null || child.signalCode !== null) {
    resolveExit(true);
    return;
  }
  const timer = setTimeout(() => {
    child.removeListener('exit', exited);
    resolveExit(false);
  }, milliseconds);
  const exited = () => {
    clearTimeout(timer);
    resolveExit(true);
  };
  child.once('exit', exited);
});
if (!(await waitForExit(0))) {
  child.kill('SIGTERM');
  if (!(await waitForExit(5000))) {
    child.kill('SIGKILL');
    await waitForExit(5000);
  }
}
await new Promise((resolveClose) => server.close(resolveClose));
try {
  rmSync(profile, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 });
} catch (error) {
  console.error(`wasm-gpu: profile cleanup failed: ${error}`);
}
console.log(JSON.stringify(report));
if (!report.ok) process.exit(1);

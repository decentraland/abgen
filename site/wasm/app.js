import { readZip } from './zip.js';
import { runConvert } from './pool.js';

const $ = (id) => document.getElementById(id);
const drop = $('drop'), pick = $('pick'), table = $('filetable');
const feed = $('feed'), outputs = $('outputs'), sum = $('sum');
const convertBtn = $('convert');

let files = [];            // {name, data:ArrayBuffer}
let wasmModule = null;
let running = false;
let outputCount = 0, outputBytes = 0, t0 = 0;
const fileStart = new Map();

const fmt = (n) => n >= 1 << 20 ? (n / (1 << 20)).toFixed(2) + ' MB'
  : n >= 1024 ? (n / 1024).toFixed(1) + ' KB' : n + ' B';

const kindOf = (name) => {
  const n = name.toLowerCase();
  if (n.endsWith('.glb') || n.endsWith('.gltf')) return ['model', 'k-model'];
  if (/\.(png|jpe?g|webp|gif|bmp|tga|psd)$/.test(n)) return ['image', 'k-image'];
  if (n.endsWith('scene.json')) return ['scene.json', 'k-scene'];
  return ['file', 'k-other'];
};

function renderFiles() {
  table.innerHTML = '';
  for (const f of files) {
    const [k, cls] = kindOf(f.name);
    const tr = document.createElement('tr');
    tr.innerHTML = `<td><span class="kind ${cls}">${k}</span></td><td>${f.name}</td>` +
      `<td class="sz">${fmt(f.data.byteLength)}</td>`;
    table.appendChild(tr);
  }
  convertBtn.disabled = running || files.length === 0;
  $('ss0').innerHTML = files.length
    ? `<b>${files.length}</b> file(s), ${fmt(files.reduce((a, f) => a + f.data.byteLength, 0))}`
    : 'waiting for files';
}

async function addFiles(list) {
  for (const file of list) {
    const buf = await file.arrayBuffer();
    if (file.name.toLowerCase().endsWith('.zip')) {
      try {
        const entries = await readZip(buf);
        for (const e of entries) pushFile(e.name, e.data);
        log('note', `unpacked ${file.name}: ${entries.length} entries`);
      } catch (err) {
        log('err', `${file.name}: ${err.message}`);
      }
    } else {
      pushFile(file.name, buf);
    }
  }
  renderFiles();
}

function pushFile(name, data) {
  name = name.replace(/^\.\//, '');
  const i = files.findIndex((f) => f.name === name);
  if (i >= 0) files[i] = { name, data };
  else files.push({ name, data });
}

function log(cls, html) {
  const t = ((performance.now() - t0) / 1000).toFixed(1);
  const div = document.createElement('div');
  div.className = 'l-' + cls;
  div.innerHTML = (running ? `<span class="t">${t}s</span>` : '') + html;
  feed.appendChild(div);
  feed.scrollTop = feed.scrollHeight;
}

function groupValue(groupId) {
  return document.querySelector(`#${groupId} button.on`).dataset.v;
}

for (const grp of ['platgrp', 'etgrp']) {
  $(grp).addEventListener('click', (e) => {
    if (e.target.tagName !== 'BUTTON') return;
    for (const b of $(grp).querySelectorAll('button')) b.classList.remove('on');
    e.target.classList.add('on');
  });
}
$('magenta').addEventListener('click', () => $('magenta').classList.toggle('on'));
$('lodchip').addEventListener('click', () => $('lodchip').classList.toggle('on'));

drop.addEventListener('click', () => pick.click());
pick.addEventListener('change', () => addFiles(pick.files));
drop.addEventListener('dragover', (e) => { e.preventDefault(); drop.classList.add('over'); });
drop.addEventListener('dragleave', () => drop.classList.remove('over'));
drop.addEventListener('drop', (e) => {
  e.preventDefault();
  drop.classList.remove('over');
  addFiles(e.dataTransfer.files);
});

$('reset').addEventListener('click', () => {
  if (running) return;
  files = [];
  feed.innerHTML = ''; outputs.innerHTML = ''; sum.innerHTML = '';
  for (const i of [1, 2, 3]) $('ss' + i).textContent = '—';
  for (const i of [0, 1, 2, 3]) $('st' + i).classList.remove('live');
  renderFiles();
});

let plannedImages = 0, parsedFiles = 0, doneFiles = 0, modelCount = 0;

function onEvent(ev) {
  if (ev.ev === 'entity') {
    modelCount = ev.models;
    log('info', `entity <b>${ev.entityType}</b> · platform <b>${ev.platform}</b> · ` +
      `${ev.files} files, ${ev.models} model(s) · hash ${ev.entityHash.slice(0, 12)}…`);
    $('st1').classList.add('live');
  } else if (ev.ev === 'plan') {
    parsedFiles++; plannedImages += ev.images;
    $('ss1').innerHTML = `<b>${parsedFiles}</b>/${modelCount} parsed`;
    $('ss2').innerHTML = `<b>${plannedImages}</b> texture(s) queued`;
    log('info', `${ev.file}: ${ev.nodes} nodes, ${ev.materials} materials, ` +
      `${ev.images} images, ${ev.skins} skins`);
  } else if (ev.ev === 'file-start') {
    fileStart.set(ev.file, performance.now());
    $('st2').classList.add('live');
    log('start', `converting <b>${ev.file}</b> (${fmt(ev.bytes)}) → ${ev.bundle}`);
  } else if (ev.ev === 'file-done') {
    doneFiles++;
    const ms = performance.now() - (fileStart.get(ev.file) || t0);
    $('st3').classList.add('live');
    $('ss3').innerHTML = `<b>${doneFiles}</b>/${modelCount} bundle(s) written`;
    log('ok', `done <b>${ev.file}</b> in ${(ms / 1000).toFixed(2)}s → ${fmt(ev.bytes)} UnityFS`);
  } else if (ev.ev === 'file-error') {
    log('err', `${ev.file}: ${ev.error}`);
  } else if (ev.ev === 'validate') {
    const errs = ev.findings.filter((f) => f.severity === 'error');
    const warns = ev.findings.filter((f) => f.severity === 'warn');
    if (errs.length === 0) {
      log('ok', `validate ${ev.bundle.slice(0, 24)}…: structure clean` +
        (warns.length ? ` (${warns.length} warn)` : ''));
    } else {
      for (const f of errs) log('err', `validate ${f.code}: ${f.msg}`);
    }
    for (const f of warns) log('warn', `validate ${f.code}: ${f.msg}`);
  } else if (ev.ev === 'lod-start') {
    log('start', `LOD1: merging <b>${ev.models}</b> model(s), ${ev.tris} tris, ` +
      `${ev.parcels} parcel(s)`);
  } else if (ev.ev === 'lod-atlas') {
    log('info', `LOD1 atlas: ${ev.tris} tris, ${ev.materials} material(s), ` +
      `${ev.images} atlas page(s)`);
  } else if (ev.ev === 'gate') {
    if (ev.failures === 0) {
      log('ok', `LOD gate: ${ev.checks.length} checks pass ` +
        `(root GO, TexArray shader, square textures, naming)`);
    } else {
      for (const c of ev.checks.filter((c) => !c.ok)) {
        log('err', `LOD gate ${c.label}: ${c.detail}`);
      }
    }
  } else if (ev.ev === 'lod-done') {
    log('ok', `LOD1 bundle <b>${fmt(ev.bytes)}</b> → served as /${ev.servePath}`);
  } else if (ev.ev === 'note') {
    log('note', ev.msg);
  }
}

function addOutput(name, blob) {
  outputCount++; outputBytes += blob.size;
  const url = URL.createObjectURL(blob);
  const div = document.createElement('div');
  div.className = 'out';
  div.innerHTML = `<span class="nm">${name}</span><span class="sz">${fmt(blob.size)}</span>`;
  const a = document.createElement('a');
  a.href = url; a.download = name; a.textContent = 'download';
  div.appendChild(a);
  outputs.appendChild(div);
}

convertBtn.addEventListener('click', () => {
  if (running || files.length === 0) return;
  running = true;
  convertBtn.disabled = true;
  convertBtn.textContent = 'converting…';
  feed.innerHTML = ''; outputs.innerHTML = ''; sum.innerHTML = '';
  outputCount = 0; outputBytes = 0; plannedImages = 0; parsedFiles = 0; doneFiles = 0;
  t0 = performance.now();
  for (const i of [1, 2, 3]) $('ss' + i).textContent = '—';

  wasmModule ||= WebAssembly.compileStreaming(fetch('abgen_poc.wasm'));
  runConvert({
    files,
    module: wasmModule,
    platform: groupValue('platgrp'),
    entityType: groupValue('etgrp'),
    magenta: $('magenta').classList.contains('on'),
    lod: $('lodchip').classList.contains('on'),
  }, (m) => {
    if (m.type === 'ready') log('note', 'wasm module instantiated');
    else if (m.type === 'event') onEvent(m.data);
    else if (m.type === 'output') addOutput(m.name, new Blob([m.data], { type: 'application/wasm' }));
    else if (m.type === 'fatal') log('err', m.msg);
    else if (m.type === 'manifest') {
      const blob = new Blob([JSON.stringify(m.json, null, 2)], { type: 'application/json' });
      addOutput('manifest.json', blob);
      log('info', `manifest: ${m.json.files.length - 1} bundle(s), exitCode ${m.json.exitCode}`);
    } else if (m.type === 'done') {
      running = false;
      convertBtn.disabled = files.length === 0;
      convertBtn.textContent = 'convert';
      const secs = ((performance.now() - t0) / 1000).toFixed(2);
      sum.innerHTML = `<b>${outputCount}</b> artifact(s) · <b>${fmt(outputBytes)}</b> total · ` +
        `<b>${secs}s</b> wall, in this tab, <b>${m.workers}</b> workers`;
      log(m.code === 0 ? 'ok' : 'warn', `conversion finished (exit ${m.code}) in ${secs}s`);
    }
  });
});

renderFiles();

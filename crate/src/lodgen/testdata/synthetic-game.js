const engineApi = require('~system/EngineApi');

function putMessage(entity, component, timestamp, data) {
  const len = 24 + data.length;
  const buf = new ArrayBuffer(len);
  const v = new DataView(buf);
  v.setUint32(0, len, true);
  v.setUint32(4, 1, true);
  v.setUint32(8, entity, true);
  v.setUint32(12, component, true);
  v.setUint32(16, timestamp, true);
  v.setUint32(20, data.length, true);
  new Uint8Array(buf, 24).set(data);
  return new Uint8Array(buf);
}

function transformData() {
  const buf = new ArrayBuffer(44);
  const v = new DataView(buf);
  v.setFloat32(0, 8, true);
  v.setFloat32(4, 1, true);
  v.setFloat32(8, 4, true);
  v.setFloat32(24, 1, true);
  v.setFloat32(28, 2, true);
  v.setFloat32(32, 2, true);
  v.setFloat32(36, 2, true);
  return new Uint8Array(buf);
}

function gltfData(src) {
  const bytes = [0x0a, src.length];
  for (let i = 0; i < src.length; i++) bytes.push(src.charCodeAt(i));
  return new Uint8Array(bytes);
}

module.exports.onStart = async function () {
  await engineApi.crdtGetState();
  const t = putMessage(512, 1, 1, transformData());
  const g = putMessage(512, 1041, 1, gltfData('model.glb'));
  const joined = new Uint8Array(t.length + g.length);
  joined.set(t, 0);
  joined.set(g, t.length);
  await engineApi.crdtSendToRenderer({ data: joined });
};

module.exports.onUpdate = async function (_dt) {};

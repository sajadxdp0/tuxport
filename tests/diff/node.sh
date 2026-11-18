# Node.js: V8's JIT (repeated optimization and deoptimization), WebAssembly out-of-bounds access
# traps (V8 implements them with SIGSEGV and guard pages), worker_threads, child processes, HTTP
# over loopback, crypto and zlib
cd /tmp
cat > diff-node.js <<'JS'
const assert = require('assert');
const crypto = require('crypto');
const http = require('http');
const zlib = require('zlib');
const { execFileSync } = require('child_process');
const { Worker } = require('worker_threads');

// Polymorphic calls make V8 optimize, deoptimize and reoptimize the same function
function area(s) { return s.w !== undefined ? s.w * s.h : 3.14159 * s.r * s.r; }
let sum = 0;
for (let i = 0; i < 200000; i++) {
  sum += area(i % 3 === 0 ? { r: i % 10 } : { w: i % 7, h: i % 5 });
  if (i === 100000) sum += area({ w: 'x'.length, h: 2, extra: true });
}
console.log('jit', sum.toFixed(3));

// (module (memory 1) (func (export "load") (param i32) (result i32) local.get 0 i32.load))
const wasm = new Uint8Array([
  0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00,
  0x01, 0x06, 0x01, 0x60, 0x01, 0x7f, 0x01, 0x7f,
  0x03, 0x02, 0x01, 0x00,
  0x05, 0x03, 0x01, 0x00, 0x01,
  0x07, 0x08, 0x01, 0x04, 0x6c, 0x6f, 0x61, 0x64, 0x00, 0x00,
  0x0a, 0x09, 0x01, 0x07, 0x00, 0x20, 0x00, 0x28, 0x02, 0x00, 0x0b,
]);
// Must not be named exports: that clashes with a parameter of the CommonJS wrapper function, and
// Node would then load the file as an ES module instead
const { exports: mod } = new WebAssembly.Instance(new WebAssembly.Module(wasm));
console.log('wasm load', mod.load(0), mod.load(65532));
for (const addr of [65533, 65536, 1 << 30, -4]) {
  try {
    mod.load(addr);
    console.log('wasm', addr, 'no trap');
  } catch (e) {
    console.log('wasm', addr, e.constructor.name, e.message);
  }
}

console.log('sha256', crypto.createHash('sha256').update('tuxport').digest('hex'));
const key = Buffer.alloc(32, 7), iv = Buffer.alloc(16, 9);
const c = crypto.createCipheriv('aes-256-cbc', key, iv);
console.log('aes', Buffer.concat([c.update('hello world'), c.final()]).toString('hex'));
const big = Buffer.from('tuxport '.repeat(10000));
assert.deepStrictEqual(zlib.gunzipSync(zlib.gzipSync(big)), big);
console.log('zlib', zlib.deflateSync(big, { level: 9 }).length);
console.log('child', execFileSync('/bin/echo', ['from', 'a', 'child']).toString().trim());

function work(n) {
  return new Promise((resolve, reject) => {
    const w = new Worker(
      'const { parentPort, workerData } = require("worker_threads");' +
      'let s = 0; for (let i = 0; i < workerData; i++) s = (s + i * i) % 1000003;' +
      'parentPort.postMessage(s);',
      { eval: true, workerData: n });
    w.on('message', resolve);
    w.on('error', reject);
  });
}

(async () => {
  console.log('workers', await Promise.all([1e6, 2e6, 3e6, 4e6].map(work)));
  const server = http.createServer((req, res) => res.end('served ' + req.url));
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  const res = await fetch(`http://127.0.0.1:${server.address().port}/path?q=1`);
  console.log('http', res.status, await res.text());
  server.close();
})();
JS
node diff-node.js

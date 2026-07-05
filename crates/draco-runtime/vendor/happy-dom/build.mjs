import { rolldown } from 'rolldown';
import { fileURLToPath } from 'node:url';
const d = fileURLToPath(new URL('.', import.meta.url));
const stub = d + 'stub-empty.js';
const A = (f) => d + f;
const alias = {
  url: A('stub-url.js'), 'node:url': A('stub-url.js'),
  util: A('stub-util.js'), 'node:util': A('stub-util.js'),
  buffer: A('stub-buffer.js'), 'node:buffer': A('stub-buffer.js'),
  crypto: A('stub-crypto.js'), 'node:crypto': A('stub-crypto.js'),
  perf_hooks: A('stub-perf.js'), 'node:perf_hooks': A('stub-perf.js'),
  vm: A('stub-vm.js'), 'node:vm': A('stub-vm.js'),
  net: A('stub-net.js'), 'node:net': A('stub-net.js'),
  'stream/web': A('stub-streams.js'), 'node:stream/web': A('stub-streams.js'),
  fs: stub, 'node:fs': stub, path: stub, 'node:path': stub, http: stub, 'node:http': stub,
  https: stub, 'node:https': stub, zlib: stub, 'node:zlib': stub, child_process: stub, 'node:child_process': stub,
  stream: stub, 'node:stream': stub, os: stub, 'node:os': stub, tty: stub, 'node:tty': stub,
};
// base bundle (no aliases needed; it only uses npm polyfills)
let b = await rolldown({ input: 'base-entry.js', platform: 'browser' });
await b.write({ file: '../../js/base.iife.js', format: 'iife', name: 'BASE', inlineDynamicImports: true });
// happy-dom bundle
b = await rolldown({ input: 'entry.js', platform: 'browser', resolve: { alias } });
await b.write({ file: '../../js/happydom.iife.js', format: 'iife', name: 'HD', inlineDynamicImports: true });
console.log('built base + happydom');

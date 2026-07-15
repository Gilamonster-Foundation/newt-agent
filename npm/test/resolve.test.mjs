import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, mkdtempSync, mkdirSync, writeFileSync, cpSync, chmodSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';
import { spawnSync } from 'node:child_process';
import { tmpdir } from 'node:os';

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, '..');
const require = createRequire(import.meta.url);

const SHIMS = ['newt', 'newt-mcp-server'];
const platforms = JSON.parse(readFileSync(join(root, 'platforms.json'), 'utf8'));
const umbrella = JSON.parse(readFileSync(join(root, 'newt-agent', 'package.json'), 'utf8'));

const NODE_OS = new Set(['darwin', 'linux', 'win32', 'freebsd', 'openbsd', 'sunos', 'aix', 'android']);
const NODE_CPU = new Set(['arm64', 'x64', 'ia32', 'arm', 'ppc64', 's390x', 'riscv64', 'loong64']);

test('platforms.json entries are well-formed and unique', () => {
  const keys = new Set();
  for (const p of platforms) {
    for (const f of ['key', 'os', 'cpu', 'rustTarget']) {
      assert.ok(p[f], `entry missing "${f}": ${JSON.stringify(p)}`);
    }
    assert.ok(NODE_OS.has(p.os), `invalid node os: ${p.os}`);
    assert.ok(NODE_CPU.has(p.cpu), `invalid node cpu: ${p.cpu}`);
    if (p.libc) assert.ok(['glibc', 'musl'].includes(p.libc), `invalid libc: ${p.libc}`);
    assert.equal(p.key, `${p.os}-${p.cpu}`, `key must equal "<os>-<cpu>": ${p.key}`);
    assert.ok(!keys.has(p.key), `duplicate platform key: ${p.key}`);
    keys.add(p.key);
  }
});

test('each shim optionalDependencies exactly cover platforms.json, keyed by the shim name', () => {
  for (const shim of SHIMS) {
    const pkg = JSON.parse(readFileSync(join(root, shim, 'package.json'), 'utf8'));
    const expected = platforms.map((p) => `${pkg.name}-${p.key}`).sort();
    const actual = Object.keys(pkg.optionalDependencies || {}).sort();
    assert.deepEqual(actual, expected, `${pkg.name} optionalDependencies must list every platform`);
    // bin key must be the binary name (last segment of the scoped package name)
    const binName = pkg.name.split('/').pop();
    assert.equal(pkg.bin[binName], 'bin/run.cjs', `${pkg.name} must expose bin.${binName}`);
    assert.equal(pkg.publishConfig.access, 'public');
  }
});

test('umbrella newt-agent depends on @gilamonster/newt and exposes the newt bin', () => {
  assert.ok(umbrella.dependencies['@gilamonster/newt'], 'newt-agent must depend on @gilamonster/newt');
  assert.equal(umbrella.bin.newt, 'bin/run.cjs');
});

test('resolver derives the binary from its own name and throws an actionable error when absent', () => {
  const { binaryPath, BINARY, platformKey } = require(join(root, 'newt', 'lib', 'binary.cjs'));
  assert.equal(BINARY, 'newt', 'BINARY must derive from the shim package name');
  assert.match(platformKey(), /^[a-z0-9]+-[a-z0-9]+$/);
  assert.throws(
    () => binaryPath(),
    (err) => {
      assert.match(err.message, /newt-agent|Gilamonster-Foundation\/newt-agent|--include=optional/);
      return true;
    }
  );
});

test('happy path: the newt shim resolves and execs an installed platform package', () => {
  const { entryForCurrentPlatform } = require(join(root, 'newt', 'lib', 'binary.cjs'));
  const entry = entryForCurrentPlatform();
  if (!entry) return;
  if (entry.os === 'win32') return; // fake .exe isn't a real PE

  const tmp = mkdtempSync(join(tmpdir(), 'newt-npm-'));
  const scoped = join(tmp, 'node_modules', '@gilamonster');
  cpSync(join(root, 'newt'), join(scoped, 'newt'), { recursive: true });

  const platDir = join(scoped, `newt-${entry.key}`);
  mkdirSync(platDir, { recursive: true });
  writeFileSync(
    join(platDir, 'package.json'),
    JSON.stringify({ name: `@gilamonster/newt-${entry.key}`, version: '0.0.0', os: [entry.os], cpu: [entry.cpu] })
  );
  const fakeBin = join(platDir, 'newt');
  writeFileSync(fakeBin, '#!/bin/sh\necho newt-fake-ok "$@"\n');
  chmodSync(fakeBin, 0o755);

  const shim = join(scoped, 'newt', 'bin', 'run.cjs');
  const res = spawnSync(process.execPath, [shim, 'run', '--flag'], { encoding: 'utf8' });
  assert.equal(res.status, 0, res.stderr);
  assert.match(res.stdout, /newt-fake-ok run --flag/, 'shim must exec the resolved binary with argv passed through');
});

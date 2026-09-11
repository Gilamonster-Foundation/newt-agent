import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, mkdtempSync, mkdirSync, writeFileSync, cpSync, copyFileSync, chmodSync, rmSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';
import { spawn, spawnSync } from 'node:child_process';
import { once } from 'node:events';
import { tmpdir } from 'node:os';
import { runInNewContext } from 'node:vm';

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, '..');
const require = createRequire(import.meta.url);

const SHIMS = ['newt', 'newt-mcp-server'];
const platforms = JSON.parse(readFileSync(join(root, 'platforms.json'), 'utf8'));
const umbrella = JSON.parse(readFileSync(join(root, 'newt-agent', 'package.json'), 'utf8'));
const currentPlatform = platforms.find((p) => p.key === `${process.platform}-${process.arch}`);
const skipPosixFixture = !currentPlatform || process.platform === 'win32';

function installedShim(t, shim, script) {
  const tmp = mkdtempSync(join(tmpdir(), 'newt-npm-'));
  t.after(() => rmSync(tmp, { recursive: true, force: true }));
  const scoped = join(tmp, 'node_modules', '@gilamonster');
  const binaryShim = shim === 'newt-agent' ? 'newt' : shim;
  cpSync(join(root, binaryShim), join(scoped, binaryShim), { recursive: true });
  const platDir = join(scoped, `${binaryShim}-${currentPlatform.key}`);
  mkdirSync(platDir, { recursive: true });
  writeFileSync(join(platDir, 'package.json'), JSON.stringify({ name: `@gilamonster/${binaryShim}-${currentPlatform.key}` }));
  const fakeBin = join(platDir, `${binaryShim}${process.platform === 'win32' ? '.exe' : ''}`);
  if (script === undefined) copyFileSync(process.execPath, fakeBin);
  else writeFileSync(fakeBin, script);
  chmodSync(fakeBin, 0o755);
  if (shim === 'newt-agent') {
    const umbrellaDir = join(tmp, 'node_modules', shim);
    cpSync(join(root, shim), umbrellaDir, { recursive: true });
    return join(umbrellaDir, 'bin', 'run.cjs');
  }
  return join(scoped, shim, 'bin', 'run.cjs');
}

function resolverForHost(shim, host) {
  const file = join(root, shim, 'lib', 'binary.cjs');
  const module = { exports: {} };
  runInNewContext(readFileSync(file, 'utf8'), {
    module, require: createRequire(file), __dirname: dirname(file), process: host,
  });
  return module.exports;
}

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

test('shims share identical launchers, resolvers, and the release platform matrix', () => {
  for (const file of ['bin/run.cjs', 'lib/binary.cjs']) {
    assert.equal(readFileSync(join(root, SHIMS[0], file), 'utf8'), readFileSync(join(root, SHIMS[1], file), 'utf8'), file);
  }
  for (const shim of SHIMS) {
    assert.equal(readFileSync(join(root, shim, 'platforms.json'), 'utf8'), readFileSync(join(root, 'platforms.json'), 'utf8'), shim);
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

for (const shim of SHIMS) {
  test(`${shim}: Windows leaves console signals to the native child`, () => {
    const handlers = new Map();
    const kills = [];
    const child = { on() {}, kill: (signal) => kills.push(signal) };
    runInNewContext(readFileSync(join(root, shim, 'bin', 'run.cjs'), 'utf8'), {
      process: { platform: 'win32', argv: ['node', 'shim'], on: (signal, handler) => handlers.set(signal, handler) },
      require: (name) => {
        if (name === 'child_process') return { spawn: () => child };
        if (name === '../lib/binary.cjs') return { binaryPath: () => 'fixture.exe', BINARY: shim };
        return require(name);
      },
    });
    for (const handler of handlers.values()) handler();
    assert.deepEqual([...handlers.keys()], [], 'Windows must not install POSIX signal forwarding');
    assert.deepEqual(kills, [], 'Windows console signals must not become forceful child.kill calls');
  });

  test(`${shim}: unsupported platforms refuse with source-install guidance`, () => {
    const { binaryPath } = resolverForHost(shim, { platform: 'freebsd', arch: 'arm64' });
    assert.throws(() => binaryPath(), /no prebuilt binary.*freebsd-arm64[\s\S]*Supported:[\s\S]*Install from source/);
  });

  test(`${shim}: glibc-only Linux binaries refuse an unverified libc`, () => {
    const { binaryPath } = resolverForHost(shim, {
      platform: 'linux', arch: 'x64', report: { getReport: () => ({ header: {} }) },
    });
    assert.throws(() => binaryPath(), /requires glibc[\s\S]*Install from source/);
  });

  test(`${shim}: missing-package guidance installs the matching shim`, { skip: !currentPlatform }, () => {
    const { binaryPath } = require(join(root, shim, 'lib', 'binary.cjs'));
    assert.throws(() => binaryPath(), (err) => {
      assert.ok(err.message.includes(`npm install -g @gilamonster/${shim} --include=optional`));
      return true;
    });
  });

  // Real filesystem deletion grounds the launcher's spawn-error handling after
  // the resolver successfully found an installed platform package.
  test(`${shim}: a missing executable reports the launch failure`, { skip: !currentPlatform }, (t) => {
    const launcher = installedShim(t, shim, '');
    const { binaryPath } = require(join(dirname(dirname(launcher)), 'lib', 'binary.cjs'));
    rmSync(binaryPath());
    const res = spawnSync(process.execPath, [launcher], { encoding: 'utf8' });
    assert.equal(res.error, undefined);
    assert.equal(res.status, 1);
    assert.equal(res.stdout, '');
    assert.match(res.stderr, /failed to launch.*ENOENT/);
  });

  // A real subprocess grounds the resolver's package selection and the launcher's
  // claimed signal forwarding. The ready message is the barrier before signalling.
  test(`${shim}: a signal sent to the launcher reaches its child`, {
    skip: skipPosixFixture,
    timeout: 5000,
  }, async (t) => {
    const launcher = installedShim(t, shim, '#!/usr/bin/env node\n' +
      "process.on('SIGTERM', () => process.stdout.write('forwarded\\n', () => process.exit(42)));\n" +
      "process.stdout.write(`ready:${process.pid}\\n`);\nsetInterval(() => {}, 60000);\n");
    const child = spawn(process.execPath, [launcher], { stdio: ['ignore', 'pipe', 'pipe'] });
    const exited = once(child, 'exit');
    const closed = once(child, 'close');
    let output = '';
    let binaryPid;
    t.after(() => {
      child.kill('SIGKILL');
      if (binaryPid) {
        try { process.kill(binaryPid, 'SIGKILL'); } catch (err) {
          if (err.code !== 'ESRCH') throw err;
        }
      }
    });
    await new Promise((resolve, reject) => {
      child.on('error', reject);
      child.stdout.on('data', (chunk) => {
        output += chunk;
        const ready = output.match(/ready:(\d+)\n/);
        if (ready) {
          binaryPid = Number(ready[1]);
          resolve();
        }
      });
      child.once('exit', () => reject(new Error('launcher exited before child readiness')));
    });
    child.kill('SIGTERM');
    const [status, signal] = await exited;
    assert.equal(signal, null, 'launcher must wait for the child to handle the forwarded signal');
    assert.equal(status, 42, 'launcher must preserve the child exit status');
    binaryPid = undefined;
    await closed;
    assert.match(output, /forwarded\n/);
  });
}

for (const shim of [...SHIMS, 'newt-agent']) {
  // A copied native executable grounds package/bin metadata and stdio forwarding
  // on Windows as well as POSIX; only the separate signal test needs a script.
  test(`${shim}: native executable receives argv and stdio and preserves exit status`, { skip: !currentPlatform }, (t) => {
    const launcher = installedShim(t, shim);
    const args = ['argument with spaces', '--flag', ';not-a-shell'];
    for (const status of [0, 23]) {
      const code = "process.stdout.write(JSON.stringify({ args: process.argv.slice(1), input: require('fs').readFileSync(0, 'utf8') }));" +
        `process.stderr.write('fixture-stderr\\n'); process.exitCode = ${status};`;
      const res = spawnSync(process.execPath, [launcher, '-e', code, '--', ...args], {
        encoding: 'utf8', input: 'fixture-input\n',
      });
      assert.equal(res.error, undefined);
      assert.equal(res.status, status, res.stderr);
      assert.equal(res.signal, null);
      assert.deepEqual(JSON.parse(res.stdout), { args, input: 'fixture-input\n' });
      assert.equal(res.stderr, 'fixture-stderr\n');
    }
  });
}

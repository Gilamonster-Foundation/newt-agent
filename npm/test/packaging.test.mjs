import test from 'node:test';
import assert from 'node:assert/strict';
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

const npmRoot = join(dirname(fileURLToPath(import.meta.url)), '..');
const shims = ['newt', 'newt-mcp-server', 'newt-agent'];

function fixture(t) {
  const root = mkdtempSync(join(tmpdir(), 'newt-packaging-'));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  mkdirSync(join(root, 'npm'));
  for (const path of ['scripts', 'platforms.json', ...shims]) {
    cpSync(join(npmRoot, path), join(root, 'npm', path), { recursive: true });
  }
  writeFileSync(join(root, 'Cargo.toml'), '[package]\nversion = "9.9.9"\n\n[workspace.package]\nversion = "1.2.3"\n');
  writeFileSync(join(root, 'LICENSE'), 'Apache-2.0 license fixture\n');
  writeFileSync(join(root, 'binary'), 'release binary fixture\n');
  return root;
}

function script(root, name, args) {
  return spawnSync(process.execPath, [join(root, 'npm', 'scripts', name), ...args], { encoding: 'utf8' });
}

function manifests(root) {
  return shims.map((name) => readFileSync(join(root, 'npm', name, 'package.json'), 'utf8'));
}

function build(root, { version = '1.2.3', key = 'linux-x64', name = 'newt', binary = 'binary', out = 'out' } = {}) {
  return script(root, 'build-platform-package.mjs', [
    '--name', `@gilamonster/${name}`, '--binary', join(root, binary),
    '--key', key, '--version', version, '--out', join(root, out),
  ]);
}

// These real subprocess/filesystem checks ground the manifest-only unit tests:
// the actual release scripts must refuse a mismatched tag before changing files.
test('version skew refuses both release scripts without modifying packages', (t) => {
  const root = fixture(t);
  const before = manifests(root);
  const sync = script(root, 'sync-versions.mjs', ['--version', '9.9.9']);
  assert.notEqual(sync.status, 0);
  assert.match(sync.stderr, /workspace.*1\.2\.3|1\.2\.3.*workspace/);
  assert.deepEqual(manifests(root), before);
  const packed = build(root, { version: '9.9.9' });
  assert.notEqual(packed.status, 0);
  assert.match(packed.stderr, /workspace.*1\.2\.3|1\.2\.3.*workspace/);
  assert.equal(existsSync(join(root, 'out')), false);
});

test('version preflight is read-only and successful sync pins every package', (t) => {
  const root = fixture(t);
  const before = manifests(root);
  const checked = script(root, 'sync-versions.mjs', ['--version', '1.2.3', '--check']);
  assert.equal(checked.status, 0, checked.stderr);
  assert.deepEqual(manifests(root), before);
  const synced = script(root, 'sync-versions.mjs', ['--version', '1.2.3']);
  assert.equal(synced.status, 0, synced.stderr);
  for (const source of manifests(root)) {
    const pkg = JSON.parse(source);
    assert.equal(pkg.version, '1.2.3');
    for (const pin of Object.values(pkg.optionalDependencies ?? pkg.dependencies)) {
      assert.equal(pin, '1.2.3');
    }
  }
});

test('missing, malformed, and build-metadata versions fail before mutation', (t) => {
  const root = fixture(t);
  const before = manifests(root);
  for (const version of ['', 'v1.2.3', '1.2', '01.2.3', '1.2.3-01', '1.2.3+build']) {
    writeFileSync(join(root, 'Cargo.toml'), `[workspace.package]\nversion = "${version}"\n`);
    const sync = script(root, 'sync-versions.mjs', version ? ['--version', version] : []);
    assert.notEqual(sync.status, 0, `must reject ${JSON.stringify(version)}`);
    assert.deepEqual(manifests(root), before);
    const packed = build(root, { version });
    assert.notEqual(packed.status, 0, `must reject ${JSON.stringify(version)}`);
    assert.equal(existsSync(join(root, 'out')), false);
  }
});

test('matching prerelease versions are accepted without changing checked-in manifests', (t) => {
  const root = fixture(t);
  const before = manifests(root);
  writeFileSync(join(root, 'Cargo.toml'), '[workspace.package]\r\nversion = "1.2.3-rc.1"\r\n');
  const checked = script(root, 'sync-versions.mjs', ['--version', '1.2.3-rc.1', '--check']);
  assert.equal(checked.status, 0, checked.stderr);
  assert.deepEqual(manifests(root), before);
});

test('missing, empty, and non-file binaries refuse packaging before creating output', (t) => {
  const root = fixture(t);
  writeFileSync(join(root, 'empty'), '');
  mkdirSync(join(root, 'directory'));
  for (const binary of ['missing', 'empty', 'directory']) {
    const result = build(root, { binary });
    assert.notEqual(result.status, 0, `must reject ${binary}`);
    assert.equal(existsSync(join(root, 'out')), false);
  }
});

// npm pack grounds the platform manifest/files allowlist against npm's real
// tarball construction; fixture bytes avoid requiring native release binaries.
// Run through npm test: npm_execpath names npm's JavaScript entry point on every
// host, avoiding Windows npm.cmd, which spawnSync cannot execute directly.
test('every platform package packs the release bytes and license', async (t) => {
  assert.ok(process.env.npm_execpath, 'Run packaging tests through npm test; npm_execpath must identify the npm CLI');
  const root = fixture(t);
  const platforms = JSON.parse(readFileSync(join(root, 'npm', 'platforms.json'), 'utf8'));
  for (const platform of platforms) {
    for (const name of ['newt', 'newt-mcp-server']) {
      const out = `${name}-${platform.key}`;
      const result = build(root, { key: platform.key, name, out });
      assert.equal(result.status, 0, result.stderr);
      const pkg = JSON.parse(readFileSync(join(root, out, 'package.json'), 'utf8'));
      assert.equal(pkg.name, `@gilamonster/${out}`);
      assert.equal(pkg.version, '1.2.3');
      assert.deepEqual(pkg.os, [platform.os]);
      assert.deepEqual(pkg.cpu, [platform.cpu]);
      assert.deepEqual(pkg.libc, platform.libc ? [platform.libc] : undefined);
      const filename = name + (platform.os === 'win32' ? '.exe' : '');
      assert.deepEqual(readFileSync(join(root, out, filename)), readFileSync(join(root, 'binary')));
      const pack = spawnSync(process.execPath, [process.env.npm_execpath, 'pack', '--json', '--ignore-scripts'], {
        cwd: join(root, out), encoding: 'utf8',
      });
      assert.equal(pack.status, 0, pack.stderr);
      const [tarball] = JSON.parse(pack.stdout);
      assert.deepEqual(tarball.files.map((file) => file.path).sort(), ['LICENSE', 'README.md', filename, 'package.json'].sort());
      assert.equal(readFileSync(join(root, out, 'LICENSE'), 'utf8'), readFileSync(join(root, 'LICENSE'), 'utf8'));
      if (platform.os !== 'win32') {
        await t.test(`${out}: Unix executable mode survives npm pack`, {
          skip: process.platform === 'win32' ? 'Windows chmod cannot establish Unix modes; release packaging runs on Ubuntu' : false,
        }, () => {
          assert.equal(tarball.files.find((file) => file.path === filename).mode & 0o111, 0o111);
        });
      }
    }
  }
});

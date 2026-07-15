#!/usr/bin/env node
// Wrap a built binary into a publishable `@gilamonster/<bin>-<platform>` package.
//
//   node scripts/build-platform-package.mjs \
//     --name @gilamonster/newt \
//     --binary target/x86_64-unknown-linux-gnu/release/newt \
//     --key linux-x64 --version 0.6.0 --out dist-npm/newt-linux-x64
//
// Generic over the binary — `--name` is the shim package, the binary file name is
// its last path segment. Reads npm/platforms.json (single source of truth).

import { readFileSync, writeFileSync, mkdirSync, copyFileSync, chmodSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const PLATFORMS = JSON.parse(readFileSync(join(here, '..', 'platforms.json'), 'utf8'));

function arg(name) {
  const i = process.argv.indexOf(`--${name}`);
  if (i === -1 || i === process.argv.length - 1) {
    console.error(`build-platform-package: missing --${name}`);
    process.exit(2);
  }
  return process.argv[i + 1];
}

const shim = arg('name'); // @gilamonster/newt
const binary = arg('binary');
const key = arg('key');
const version = arg('version');
const out = resolve(arg('out'));

const entry = PLATFORMS.find((p) => p.key === key);
if (!entry) {
  console.error(`build-platform-package: unknown platform key "${key}"`);
  process.exit(2);
}

const binName = shim.split('/').pop();
const fileName = entry.os === 'win32' ? `${binName}.exe` : binName;

mkdirSync(out, { recursive: true });

const pkg = {
  name: `${shim}-${entry.key}`,
  version,
  description: `Prebuilt ${binName} binary for ${entry.os}-${entry.cpu}.`,
  homepage: 'https://github.com/Gilamonster-Foundation/newt-agent#readme',
  repository: { type: 'git', url: 'git+https://github.com/Gilamonster-Foundation/newt-agent.git' },
  license: 'Apache-2.0',
  os: [entry.os],
  cpu: [entry.cpu],
  ...(entry.libc ? { libc: [entry.libc] } : {}),
  files: [fileName, 'README.md'],
  publishConfig: { access: 'public' },
};

writeFileSync(join(out, 'package.json'), JSON.stringify(pkg, null, 2) + '\n');
copyFileSync(binary, join(out, fileName));
if (entry.os !== 'win32') chmodSync(join(out, fileName), 0o755);
writeFileSync(
  join(out, 'README.md'),
  `# ${pkg.name}\n\nPrebuilt \`${binName}\` binary for ${entry.os}-${entry.cpu}. Installed automatically ` +
    `as an optional dependency of [\`${shim}\`](https://www.npmjs.com/package/${shim}); do not depend on it directly.\n`
);

console.log(`built ${pkg.name}@${version} -> ${out}`);

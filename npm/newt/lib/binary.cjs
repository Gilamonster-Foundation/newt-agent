'use strict';

// Generic resolver for a `@gilamonster/<bin>` shim. Everything is derived from
// this package's OWN name, so the exact same file is shipped verbatim by every
// binary shim (newt, newt-mcp-server, …):
//
//   @gilamonster/newt              -> binary "newt",  platform pkgs @gilamonster/newt-<platform>
//   @gilamonster/newt-mcp-server   -> binary "newt-mcp-server", @gilamonster/newt-mcp-server-<platform>
//
// The uv / esbuild optionalDependencies pattern — no postinstall, no network.

const fs = require('fs');
const path = require('path');

const self = require('../package.json');
const SHIM = self.name; // e.g. "@gilamonster/newt-mcp-server"
const BINARY = SHIM.split('/').pop(); // "newt-mcp-server"
const PLATFORMS = JSON.parse(fs.readFileSync(path.join(__dirname, '..', 'platforms.json'), 'utf8'));

const REPO = 'https://github.com/Gilamonster-Foundation/newt-agent';

function platformKey() {
  return `${process.platform}-${process.arch}`;
}

function entryForCurrentPlatform() {
  const key = platformKey();
  return PLATFORMS.find((p) => p.key === key) || null;
}

function binaryFile(entry) {
  return entry.os === 'win32' ? `${BINARY}.exe` : BINARY;
}

// "2.36" >= "2.39"? Numeric per component; a malformed runtime string counts
// as too old, never as new enough.
function glibcAtLeast(runtime, min) {
  const parse = (v) => String(v).split('.').map((n) => Number.parseInt(n, 10));
  const [r, m] = [parse(runtime), parse(min)];
  for (let i = 0; i < Math.max(r.length, m.length); i += 1) {
    const a = Number.isNaN(r[i]) ? -1 : (r[i] ?? 0);
    const b = m[i] ?? 0;
    if (a !== b) return a > b;
  }
  return true;
}

function binaryPath() {
  const key = platformKey();
  const entry = entryForCurrentPlatform();

  if (!entry) {
    const supported = PLATFORMS.map((p) => p.key).join(', ');
    throw new Error(
      `${BINARY}: no prebuilt binary for this platform (${key}).\n` +
        `Supported: ${supported}.\n` +
        `Install from source instead:  ${REPO}`
    );
  }

  if (entry.libc === 'glibc') {
    const runtime = process.report?.getReport?.()?.header?.glibcVersionRuntime;
    if (!runtime) {
      throw new Error(
        `${BINARY}: this Linux binary requires glibc; this Node runtime did not report glibc.\n` +
          `Install from source instead:  ${REPO}`
      );
    }
    // The prebuilt binary is linked against the glibc of the release builder
    // (platforms.json `glibcMin`). Older hosts would install cleanly and then
    // die in ld.so with "version GLIBC_x.y not found"; say so up front.
    if (entry.glibcMin && !glibcAtLeast(runtime, entry.glibcMin)) {
      throw new Error(
        `${BINARY}: this Linux binary requires glibc >= ${entry.glibcMin}; this host has glibc ${runtime}.\n` +
          `Install from source instead:  ${REPO}`
      );
    }
  }

  const pkg = `${SHIM}-${entry.key}`;
  try {
    const pkgJsonPath = require.resolve(`${pkg}/package.json`);
    return path.join(path.dirname(pkgJsonPath), binaryFile(entry));
  } catch (_err) {
    throw new Error(
      `${BINARY}: the platform package "${pkg}" is not installed.\n` +
        `This usually means optionalDependencies were skipped during install.\n` +
        `Try:   npm install -g ${SHIM} --include=optional\n` +
        `Or install from source:  ${REPO}`
    );
  }
}

module.exports = { PLATFORMS, BINARY, SHIM, platformKey, entryForCurrentPlatform, binaryPath };

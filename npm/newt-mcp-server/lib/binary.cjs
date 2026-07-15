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

  const pkg = `${SHIM}-${entry.key}`;
  try {
    const pkgJsonPath = require.resolve(`${pkg}/package.json`);
    return path.join(path.dirname(pkgJsonPath), binaryFile(entry));
  } catch (_err) {
    throw new Error(
      `${BINARY}: the platform package "${pkg}" is not installed.\n` +
        `This usually means optionalDependencies were skipped during install.\n` +
        `Try:   npm install -g newt-agent --include=optional\n` +
        `Or install from source:  ${REPO}`
    );
  }
}

module.exports = { PLATFORMS, BINARY, SHIM, platformKey, entryForCurrentPlatform, binaryPath };

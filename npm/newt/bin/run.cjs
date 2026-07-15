#!/usr/bin/env node
'use strict';

// Generic launcher shared by every @gilamonster/<bin> shim: resolve the platform
// binary and exec it, passing through argv, stdio, exit code, and signals.

const os = require('os');
const { spawnSync } = require('child_process');
const { binaryPath, BINARY } = require('../lib/binary.cjs');

let bin;
try {
  bin = binaryPath();
} catch (err) {
  process.stderr.write(`${err && err.message ? err.message : err}\n`);
  process.exit(1);
}

const result = spawnSync(bin, process.argv.slice(2), { stdio: 'inherit' });

if (result.error) {
  process.stderr.write(`${BINARY}: failed to launch ${bin}: ${result.error.message}\n`);
  process.exit(1);
}

if (result.signal) {
  const num = os.constants.signals[result.signal];
  process.exit(num ? 128 + num : 1);
}

process.exit(result.status === null ? 1 : result.status);

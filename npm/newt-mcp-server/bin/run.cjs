#!/usr/bin/env node
'use strict';

// Generic launcher shared by every @gilamonster/<bin> shim: resolve the platform
// binary and exec it, passing through argv, stdio, exit code, and Unix signals.

const os = require('os');
const { spawn } = require('child_process');
const { binaryPath, BINARY } = require('../lib/binary.cjs');

let bin;
try {
  bin = binaryPath();
} catch (err) {
  process.stderr.write(`${err && err.message ? err.message : err}\n`);
  process.exit(1);
}

const child = spawn(bin, process.argv.slice(2), { stdio: 'inherit' });
if (process.platform !== 'win32') {
  for (const signal of ['SIGINT', 'SIGTERM', 'SIGHUP']) {
    process.on(signal, () => child.kill(signal));
  }
}
child.on('error', (err) => {
  process.stderr.write(`${BINARY}: failed to launch ${bin}: ${err.message}\n`);
  process.exit(1);
});
child.on('exit', (status, signal) => {
  const num = signal && os.constants.signals[signal];
  process.exit(signal ? (num ? 128 + num : 1) : (status === null ? 1 : status));
});

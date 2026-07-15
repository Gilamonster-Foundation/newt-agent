#!/usr/bin/env node
'use strict';

// Umbrella launcher. `newt-agent` depends on `@gilamonster/newt`; delegate to its
// launcher, which resolves and execs the platform binary (and exits).
require('@gilamonster/newt/bin/run.cjs');

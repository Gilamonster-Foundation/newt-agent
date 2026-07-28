#!/usr/bin/env bash
# PASS when `python3 add.py` prints 5.
[ -f add.py ] || { echo "add.py missing"; exit 1; }
out="$(python3 add.py 2>/dev/null | tr -d '[:space:]')"
[ "$out" = "5" ] || { echo "expected 5, got: $out"; exit 1; }

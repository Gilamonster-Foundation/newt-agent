#!/usr/bin/env bash
# PASS when hello.txt contains exactly "hello".
[ -f hello.txt ] || { echo "hello.txt missing"; exit 1; }
[ "$(tr -d '[:space:]' < hello.txt)" = "hello" ] || { echo "wrong contents: $(cat hello.txt)"; exit 1; }

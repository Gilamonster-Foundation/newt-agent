#!/bin/sh
grep -q "the quick" notes.md && ! grep -q "teh" notes.md

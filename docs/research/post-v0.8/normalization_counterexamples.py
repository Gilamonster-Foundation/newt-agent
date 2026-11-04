#!/usr/bin/env python3
"""Counterexample 1 of authorization-attestation-composition.md.

A constraint that compares strings can be sound over strings and still be
wrong about the objects those strings resolve to. Each row prints whether a
textual check accepts the child value and whether the resolved object is
still inside the parent's intended scope. A row with True / False is a token
that looks narrower but is not.

Standard library only. Run with `python3 -I`. Not wired into any build.
"""
import os
import posixpath
import tempfile
import unicodedata
from urllib.parse import unquote, urlsplit


def row(label, textual, resolved):
    print(f"{label:<44} textual_ok={textual!s:<5} resolved_inside={resolved}")


def url_inside(parent, child):
    p, c = urlsplit(parent), urlsplit(child)
    path = posixpath.normpath(unquote(c.path))
    return c.hostname == p.hostname and (path + "/").startswith(p.path.rstrip("/") + "/")


# 1. Path: a lexical prefix test (an unsound extension type) vs normalization.
parent, child = "/work/repo-a/", "/work/repo-a/../repo-b/secret"
row("path prefix + '..'", child.startswith(parent), posixpath.normpath(child).startswith(parent))

# 2. URL: a lexical prefix test vs parsed host and decoded, normalized path.
for parent, child in [
    ("https://forge.example/org/repo-a", "https://forge.example/org/repo-a/../repo-b"),
    ("https://forge.example/org/repo-a", "https://forge.example/org/repo-a%2F..%2Frepo-b"),
    ("https://forge.example", "https://forge.example@evil.example/org/repo-a"),
    ("https://forge.example", "https://forge.example.evil.example/org/repo-a"),
]:
    row("url prefix: " + child[len("https://"):][:31], child.startswith(parent), url_inside(parent, child))

# 3. Denylist (not_one_of) vs aliases a normalizing or case-folding FS merges.
excluded = {"/srv/caf\u00e9/secret", "/var/agent/.ssh/id_ed25519"}
for child, fold in [
    ("/srv/cafe\u0301/secret", lambda s: unicodedata.normalize("NFC", s)),
    ("/var/agent/.SSH/id_ed25519", str.lower),
]:
    row("not_one_of: " + ascii(child)[1:-1], child not in excluded,
        fold(child) not in {fold(e) for e in excluded})

# 4. exact() is unchanged, but the object behind it moves between grant and use.
with tempfile.TemporaryDirectory() as raw_tmp:
    # realpath: on macOS the temp dir sits behind the /var -> /private/var link.
    tmp = os.path.realpath(raw_tmp)
    for d in ("repo-a", "repo-b"):
        os.mkdir(os.path.join(tmp, d))
    open(os.path.join(tmp, "repo-a", "README"), "w").close()
    open(os.path.join(tmp, "repo-b", "secret"), "w").close()
    link = os.path.join(tmp, "repo-a", "link")
    os.symlink("README", link)
    at_grant = os.path.relpath(os.path.realpath(link), tmp)
    os.remove(link)
    os.symlink(os.path.join("..", "repo-b", "secret"), link)
    at_use = os.path.relpath(os.path.realpath(link), tmp)
    row(f"exact symlink: {at_grant} -> {at_use}", True, at_use.startswith("repo-a" + os.sep))

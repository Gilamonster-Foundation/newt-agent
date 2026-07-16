#!/usr/bin/env bash
# ollama-export-gguf.sh — export every locally-installed Ollama model into a
# normally-named GGUF library that llama.cpp (llama-server router mode) can
# serve directly.
#
# Ollama stores model weights as content-addressed blobs; the weights blob is
# a plain GGUF. This script reads each manifest, finds the weights layer (and
# any multimodal projector layer), and hardlinks it into <dest-dir> under a
# readable name derived from the model:tag. Hardlinks cost zero disk and
# survive later removal of the Ollama store; when <dest-dir> is on a
# different filesystem the script falls back to copying.
#
# Usage: ollama-export-gguf.sh <dest-dir>
#
# Skipped automatically: cloud/remote-only models (no local weights layer).
# Remember: Ollama's Go-format chat templates do NOT transfer — serve the
# exported library with `llama-server --jinja` so each GGUF's embedded
# template is used, and smoke-test each model you care about.
set -euo pipefail

dest="${1:?usage: ollama-export-gguf.sh <dest-dir>}"
store="${OLLAMA_MODELS:-$HOME/.ollama/models}"

[ -d "$store/manifests" ] || { echo "no Ollama store at $store" >&2; exit 1; }
mkdir -p "$dest"

find "$store/manifests" -type f | while read -r manifest; do
    # Name from the manifest path: .../<registry>/<ns>/<model>/<tag>
    rel="${manifest#"$store/manifests/"}"
    name="$(echo "$rel" | awk -F/ '{ printf "%s_%s", $(NF-1), $NF }')"

    python3 - "$manifest" "$store" "$dest" "$name" <<'PY'
import json, os, shutil, sys

manifest, store, dest, name = sys.argv[1:5]
with open(manifest) as f:
    m = json.load(f)

# mediaType -> output filename; the projector (multimodal mmproj) rides along
# when present so vision models stay usable.
kinds = {
    "application/vnd.ollama.image.model": f"{name}.gguf",
    "application/vnd.ollama.image.projector": f"{name}.mmproj.gguf",
}

# `or []` (not a .get default): cloud-only models carry an explicit null.
for layer in m.get("layers") or []:
    out = kinds.get(layer.get("mediaType"))
    if not out:
        continue
    blob = os.path.join(store, "blobs", layer["digest"].replace(":", "-"))
    target = os.path.join(dest, out)
    if not os.path.exists(blob):
        print(f"skip {name}: blob missing (cloud/partial model)", file=sys.stderr)
        continue
    if os.path.exists(target):
        continue
    try:
        os.link(blob, target)  # zero-cost on the same filesystem
        how = "hardlink"
    except OSError:
        shutil.copy2(blob, target)  # cross-filesystem fallback
        how = "copy"
    print(f"{out}  <-  {os.path.basename(blob)}  ({how})")
PY
done

echo "done. Serve with: llama-server --models-dir '$dest' -ngl 99 --jinja" >&2

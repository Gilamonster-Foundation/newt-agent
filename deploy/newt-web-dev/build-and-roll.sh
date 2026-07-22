#!/usr/bin/env bash
# Build newt-web on the build box, bake the dev image, roll the deployment.
# Usage: REGISTRY=<host:port> ./build-and-roll.sh   (run from the repo root)
# Security invariant: REGISTRY is operator-local; never commit a real host.
set -euo pipefail
: "${REGISTRY:?set REGISTRY=<your-registry-host:port>}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
# The binary builds INSIDE the image (glibc-matched); context = repo root.
docker build -t "$REGISTRY/newt-web-dev:latest" -f "$HERE/Containerfile" "$ROOT"
docker push "$REGISTRY/newt-web-dev:latest"
sed "s|REGISTRY|$REGISTRY|" "$HERE/deployment.yaml" | kubectl apply -f "$HERE/service.yaml" -f -
kubectl -n drake rollout status deploy/drake-interactive --timeout=300s

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
# Pin the roll to the exact pushed bytes: what was attested is what runs.
DIGEST_REF="$(docker inspect --format '{{index .RepoDigests 0}}' "$REGISTRY/newt-web-dev:latest")"
echo "rolling image $DIGEST_REF"
sed -e "s|image: REGISTRY/newt-web-dev:latest|image: $DIGEST_REF|" \
    "$HERE/deployment.yaml" | kubectl apply -f "$HERE/service.yaml" -f "$HERE/web-service.yaml" -f -
# SSO ingress: WEB_HOST is operator-local (internal DNS never committed).
if [ -n "${WEB_HOST:-}" ]; then
  sed "s|WEB_HOST|$WEB_HOST|g" "$HERE/ingress.yaml" | kubectl apply -f -
fi
kubectl -n drake rollout status deploy/drake-interactive --timeout=300s

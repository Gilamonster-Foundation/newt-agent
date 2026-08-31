#!/usr/bin/env bash
# Build newt-web on the build box, bake the dev image, roll the deployment.
# Usage: REGISTRY=<host:port> ./build-and-roll.sh   (run from the repo root)
# Security invariant: REGISTRY is operator-local; never commit a real host.
set -euo pipefail
: "${REGISTRY:?set REGISTRY=<your-registry-host:port>}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
COMMON_GIT_DIR="$(git -C "$ROOT" rev-parse --path-format=absolute --git-common-dir)"
PRIMARY_REPO_DIR="$(cd "$COMMON_GIT_DIR/.." && pwd)"
AGENT_MESH_DIR="${AGENT_MESH_DIR:-$PRIMARY_REPO_DIR/../agent-mesh}"
test -f "$AGENT_MESH_DIR/agent-mesh-protocol/Cargo.toml" || {
    echo "missing agent-mesh build context: $AGENT_MESH_DIR" >&2
    exit 1
}
# The binary builds INSIDE the image (glibc-matched); context = repo root.
docker build \
    --build-context "agent_mesh=$AGENT_MESH_DIR" \
    -t "$REGISTRY/newt-web-dev:latest" \
    -f "$HERE/Containerfile" \
    "$ROOT"
docker push "$REGISTRY/newt-web-dev:latest"
# Pin the roll to the exact pushed bytes: what was attested is what runs.
DIGEST_REF="$(docker inspect --format '{{index .RepoDigests 0}}' "$REGISTRY/newt-web-dev:latest")"
echo "rolling image $DIGEST_REF"
sed -e "s|image: REGISTRY/newt-web-dev:latest|image: $DIGEST_REF|" \
    "$HERE/deployment.yaml" | kubectl apply \
      -f "$HERE/service.yaml" -f "$HERE/web-service.yaml" \
      -f "$HERE/networkpolicy.yaml" -f -
# SSO ingress: WEB_HOST is operator-local (internal DNS never committed).
if [ -n "${WEB_HOST:-}" ]; then
  sed "s|WEB_HOST|$WEB_HOST|g" "$HERE/ingress.yaml" | kubectl apply -f -
fi
kubectl -n drake rollout status deploy/drake-interactive --timeout=300s

# Model: not exposed by harness | Harness: Codex | Operator: Shawn Hartsock | Time: 12:33 UTC | Date: 2026-08-15

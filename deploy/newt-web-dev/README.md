# newt-web-dev — the repurposed drake-interactive environment

Decision record: `docs/decisions/newt_web_htmx.md` (D5). Identity is
cluster-side (Secrets: `drake-interactive-hostkeys` — the registered SSH host
keys, escrowed 2026-07-22; `drake-interactive-keys`; `drake-nats-nkey`); the
image and Deployment are disposable.

Security invariant (RATCHET.md posture): **no real hosts/IPs in committed
files** — `REGISTRY` and `<node-ip>` are placeholders; the real values live in
the operator's local environment.

```sh
# build + push (REGISTRY = your local registry host:port)
podman build -t REGISTRY/newt-web-dev:latest -f Containerfile .
podman push REGISTRY/newt-web-dev:latest
# roll
sed "s|REGISTRY|<your-registry>|" deployment.yaml | kubectl apply -f service.yaml -f -
kubectl -n drake rollout status deploy/drake-interactive
# verify the registered identity survived (must print the escrowed fingerprint)
ssh-keyscan -p 30122 -t ed25519 <node-ip> | ssh-keygen -lf -
```

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

## Hardening contract (review, 2026-07-22)

- **Digest-pinned rolls:** `build-and-roll.sh` applies the image by
  **sha256 digest**, never `:latest` — the attested bytes are the running
  bytes. Re-rolling always goes through the script.
- **Host-key rotation requires a roll:** the entrypoint stages keys ONCE at
  startup. Updating `drake-interactive-hostkeys` does NOT rotate the live
  identity — roll the pod afterward, deliberately.
- **Supply chain:** base image digest-pinned; the one downloaded artifact
  (`just`) is sha256-verified per-arch before execution; unsupported arches
  fail the build loudly.
- **Runtime posture:** caps dropped to the named sshd set,
  `allowPrivilegeEscalation: false`, seccomp RuntimeDefault, TCP readiness on
  :22. `fsGroup: 100` serves the shared PVC/NFS storage, not the Secret.

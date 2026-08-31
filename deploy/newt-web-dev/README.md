# newt-web-dev — the repurposed drake-interactive environment

Decision record: `docs/decisions/newt_web_htmx.md` (D5). Identity is
cluster-side (Secrets: `drake-interactive-hostkeys` — the registered SSH host
keys, escrowed 2026-07-22; `drake-interactive-keys`; `drake-nats-nkey`); the
image and Deployment are disposable.

Security invariant (RATCHET.md posture): **no real hosts/IPs in committed
files** — `REGISTRY` and `<node-ip>` are placeholders; the real values live in
the operator's local environment.

```sh
# Run from the repository root. The sibling agent-mesh checkout supplies
# newt-web's path dependency; override AGENT_MESH_DIR if needed.
REGISTRY=<your-registry> ./deploy/newt-web-dev/build-and-roll.sh
# verify the registered identity survived (must print the escrowed fingerprint)
ssh-keyscan -p 30122 -t ed25519 <node-ip> | ssh-keygen -lf -
```

## Hardening contract (review, 2026-08-15)

- **Digest-pinned rolls:** `build-and-roll.sh` applies the image by
  **sha256 digest**, never `:latest` — the attested bytes are the running
  bytes. Re-rolling always goes through the script.
- **Host-key rotation requires a roll:** the entrypoint stages keys ONCE at
  startup. Updating `drake-interactive-hostkeys` does NOT rotate the live
  identity — roll the pod afterward, deliberately.
- **Supply chain:** base image digest-pinned; downloaded `just` and Herdr
  artifacts are sha256-verified per-arch before execution; unsupported
  architectures fail the build loudly.
- **Runtime posture:** caps dropped to the named sshd set,
  `allowPrivilegeEscalation: false`, seccomp RuntimeDefault, HTTP health
  readiness on :8880. `fsGroup: 100` serves persistent home and workspace
  storage, not the Secret. The workload is pinned to `nuc`, where `/workspaces`
  uses the workspace export's local backing directory to avoid loopback NFS.

Model: not exposed by harness | Harness: Codex | Operator: Shawn Hartsock | Time: 12:33 UTC | Date: 2026-08-15

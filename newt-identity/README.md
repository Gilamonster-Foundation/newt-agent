# newt-identity

Per-user object-capability identity for newt-agent: a root `UserKey` and
signed, attenuation-only `AgentKey` delegations that bound an agent's tool
authority.

This is where a newt host turns a name-based permission preset into a real
capability: a signed, verified, attenuation-only authority rooted in a
per-user key (`~/.newt/identity.pem`, mode 0600). The operating key's caveats
are signed into its cert and provably narrower than the root — delegation
refuses to mint a child that amplifies, and chain verification re-checks
attenuation at every link, so a confused or compromised agent can only ever
narrow its authority, never widen it.

Part of [Newt-Agent](https://github.com/Gilamonster-Foundation/newt-agent), a
free, friendly, local agentic coder.

## License

Apache-2.0

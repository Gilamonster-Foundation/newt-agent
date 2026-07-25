# newt-git

`newt-git` is Newt-Agent's embedded local Git engine. It wraps the pure-Rust
[`grit-lib`](https://crates.io/crates/grit-lib) implementation behind
Newt-Agent's `GitCaveats` capability surface.

The crate exposes structured operations for repository status, history,
diffs, staging, commits, branches, checkout, rebasing, and stashes. Every
repository read or write receives the caller's already-composed caveats and
fails closed when the required capability is absent.

Operations are local to an existing repository. Network operations such as
clone, fetch, and push are outside this crate's current surface.

Part of [Newt-Agent](https://github.com/Gilamonster-Foundation/newt-agent).

## License

Apache-2.0

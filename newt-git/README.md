# newt-git

`newt-git` is Newt-Agent's embedded local Git engine. It wraps the pure-Rust
[`grit-lib`](https://crates.io/crates/grit-lib) implementation behind
Newt-Agent's `GitCaveats` capability surface.

The crate exposes structured operations for repository status, history,
diffs, staging, commits, branches, checkout, rebasing, and stashes. Every
repository read or write receives the caller's already-composed caveats and
fails closed when the required capability is absent.

The injected tool also requires the session's filesystem read grants for its
repository and Git administrative directories. `branch-list` reports separate
counts and full ref names for local branches and cached remote-tracking
branches (`scope=local|remote|all`, default `all`), excluding symbolic remote
aliases. It does not contact remotes or count open pull requests. This is the
only Git operation exposed in Explain, Research, and Plan dispositions; other
operations retain their existing Act boundary.

Branch listing reads the files-backed ref store without opening an object
database or loading the global Git configuration cascade. Linked worktrees
need explicit read grants for their external Git metadata. Reftables, nested
common directories, ambient `GIT_NAMESPACE`, and symbolic targets outside
`refs/` are currently unsupported and fail with an error rather than an
incomplete count. Canonical path preflight is not a replacement for the
workspace's object-bound filesystem confinement work: concurrent path
replacement remains outside this check.

Operations are local to an existing repository. Network operations such as
clone, fetch, and push are outside this crate's current surface.

Part of [Newt-Agent](https://github.com/Gilamonster-Foundation/newt-agent).

## License

Apache-2.0

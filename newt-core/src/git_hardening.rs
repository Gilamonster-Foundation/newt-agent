//! Confused-deputy-safe `git` subprocess invocation (step-7.4).
//!
//! # Why this exists
//!
//! The harness runs `git` as a *subprocess* in several internal, non-model
//! paths — collecting turn-end evidence ([`crate::agentic`] `claim_check`),
//! building the workspace context banner, computing a diff for the ACP worker,
//! crew bookkeeping. Each of those runs `git` **in the user's workspace**, which
//! on the hostile-repository / hostile-model threat model is attacker-controlled.
//!
//! `git` is a confused-deputy engine: a repository's `.git/config` (or
//! `.gitattributes`) can point ordinary read commands at an arbitrary program —
//! `core.fsmonitor` (fires on `git status`), `core.hooksPath` / hooks,
//! `diff.external` and per-driver `textconv` (fire on `git diff`), `core.pager`,
//! `core.sshCommand`, `protocol.ext`. A raw `Command::new("git")` in the
//! workspace therefore executes attacker code **outside** the Landlock/OCAP
//! fence, inheriting newt's full environment (provider keys, `NEWT_AGENT_KEY`).
//! This was empirically confirmed: `git status` with a repo-local
//! `core.fsmonitor=<payload>` ran the payload out-of-fence.
//!
//! [`hardened_git`] neutralizes that surface for every harness `git` call:
//!
//! - **`-c` overrides** beat repo-local `.git/config`, so `core.fsmonitor=`,
//!   `core.hooksPath=/dev/null`, `core.pager=cat`, `core.sshCommand=false`,
//!   `diff.external=`, and `protocol.ext.allow=never` disarm those gadgets even
//!   when the attacker wrote them into the repo.
//! - **`env_clear` + a minimal allowlist** drops every ambient gadget variable
//!   (`GIT_EXTERNAL_DIFF`, `GIT_SSH*`, `GIT_PAGER`, `GIT_ASKPASS`, …) AND newt's
//!   own secrets/authority, so a gadget that somehow still fires gets neither a
//!   payload from the environment nor newt's credentials.
//! - **`GIT_CONFIG_GLOBAL=/dev/null` + `GIT_CONFIG_SYSTEM=/dev/null`** ignore the
//!   user/system git config entirely.
//!
//! `textconv` uses *named* drivers that `-c` cannot wildcard away, so a caller
//! that runs `git diff` / `git log -p` / `git show` should ALSO pass
//! `--no-textconv --no-ext-diff` in `args` (belt-and-suspenders on top of the
//! `diff.external=` override).

use std::path::Path;
use std::process::Command;

/// Git config keys/values forced via `-c` so a hostile repo `.git/config` cannot
/// turn a harness `git` call into code execution. `-c` outranks repo-local
/// config, so these win even when the attacker set the opposite in `.git/config`.
const GIT_HARDENING_OVERRIDES: &[&str] = &[
    "core.fsmonitor=",          // no fsmonitor hook (fires on `git status`)
    "core.hooksPath=/dev/null", // no hooks (fire on commit/checkout)
    "core.pager=cat",           // no pager subprocess
    "core.sshCommand=false",    // no ssh gadget
    "core.askpass=",            // no askpass gadget
    "core.editor=false",        // no editor gadget
    "diff.external=",           // no external diff program
    "protocol.ext.allow=never", // no `ext::` transport
];

/// Build a **confused-deputy-safe** `git` [`Command`] running in `cwd` with
/// `args`. Every harness `git` subprocess that touches a (possibly hostile)
/// workspace must go through this instead of a raw `Command::new("git")`.
#[must_use]
pub fn hardened_git(cwd: &Path, args: &[&str]) -> Command {
    let mut c = Command::new("git");
    // A top-level option: never take the optional fsmonitor/index locks that can
    // trigger the fsmonitor hook as a side effect.
    c.arg("--no-optional-locks");
    for kv in GIT_HARDENING_OVERRIDES {
        c.arg("-c").arg(kv);
    }
    c.args(args).current_dir(cwd);

    // Start from an EMPTY environment: no ambient GIT_* gadget var, and none of
    // newt's secrets/authority, can reach git or a gadget that fires.
    c.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        c.env("PATH", path);
    }
    // Keep HOME for git's own housekeeping, but the global config is redirected
    // to /dev/null below, so ~/.gitconfig / XDG git config are ignored anyway.
    if let Some(home) = std::env::var_os("HOME") {
        c.env("HOME", home);
    }
    c.env("LC_ALL", "C")
        .env("LANG", "C")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat");
    c
}

//! Layered runtime configuration for the bridle (agent-bridle#141 / epic #139).
//!
//! `Caveats` is **authority** (per-invocation, rides [`crate::ToolContext`]).
//! [`BridleConfig`] is **mechanism**: the tunable knobs, limits, path lists, and
//! feature toggles that shape *how* confinement is applied — a separate channel
//! that never amplifies authority and never touches the mint chokepoint or the
//! honesty lattice (ADR 0017).
//!
//! These are pure, serde-only **types**; the file/env loader and its precedence
//! (`defaults → file → env → API`) live in the `agent-bridle-config` crate (#142).
//! Every [`Default`] here reproduces today's hard-coded constants **byte-for-byte**
//! — the anti-drift tests below assert each `Policy::default()` equals the
//! constant it mirrors (`gate.rs`, `sandbox.rs`, `rootfs.rs`), so this file can
//! never silently diverge from current behavior. Nothing consumes `BridleConfig`
//! yet (this issue is inert); wiring lands in #143–#153.

use serde::{Deserialize, Serialize};

use crate::report::AxisEnforcement;
use crate::HumanGate;

fn to_vec(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

/// A configurable path list with **extend-by-default** semantics: `resolve()`
/// returns `base ∪ extra` unless `replace` is set, in which case only `extra` is
/// used. This lets config *widen* a security-relevant list (add a read path)
/// safely, while **shrinking** one (dropping a loader path that would break
/// confinement) requires an explicit `replace = true` opt-in. A widening is
/// surfaced via [`PathList::widens`] so it can be disclosed (never silent).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathList {
    /// Built-in defaults (today's constant). Not normally set from config.
    pub base: Vec<String>,
    /// Operator additions (extend) — or the full set when `replace`.
    #[serde(default)]
    pub extra: Vec<String>,
    /// `true` ⇒ ignore `base`, use only `extra` (the shrink opt-in).
    #[serde(default)]
    pub replace: bool,
}

impl PathList {
    /// A default-backed list from a static slice (the const-mirroring constructor).
    #[must_use]
    pub fn from_defaults(base: &[&str]) -> Self {
        Self {
            base: to_vec(base),
            extra: Vec::new(),
            replace: false,
        }
    }

    /// The effective list: `base ∪ extra` (dedup, order-preserving), or `extra`
    /// alone when `replace`.
    #[must_use]
    pub fn resolve(&self) -> Vec<String> {
        if self.replace {
            return self.extra.clone();
        }
        let mut out = self.base.clone();
        for e in &self.extra {
            if !out.contains(e) {
                out.push(e.clone());
            }
        }
        out
    }

    /// `true` when the operator has *widened* the built-in list (added entries
    /// without replacing) — a disclosure-worthy loosening.
    #[must_use]
    pub fn widens(&self) -> bool {
        !self.replace && !self.extra.is_empty()
    }
}

/// The top-level confinement mode. `Bridled` (default) confines per the caveats +
/// backends; `Unbridle` is the explicit, acknowledged, honest "off" (grant
/// `Caveats::top()`, advisory floor, `SandboxKind::None`) — resolved by the loader
/// (#151), never reachable by omission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BridleMode {
    /// Confine normally (the default).
    #[default]
    Bridled,
    /// No confinement — advisory only, loudly disclosed (#151).
    Unbridle,
}

/// Gate defaults (`gate.rs` constants) — and the **human-gesture axis** of the
/// ADR 0018 mode lattice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatePolicy {
    /// The fence-strength floor stamped when none is set (`DEFAULT_STRENGTH_FLOOR`).
    pub default_strength_floor: AxisEnforcement,
    /// Cap on the discharge freshness-window scan (`MAX_FRESHNESS_WINDOW`).
    pub max_freshness_window: u64,
    /// The **step-up floor** — the human-gate posture the host enforces for a
    /// HIGH-consequence act (ADR 0018 D9/D11, R6). This is the human-gesture axis
    /// of the mode lattice, the config sibling of the capability axis
    /// [`BridleConfig::mode`]. Defaults to [`HumanGate::Passkey`] (the human leash
    /// is on unless deliberately lowered). `none` is the *legal* "no ceremony"
    /// case (e.g. CI) — distinct from the illegal *acked-off* combination the
    /// loader refuses (the D10 no-step-up ack **while bridled**; that ack rides a
    /// separate env-only channel, never a config file — ADR 0018 D3). The
    /// Autonomous posture (`none` while unbridled) is reached at runtime via that
    /// second ack, never by a config file lowering this floor.
    #[serde(default)]
    pub step_up: HumanGate,
}

impl Default for GatePolicy {
    fn default() -> Self {
        Self {
            default_strength_floor: crate::gate::DEFAULT_STRENGTH_FLOOR,
            max_freshness_window: 4096,
            step_up: HumanGate::Passkey,
        }
    }
}

/// Backend availability toggles (subsumes `BRIDLE_REQUIRE_*`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendToggles {
    /// Require Landlock (fail closed if unavailable).
    #[serde(default)]
    pub require_landlock: bool,
    /// Require Seatbelt (fail closed if unavailable).
    #[serde(default)]
    pub require_seatbelt: bool,
    /// Backends to force off by name (e.g. `["seatbelt"]`).
    #[serde(default)]
    pub disable: Vec<String>,
}

/// How a confined child's *direct* network-socket authority is enforced, beyond
/// the caveat-driven Landlock TCP rule. A **mechanism** knob (it rides
/// [`SandboxPolicy`]), never authority (`Caveats`): the caller states the
/// required floor and the backend owns the enforcement (ADR 0017).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ChildNetworkPolicy {
    /// Historical behavior: rely on the caveat-driven Landlock net rule alone.
    /// An empty `net` scope kernel-denies TCP **connect/bind** on an ABI-v4
    /// kernel, but Landlock cannot filter UDP, DNS, raw or packet sockets — a
    /// child under `net: none` can still create them. Backward-compatible
    /// default, so existing configs and callers are unchanged.
    #[default]
    LandlockOnly,
    /// Additionally install a seccomp `socket()`-family deny (AF_INET / AF_INET6
    /// / AF_PACKET → `EACCES`) on the confining thread immediately before spawn,
    /// so the child and every fork/exec descendant cannot create ANY off-box
    /// socket regardless of protocol — closing the UDP/DNS/raw leg Landlock
    /// misses. `AF_UNIX` is deliberately still allowed (a path-named unix socket
    /// is already governed by the fs fence; abstract-namespace unix sockets are a
    /// bounded residual). Takes effect only when the `net` caveat is already
    /// deny-all (`net: none`); a granted net scope leaves it inert (the caller
    /// asked for egress). Requires the `linux-landlock` backend (which pulls the
    /// safe `seccompiler` install path); on other platforms it is inert, so a
    /// caller that *requires* it must fail closed via `backends.require_landlock`.
    DenyDirect,
}

/// Sandbox path lists + ABI floors (`sandbox.rs` constants).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxPolicy {
    /// Trusted inventory of private roots which runtime closure additions must
    /// not expose. Required only for NamedRoot; never inferred from model input.
    /// This is inventory-relative protection, not discovery of all private state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub named_root_protected_roots: Option<std::collections::BTreeSet<String>>,
    /// Backend enable/require toggles.
    #[serde(default)]
    pub backends: BackendToggles,
    /// Direct child network-socket confinement beyond the Landlock TCP rule
    /// ([`ChildNetworkPolicy`]). `#[serde(default)]` so configs written before
    /// this field existed keep parsing (they get `LandlockOnly`).
    #[serde(default)]
    pub child_network: ChildNetworkPolicy,
    /// Windows AppContainer launcher provenance: optional absolute path to the
    /// trusted `agent-bridle-aclaunch.exe` helper. When unset, Windows uses only
    /// the helper shipped next to the current executable. Ambient `PATH` is never
    /// a trusted source for this sandbox constructor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub appcontainer_launcher_path: Option<String>,
    /// Read base when `fs_read` restricted (`BASE_READ_PATHS`).
    pub base_read_paths: PathList,
    /// Executable dirs read-allowed only when `exec` is ambient (`BIN_READ_PATHS`).
    pub bin_read_paths: PathList,
    /// Device sinks that are ALWAYS openable read+write inside the jail
    /// (#1220): discard/zero devices carry no authority, and tools open them
    /// as plumbing — git opens `/dev/null` O_RDWR and dies inside a confined
    /// child otherwise. Folded into every write ruleset/profile regardless of
    /// the granted `fs_write` scope. `#[serde(default)]` so configs written
    /// before this field existed keep parsing (they get the defaults).
    #[serde(default = "default_device_sink_paths")]
    pub device_sink_paths: PathList,
    /// macOS runtime support for fresh pseudo-terminals. Slave access requires
    /// the kernel's `com.apple.sandbox.pty` extension, never a blanket tty path
    /// grant. This trusted mechanism option is disabled by default and inert on
    /// other backends; it does not assert cross-platform terminal isolation.
    #[serde(default)]
    pub macos_private_ptys: bool,
    /// Execute allow-list: the dynamic loader files only (`LOADER_PATHS`).
    pub loader_paths: PathList,
    /// Loopback identifiers for the net axis (`LOOPBACK_HOSTS`).
    pub loopback_hosts: Vec<String>,
    /// Minimum Landlock ABI (`ABI_FLOOR`).
    pub landlock_abi_floor: u32,
    /// Minimum Landlock ABI for TCP net rules (`NET_ABI_FLOOR`).
    pub landlock_net_abi_floor: u32,
}

/// The `device_sink_paths` default: the null/zero/full discard devices —
/// every write to them is a no-op (or ENOSPC by contract, for `/dev/full`),
/// so granting fresh opens carries no authority. Deliberately excludes
/// `/dev/tty` (interactive prompts should fail fast inside confinement) and
/// `/dev/std{out,err}` (inherited fds need no open; the procfs symlinks
/// resolve to arbitrary files and would widen the grant).
fn default_device_sink_paths() -> PathList {
    PathList::from_defaults(&["/dev/null", "/dev/zero", "/dev/full"])
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        // The read base is backend-specific: the Landlock (Linux) loader/library
        // trees vs the Seatbelt (macOS) dyld/system base. One `base_read_paths`
        // field defaults to whichever backend this host runs (I5-B, #144). The
        // literals are duplicated here (not referenced from the cfg-gated sandbox
        // consts) so `BridleConfig` constructs on every platform.
        #[cfg(target_os = "macos")]
        let base_read: &[&str] = &[
            "/usr",
            "/bin",
            "/sbin",
            "/System",
            "/Library",
            "/opt",
            "/private/etc",
            "/private/var/db/dyld",
            // /etc/localtime and /usr/share/zoneinfo resolve into this
            // OS-owned database on macOS. TZ never adds arbitrary read roots.
            "/private/var/db/timezone",
            "/dev",
        ];
        #[cfg(not(target_os = "macos"))]
        let base_read: &[&str] = &[
            "/lib",
            "/lib64",
            "/lib32",
            "/libx32",
            "/usr/lib",
            "/usr/lib64",
            "/usr/libexec",
            "/usr/share",
            "/etc/ld.so.cache",
            "/etc/ld.so.preload",
            "/etc/alternatives",
            "/etc/nsswitch.conf",
            "/etc/localtime",
            "/etc/resolv.conf",
            "/etc/ssl",
            "/etc/ca-certificates",
            "/proc/self",
            "/dev/null",
            "/dev/zero",
            "/dev/full",
            "/dev/urandom",
            "/dev/random",
        ];
        Self {
            backends: BackendToggles::default(),
            child_network: ChildNetworkPolicy::default(),
            appcontainer_launcher_path: None,
            named_root_protected_roots: None,
            base_read_paths: PathList::from_defaults(base_read),
            device_sink_paths: default_device_sink_paths(),
            macos_private_ptys: false,
            bin_read_paths: PathList::from_defaults(&[
                "/usr/bin",
                "/bin",
                "/usr/sbin",
                "/sbin",
                "/usr/local/bin",
                "/usr/local/sbin",
                "/opt",
            ]),
            loader_paths: PathList::from_defaults(&[
                "/lib64/ld-linux-x86-64.so.2",
                "/lib/ld-linux-x86-64.so.2",
                "/lib/ld-linux.so.2",
                "/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2",
                "/lib64/ld64.so.2",
                "/lib/ld-linux-aarch64.so.1",
                "/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1",
                "/lib/ld-linux-armhf.so.3",
                "/lib/ld-musl-x86-64.so.1",
                "/lib/ld-musl-aarch64.so.1",
            ]),
            loopback_hosts: to_vec(&["localhost", "127.0.0.1", "::1"]),
            landlock_abi_floor: 3,
            landlock_net_abi_floor: 4,
        }
    }
}

/// Minimal-rootfs builder inputs (`rootfs.rs` constants).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootfsPolicy {
    /// Curated runtime data injected into every plan (`DATA_PATHS`).
    pub data_paths: PathList,
    /// `$PATH` fallback for bare-name program resolution (`search_dirs`).
    pub search_dirs: Vec<String>,
}

impl Default for RootfsPolicy {
    fn default() -> Self {
        Self {
            // The curated runtime data paths (formerly `rootfs::DATA_PATHS`).
            // This policy is the single source of truth — the rootfs builder
            // reads it directly (I5, #144). Kept here (not in `rootfs.rs`) so the
            // default is platform-independent: `rootfs` is a Linux-only module,
            // but `BridleConfig` must construct on every host.
            data_paths: PathList::from_defaults(&[
                "/usr/share",
                "/usr/lib/locale",
                "/etc/ld.so.cache",
                "/etc/ld.so.preload",
                "/etc/alternatives",
                "/etc/nsswitch.conf",
                "/etc/localtime",
                "/etc/resolv.conf",
                "/etc/ssl",
                "/etc/ca-certificates",
                "/proc/self",
                "/dev/null",
                "/dev/zero",
                "/dev/full",
                "/dev/urandom",
                "/dev/random",
            ]),
            search_dirs: to_vec(&[
                "/usr/local/bin",
                "/usr/bin",
                "/bin",
                "/usr/local/sbin",
                "/usr/sbin",
                "/sbin",
            ]),
        }
    }
}

/// The default `PATH` for a **fully-authorized** (`exec = Scope::All`) confined
/// child: the ambient `$PATH` when set and non-empty, else the conventional
/// [`RootfsPolicy`] search dirs, joined with the platform separator.
///
/// The sandbox-host engine (`HostShellTool`, in `agent-bridle-tool-shell`) seeds
/// this into the child so bare program names (`grep`/`ls`/`find`) resolve like
/// the host shell would, instead of leaning on the shell's fragile compiled
/// `_CS_PATH` fallback. It is only meaningful — and only called — when
/// `exec` is unrestricted, so seeding it grants nothing the caller's exec
/// authority does not already permit; `env_clear` still scrubs everything else.
/// Mirrors the exec-search-dir precedence that anchors the L3 `Execute`
/// allow-list (`sandbox::exec_search_dirs`); kept here so it constructs on every
/// platform.
#[must_use]
pub fn default_exec_path() -> String {
    if let Ok(path) = std::env::var("PATH") {
        if !path.is_empty() {
            return path;
        }
    }
    let dirs = RootfsPolicy::default().search_dirs;
    std::env::join_paths(&dirs)
        .ok()
        .and_then(|joined| joined.into_string().ok())
        .unwrap_or_else(|| dirs.join(if cfg!(windows) { ";" } else { ":" }))
}

/// Toggles for the automatic "normalizations" (assists) — each defaults to today's
/// always-on behavior; only *loosening*-safe ones are exposed as on/off (safety
/// normalizations like fs canonicalization and env-scrub are intentionally absent
/// here — see ADR 0017).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizationPolicy {
    /// Match a granted bare exec name against a program's basename (`context.rs`).
    pub exec_basename_match: bool,
    /// Resolve a dynamic program's `ldd` shared-library closure (`rootfs.rs`).
    pub ldd_closure: bool,
    /// #113 fallback: add glibc NSS modules to the closure.
    pub nss_closure_fallback: bool,
    /// #113 fallback: add the Python stdlib dirs when a `python*` is granted.
    pub python_closure_fallback: bool,
    /// Emit the missing-`.so` deny-of-function canary diagnostic (`jaild`).
    pub missing_so_canary: bool,
    /// Use the content-addressed rootfs build cache.
    pub rootfs_cache: bool,
}

impl Default for NormalizationPolicy {
    fn default() -> Self {
        Self {
            exec_basename_match: true,
            ldd_closure: true,
            nss_closure_fallback: true,
            python_closure_fallback: true,
            missing_so_canary: true,
            rootfs_cache: true,
        }
    }
}

/// Default network posture when no rule matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NetDefault {
    /// Fail-closed default (today's behavior).
    #[default]
    Deny,
    /// Allow — only meaningful under an explicit relaxed/unbridle posture.
    Allow,
}

/// How a host is matched. `#[non_exhaustive]` so REST/gRPC predicate variants
/// (#153) can be added later without breaking existing configs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum HostMatch {
    /// Exact hostname (today's semantics).
    Exact(String),
    /// Domain suffix (e.g. `.example.com`).
    Suffix(String),
    /// Glob pattern.
    Glob(String),
}

/// One network rule. `#[non_exhaustive]` to admit `Rest {..}` / `Grpc {..}`
/// variants additively (#153/#153-followup).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum NetRule {
    /// Host-level allow (the v1 predicate; a superset of exact-host).
    Host(HostMatch),
}

/// Network policy — *refines* how the `net` authority axis (`Scope<String>`) is
/// interpreted/enforced; defaults to today's exact-host behavior (empty rules).
/// A structured rule is proxy-enforced (userspace) ⇒ the honesty report keeps a
/// non-loopback allow-list `advisory`, never `kernel` (#152).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetPolicy {
    /// Posture when no rule matches.
    #[serde(default)]
    pub default: NetDefault,
    /// Ordered match rules (empty ⇒ pure `Scope<String>` exact-host behavior).
    #[serde(default)]
    pub rules: Vec<NetRule>,
}

impl Default for NetPolicy {
    fn default() -> Self {
        Self {
            default: NetDefault::Deny,
            rules: Vec::new(),
        }
    }
}

/// Shell-tool + egress-proxy limits (`shell_tool.rs` / `net_proxy.rs` constants).
/// Anti-drift tests for these land with the wiring PR (#143) where the consts live.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitsPolicy {
    /// Max permitted wall-clock timeout, seconds (`MAX_TIMEOUT_SECS`).
    pub max_timeout_secs: u64,
    /// Default timeout when unspecified, seconds (`DEFAULT_TIMEOUT_SECS`).
    pub default_timeout_secs: u64,
    /// Captured stdout/stderr cap, bytes (`MAX_OUTPUT_BYTES`).
    pub max_output_bytes: usize,
    /// Env vars expandable in redirect targets (`VAR_ALLOWLIST`).
    pub var_allowlist: Vec<String>,
    /// Max glob nesting depth (`MAX_GLOB_DEPTH`).
    pub max_glob_depth: usize,
    /// Max glob matches (`MAX_GLOB_MATCHES`).
    pub max_glob_matches: usize,
    /// Proxy request-header cap, bytes (`MAX_HEAD`).
    pub proxy_max_head: usize,
    /// Proxy per-connection socket timeout, seconds (`CONN_TIMEOUT`).
    pub proxy_conn_timeout_secs: u64,
    /// Proxy bind address (loopback ephemeral).
    pub proxy_bind: String,
    /// Egress audit sink path (subsumes `BRIDLE_NET_AUDIT`); `None` = off.
    #[serde(default)]
    pub audit_sink: Option<String>,
    /// AB-004: environment variable names DROPPED before spawning a child,
    /// regardless of the exec leash. Loader / interpreter / hook vars
    /// (`LD_PRELOAD`, `PYTHONPATH`, `GIT_SSH_COMMAND`, `BASH_ENV`, …) change what
    /// an *allowed* program actually executes, so they can never be caller-set.
    /// Overridable config data (three-Cs).
    #[serde(default = "default_env_denylist")]
    pub env_denylist: Vec<String>,
}

/// The default loader/interpreter/hook environment denylist (AB-004). Each name
/// here can hijack code execution inside an otherwise-allowed program.
#[must_use]
pub fn default_env_denylist() -> Vec<String> {
    to_vec(&[
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "LD_AUDIT",
        "DYLD_INSERT_LIBRARIES",
        "DYLD_LIBRARY_PATH",
        "PYTHONPATH",
        "PYTHONSTARTUP",
        "NODE_OPTIONS",
        "RUBYOPT",
        "RUBYLIB",
        "PERL5OPT",
        "PERL5LIB",
        "CLASSPATH",
        "JAVA_TOOL_OPTIONS",
        "_JAVA_OPTIONS",
        "GIT_SSH_COMMAND",
        "GIT_EXEC_PATH",
        "GIT_TEMPLATE_DIR",
        "BASH_ENV",
        "ENV",
        "SHELLOPTS",
        "BASHOPTS",
        "PROMPT_COMMAND",
    ])
}

/// Drop denied loader/hook keys from `env`, returning the surviving map plus the
/// dropped names (for disclosure). See [`LimitsPolicy::env_denylist`] (AB-004).
#[must_use]
pub fn fence_env(
    env: &std::collections::BTreeMap<String, String>,
    denylist: &[String],
) -> (std::collections::BTreeMap<String, String>, Vec<String>) {
    let denied: std::collections::BTreeSet<&str> = denylist.iter().map(String::as_str).collect();
    let mut fenced = std::collections::BTreeMap::new();
    let mut dropped = Vec::new();
    for (k, v) in env {
        if denied.contains(k.as_str()) {
            dropped.push(k.clone());
        } else {
            fenced.insert(k.clone(), v.clone());
        }
    }
    (fenced, dropped)
}

impl Default for LimitsPolicy {
    fn default() -> Self {
        Self {
            max_timeout_secs: 300,
            default_timeout_secs: 60,
            max_output_bytes: 1 << 20,
            var_allowlist: to_vec(&[
                "HOME", "PWD", "OLDPWD", "USER", "LOGNAME", "TMPDIR", "LANG", "LC_ALL", "SHELL",
                "HOSTNAME", "TERM",
            ]),
            max_glob_depth: 64,
            max_glob_matches: 4096,
            proxy_max_head: 8 * 1024,
            proxy_conn_timeout_secs: 30,
            proxy_bind: "127.0.0.1:0".to_string(),
            audit_sink: None,
            env_denylist: default_env_denylist(),
        }
    }
}

/// Web-fetch limits (`web_fetch.rs` constants). Anti-drift lands with wiring.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebPolicy {
    /// Max redirect hops (`MAX_REDIRECTS`).
    pub max_redirects: usize,
    /// Default response body cap, bytes (`DEFAULT_MAX_BYTES`).
    pub default_max_bytes: usize,
    /// Absolute ceiling on the body cap, bytes (`HARD_MAX_BYTES`).
    pub hard_max_bytes: usize,
    /// Per-request timeout, seconds (`REQUEST_TIMEOUT_SECS`).
    pub request_timeout_secs: u64,
}

impl Default for WebPolicy {
    fn default() -> Self {
        Self {
            max_redirects: 10,
            default_max_bytes: 5 * 1024 * 1024,
            hard_max_bytes: 25 * 1024 * 1024,
            request_timeout_secs: 30,
        }
    }
}

/// Micro-VM / jail parameters (`jaild` constants). Anti-drift lands with wiring (#147).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmPolicy {
    /// Candidate qemu binaries (`QEMU_PATH`).
    pub qemu_path: Vec<String>,
    /// Guest kernel search paths (`/boot/vmlinuz`).
    pub kernel_search: Vec<String>,
    /// Guest memory, MiB (`VM_MEMORY`).
    pub memory_mb: u32,
    /// qemu accelerator spec (`accel=kvm:tcg`).
    pub accel: String,
    /// Guest kernel command line.
    pub kernel_cmdline: String,
    /// Merged-usr top-level symlinks to reproduce (`MERGED_USR_LINKS`).
    pub merged_usr_links: Vec<String>,
    /// Broker protocol max frame, bytes (`MAX_FRAME`).
    pub max_frame: usize,
    /// Broker socket path (subsumes `BRIDLE_JAILD_SOCKET`).
    #[serde(default)]
    pub jaild_socket: Option<String>,
    /// Guest-init binary path (subsumes `BRIDLE_JAIL_INIT`).
    #[serde(default)]
    pub jail_init: Option<String>,
}

impl Default for VmPolicy {
    fn default() -> Self {
        Self {
            qemu_path: to_vec(&["/usr/bin/qemu-system-x86_64"]),
            kernel_search: to_vec(&["/boot/vmlinuz"]),
            memory_mb: 512,
            accel: "kvm:tcg".to_string(),
            kernel_cmdline: "console=ttyS0 panic=1 loglevel=4".to_string(),
            merged_usr_links: to_vec(&["bin", "sbin", "lib", "lib32", "lib64", "libx32"]),
            max_frame: 64 * 1024 * 1024,
            jaild_socket: None,
            jail_init: None,
        }
    }
}

/// The complete, layered bridle configuration (mechanism). Every field defaults to
/// today's behavior; see the module docs for the authority-vs-mechanism split.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BridleConfig {
    /// Top-level confinement mode.
    pub mode: BridleMode,
    /// Gate defaults.
    pub gate: GatePolicy,
    /// Sandbox path lists + backend toggles.
    pub sandbox: SandboxPolicy,
    /// Automatic-normalization toggles.
    pub normalization: NormalizationPolicy,
    /// Minimal-rootfs builder inputs.
    pub rootfs: RootfsPolicy,
    /// Network refinement policy.
    pub net: NetPolicy,
    /// Shell + proxy limits.
    pub limits: LimitsPolicy,
    /// Web-fetch limits.
    pub web: WebPolicy,
    /// Micro-VM / jail parameters.
    pub vm: VmPolicy,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AB-004: `fence_env` drops every denied loader/hook key and keeps the rest,
    /// reporting what it dropped.
    #[test]
    fn fence_env_drops_denied_keeps_others() {
        let mut env = std::collections::BTreeMap::new();
        env.insert("LD_PRELOAD".to_string(), "/evil.so".to_string());
        env.insert("GIT_SSH_COMMAND".to_string(), "evil".to_string());
        env.insert("SAFE".to_string(), "keep".to_string());
        let (fenced, dropped) = fence_env(&env, &default_env_denylist());
        assert!(!fenced.contains_key("LD_PRELOAD"));
        assert!(!fenced.contains_key("GIT_SSH_COMMAND"));
        assert_eq!(fenced.get("SAFE").map(String::as_str), Some("keep"));
        assert_eq!(dropped.len(), 2);
        assert!(dropped.contains(&"LD_PRELOAD".to_string()));
        assert!(dropped.contains(&"GIT_SSH_COMMAND".to_string()));
    }

    #[test]
    fn mode_defaults_to_bridled() {
        assert_eq!(BridleMode::default(), BridleMode::Bridled);
        assert_eq!(BridleConfig::default().mode, BridleMode::Bridled);
    }

    #[test]
    fn pathlist_extends_by_default_and_replaces_on_opt_in() {
        let mut p = PathList::from_defaults(&["/a", "/b"]);
        assert_eq!(p.resolve(), vec!["/a".to_string(), "/b".to_string()]);
        assert!(!p.widens());

        p.extra = vec!["/c".to_string(), "/a".to_string()]; // /a is a dup
        assert_eq!(
            p.resolve(),
            vec!["/a".to_string(), "/b".to_string(), "/c".to_string()],
            "extend dedups and preserves order"
        );
        assert!(p.widens(), "adding entries without replace is a widening");

        p.replace = true;
        assert_eq!(
            p.resolve(),
            vec!["/c".to_string(), "/a".to_string()],
            "replace uses only extra (the shrink opt-in)"
        );
        assert!(!p.widens(), "replace is not a widening");
    }

    #[test]
    fn gate_policy_defaults_match_constants() {
        let g = GatePolicy::default();
        assert_eq!(g.default_strength_floor, AxisEnforcement::Advisory);
        assert_eq!(g.max_freshness_window, 4096);
        // The human-gesture axis floor (ADR 0018 R6) defaults ON: the human leash
        // is Passkey unless deliberately lowered — Autonomous is never by omission.
        assert_eq!(g.step_up, HumanGate::Passkey);
    }

    #[test]
    fn gate_step_up_floor_round_trips_each_posture() {
        // Each human-gesture posture survives serialize→deserialize as snake_case,
        // so the config file / env / API can express any of them.
        for (gate, tok) in [
            (HumanGate::None, "none"),
            (HumanGate::Prompt, "prompt"),
            (HumanGate::Passkey, "passkey"),
        ] {
            let g = GatePolicy {
                step_up: gate,
                ..GatePolicy::default()
            };
            let json = serde_json::to_value(&g).unwrap();
            assert_eq!(json["step_up"], tok, "serializes snake_case");
            let back: GatePolicy = serde_json::from_value(json).unwrap();
            assert_eq!(back.step_up, gate);
        }
    }

    #[test]
    fn default_config_round_trips_through_json() {
        let c = BridleConfig::default();
        let json = serde_json::to_string(&c).unwrap();
        let back: BridleConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn appcontainer_launcher_path_is_explicit_mechanism_config() {
        assert_eq!(SandboxPolicy::default().appcontainer_launcher_path, None);
        let policy = SandboxPolicy {
            appcontainer_launcher_path: Some(
                "C:\\Program Files\\Agent Bridle\\agent-bridle-aclaunch.exe".to_string(),
            ),
            ..SandboxPolicy::default()
        };

        let json = serde_json::to_value(&policy).unwrap();
        assert_eq!(
            json["appcontainer_launcher_path"],
            "C:\\Program Files\\Agent Bridle\\agent-bridle-aclaunch.exe"
        );
        let policy: SandboxPolicy = serde_json::from_value(json).unwrap();
        assert_eq!(
            policy.appcontainer_launcher_path.as_deref(),
            Some("C:\\Program Files\\Agent Bridle\\agent-bridle-aclaunch.exe")
        );
    }

    #[test]
    fn net_policy_defaults_to_deny_with_no_rules() {
        let n = NetPolicy::default();
        assert_eq!(n.default, NetDefault::Deny);
        assert!(n.rules.is_empty(), "empty rules ⇒ pure exact-host behavior");
    }

    // ── Anti-drift: defaults must equal the source constants byte-for-byte, so
    //    this inert config can never silently diverge from today's behavior. ──

    #[test]
    fn gate_defaults_match_source_constants() {
        let g = GatePolicy::default();
        assert_eq!(
            g.default_strength_floor,
            crate::gate::DEFAULT_STRENGTH_FLOOR
        );
        assert_eq!(g.max_freshness_window, crate::gate::MAX_FRESHNESS_WINDOW);
    }

    #[test]
    fn sandbox_loopback_default_matches_constant() {
        assert_eq!(
            SandboxPolicy::default().loopback_hosts,
            to_vec(crate::sandbox::LOOPBACK_HOSTS)
        );
    }

    #[test]
    fn sandbox_path_and_abi_defaults_are_byte_for_byte() {
        // Byte-for-byte anti-drift for the kernel-confinement allow-lists this
        // config now owns (#144, I5-B). The old `sandbox.rs` consts were deleted,
        // so `SandboxPolicy::default()` is the single source of truth — pin its
        // exact contents (mirroring the retained `LOOPBACK_HOSTS` / gate guards)
        // so any silent edit to these security-relevant lists is caught, honoring
        // the PR's "defaults == today, byte-for-byte" claim.
        let d = SandboxPolicy::default();

        // `base_read_paths` is platform-conditional: Landlock's read base on
        // Linux, Seatbelt's on macOS.
        #[cfg(target_os = "macos")]
        let want_base = to_vec(&[
            "/usr",
            "/bin",
            "/sbin",
            "/System",
            "/Library",
            "/opt",
            "/private/etc",
            "/private/var/db/dyld",
            "/private/var/db/timezone",
            "/dev",
        ]);
        #[cfg(not(target_os = "macos"))]
        let want_base = to_vec(&[
            "/lib",
            "/lib64",
            "/lib32",
            "/libx32",
            "/usr/lib",
            "/usr/lib64",
            "/usr/libexec",
            "/usr/share",
            "/etc/ld.so.cache",
            "/etc/ld.so.preload",
            "/etc/alternatives",
            "/etc/nsswitch.conf",
            "/etc/localtime",
            "/etc/resolv.conf",
            "/etc/ssl",
            "/etc/ca-certificates",
            "/proc/self",
            "/dev/null",
            "/dev/zero",
            "/dev/full",
            "/dev/urandom",
            "/dev/random",
        ]);
        assert_eq!(d.base_read_paths.resolve(), want_base, "base_read drift");

        // `bin_read_paths` + `loader_paths` are platform-independent.
        assert_eq!(
            d.bin_read_paths.resolve(),
            to_vec(&[
                "/usr/bin",
                "/bin",
                "/usr/sbin",
                "/sbin",
                "/usr/local/bin",
                "/usr/local/sbin",
                "/opt",
            ]),
            "bin_read drift"
        );
        assert_eq!(
            d.loader_paths.resolve(),
            to_vec(&[
                "/lib64/ld-linux-x86-64.so.2",
                "/lib/ld-linux-x86-64.so.2",
                "/lib/ld-linux.so.2",
                "/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2",
                "/lib64/ld64.so.2",
                "/lib/ld-linux-aarch64.so.1",
                "/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1",
                "/lib/ld-linux-armhf.so.3",
                "/lib/ld-musl-x86-64.so.1",
                "/lib/ld-musl-aarch64.so.1",
            ]),
            "loader drift"
        );

        // ABI floors reproduce the old `ABI::V3` / `ABI::V4` constants.
        assert_eq!(d.landlock_abi_floor, 3, "fs ABI floor drift");
        assert_eq!(d.landlock_net_abi_floor, 4, "net ABI floor drift");

        // Landlock invariant (ADR 0011 D3): the read base must NOT contain the
        // executable dirs — keeping `/usr/bin` etc. out of the read set shrinks
        // the ld.so-trampoline corpus. A widening that re-admits a bin dir would
        // silently re-open it, so guard against it here. (macOS Seatbelt read
        // confinement is content-level and intentionally admits `/bin` etc.)
        #[cfg(not(target_os = "macos"))]
        for bin in ["/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
            assert!(
                !d.base_read_paths.resolve().iter().any(|p| p == bin),
                "Linux base read must exclude the executable dir {bin} (trampoline corpus)"
            );
        }
    }
}

// Model: gpt-6-astra | Harness: Codex 0.153.4 | Operator: Shawn Hartsock | Time: 22:29 UTC | Date: 2026-09-12

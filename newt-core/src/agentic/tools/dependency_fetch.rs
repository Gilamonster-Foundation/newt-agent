//! Missing-dependency recovery for the confined build lane.
//!
//! Cargo dependency resolution in the build lane is offline by design
//! (`CARGO_NET_OFFLINE`), so a `Cargo.lock` that pins a crate the
//! operator's cache lacks fails with cargo's `--offline was specified` error.
//! Nothing a model does inside the fence can fix that, and a live ornith-35b
//! refactor run spent its last 12 rounds trying until the no-progress stop
//! fired. Here the harness fetches the locked crates itself, with the
//! operator's `net` approval, and re-runs the build once. When it will not, it
//! says why, and that the operator has to act.

use std::path::Path;

use crate::agentic::permissions::{
    DenialKind, PermissionDecision, PermissionGate, PermissionRequest,
};
use crate::caveats::{Caveats, CaveatsExt as _};
use crate::confined_exec::{dependency_fetch_request, ConstrainedExecutor, CRATES_IO_FETCH_HOSTS};

/// What cargo prints when the offline lane needs a crate the cache lacks.
const OFFLINE_FETCH_NEEDED: &str = "but --offline was specified";

/// The `Cargo.lock` `source` values that name crates.io, git-index and sparse.
const CRATES_IO_SOURCES: &[&str] = &[
    "registry+https://github.com/rust-lang/crates.io-index",
    "sparse+https://index.crates.io/",
];

/// A fetch that has not finished in this long is not going to.
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Did this failed build stop because the cache lacks a locked crate?
pub(super) fn needs_dependency_fetch(stderr: &[u8]) -> bool {
    String::from_utf8_lossy(stderr).contains(OFFLINE_FETCH_NEEDED)
}

/// Is every package in `lock` either local (no `source`) or from crates.io?
/// A git or alternate-registry source could route the fetch, and any registry
/// credential, somewhere the operator's crates.io approval never covered.
fn lock_names_only_crates_io(lock: &str) -> bool {
    let Ok(lock) = lock.parse::<toml::Table>() else {
        return false;
    };
    lock.get("package")
        .and_then(toml::Value::as_array)
        .is_some_and(|packages| {
            packages.iter().all(|package| {
                package.get("source").is_none_or(|source| {
                    source
                        .as_str()
                        .is_some_and(|s| CRATES_IO_SOURCES.contains(&s))
                })
            })
        })
}

/// Why newt will not fetch for a build in `cwd` under `root`, if it will not.
///
/// A repository `.cargo/config` between `cwd` and `root` can replace the
/// crates.io source or name a credential-provider program, so its presence
/// alone refuses. `read` is the filesystem seam: `Some(contents)` for a file
/// that exists.
fn fetch_refusal(
    root: &Path,
    cwd: &Path,
    read: impl Fn(&Path) -> Option<String>,
) -> Option<&'static str> {
    let dirs: Vec<&Path> = cwd
        .ancestors()
        .take_while(|dir| dir.starts_with(root))
        .collect();
    let repo_config = dirs.iter().any(|dir| {
        ["config.toml", "config"]
            .iter()
            .any(|name| read(&dir.join(".cargo").join(name)).is_some())
    });
    if repo_config {
        return Some("the repository has its own .cargo/config, which can redirect where cargo downloads from");
    }
    match dirs.iter().find_map(|dir| read(&dir.join("Cargo.lock"))) {
        None => Some("there is no Cargo.lock to fetch against"),
        Some(lock) if !lock_names_only_crates_io(&lock) => {
            Some("Cargo.lock names a package source other than crates.io")
        }
        Some(_) => None,
    }
}

/// Fetch the crates `Cargo.lock` pins into the operator's cache, after the
/// operator approves network access to crates.io (unless `caveats` already
/// grant it). `Err` carries a reason written for the model.
pub(super) async fn fetch_locked_dependencies(
    root: &Path,
    cwd: &Path,
    caveats: &Caveats,
    permission_gate: &mut Option<&mut dyn PermissionGate>,
    read: impl Fn(&Path) -> Option<String>,
) -> Result<(), String> {
    if let Some(refusal) = fetch_refusal(root, cwd, read) {
        return Err(refusal.to_owned());
    }
    let network = fetch_network_scope_for_platform(
        root,
        cwd,
        caveats,
        permission_gate,
        cfg!(target_os = "macos"),
    )?;
    match ConstrainedExecutor::run_async(
        dependency_fetch_request(root, cwd, &network).timeout(FETCH_TIMEOUT),
    )
    .await
    {
        Ok(out) if out.success => Ok(()),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let lines: Vec<&str> = stderr.lines().collect();
            let tail = lines[lines.len().saturating_sub(6)..].join("\n");
            Err(format!("`cargo fetch --locked` failed:\n{tail}"))
        }
        Err(error) => Err(format!("`cargo fetch --locked` could not run: {error}")),
    }
}

/// Resolve the authority carried by the fetch request before spawning cargo.
fn fetch_network_scope_for_platform(
    root: &Path,
    cwd: &Path,
    caveats: &Caveats,
    permission_gate: &mut Option<&mut dyn PermissionGate>,
    macos: bool,
) -> Result<crate::Scope<String>, String> {
    if macos && caveats.net != crate::Scope::All {
        let request = dependency_fetch_request(root, cwd, &crate::Scope::All);
        let permission = PermissionRequest {
            tool: "lifecycle".into(),
            kind: DenialKind::Net,
            target: "all destinations for one cargo fetch --locked".into(),
            reason: format!(
                "Fetch the crates Cargo.lock pins in {} using the fixed `cargo fetch --locked`. macOS cannot enforce a crates.io host allowlist: this ONE fetch needs unrestricted network egress. Cargo.lock sources/checksums are checked; only the cache and confined workspace are writable. No compilation or build scripts. This does NOT grant network access to builds or the session.",
                cwd.display(),
            ),
            harness_bound: true,
        };
        let gate = permission_gate.as_deref_mut().ok_or_else(|| {
            "no operator is present to approve operation-scoped dependency-fetch egress".to_owned()
        })?;
        return match gate.ask_dependency_fetch(request.caveats(), &permission) {
            PermissionDecision::Allow(granted) if request.caveats().leq(&granted) => Ok(crate::Scope::All),
            _ => Err("the operator declined operation-scoped dependency-fetch egress or its authority ceiling forbids it".into()),
        };
    }
    let ungranted: Vec<&str> = CRATES_IO_FETCH_HOSTS
        .iter()
        .copied()
        .filter(|host| !caveats.permits_net(host))
        .collect();
    let mut network = caveats.net.clone();
    if !ungranted.is_empty() {
        let requests: Vec<PermissionRequest> = ungranted
            .iter()
            .map(|host| PermissionRequest {
                tool: "lifecycle".into(),
                kind: DenialKind::Net,
                target: (*host).into(),
                reason: format!(
                    "Fetch the crates Cargo.lock pins so the offline build can run: `cargo fetch --locked` in {}.\nDownloads from {} only, checked against Cargo.lock checksums; writes only to the Cargo registry cache. Compiles nothing and runs no build scripts.",
                    cwd.display(),
                    CRATES_IO_FETCH_HOSTS.join(" and "),
                ),
                harness_bound: false,
            })
            .collect();
        // OCAP-DANGER: dependency-fetch-egress
        // OCAP-GATE: dependency-fetch-egress (fail-closed without the operator's net grant)
        let approved = match permission_gate.as_deref_mut() {
            None => {
                return Err("no operator is present to approve network access to crates.io".into())
            }
            Some(gate) => gate.ask_with_caveats(caveats, &requests),
        };
        match approved {
            PermissionDecision::Allow(granted)
                if CRATES_IO_FETCH_HOSTS
                    .iter()
                    .all(|host| granted.permits_net(host)) =>
            {
                network = granted.net;
            }
            _ => return Err("the operator declined network access to crates.io".into()),
        }
    }
    // Keep the existing least-authority fetch for a restricted operator grant.
    // An explicit All grant must remain All: on macOS, narrowing it to a host
    // list requests a boundary Seatbelt cannot establish and L3 refuses it.
    if network != crate::Scope::All {
        network = crate::Scope::only(CRATES_IO_FETCH_HOSTS.iter().map(|host| (*host).to_owned()));
    }
    Ok(network)
}

/// Appended to the re-run build's output, so the model knows what happened.
pub(super) const FETCHED_NOTE: &str =
    "note: the local Cargo cache was missing crates this Cargo.lock pins; newt fetched them (`cargo fetch --locked`) and re-ran the build above.";

/// Appended when the harness could not fetch. Report the actual failure while
/// retaining the normal permission/setup recovery path; do not retry here.
pub(super) fn blocked_note(reason: &str, cwd: &Path) -> String {
    format!(
        "note: this build needs locked dependencies missing from the local Cargo cache. Automatic `cargo fetch --locked` failed in {}: {reason}. Request the needed permission or report the setup failure.",
        cwd.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agentic::permissions::HumanQuestionOutcome;
    use std::collections::HashMap;
    use std::path::PathBuf;

    const CRATES_IO_LOCK: &str = r#"
version = 4

[[package]]
name = "app"
version = "0.1.0"

[[package]]
name = "agent-bridle"
version = "0.7.10"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "00"
"#;

    fn files(entries: &[(&str, &str)]) -> impl Fn(&Path) -> Option<String> {
        let map: HashMap<PathBuf, String> = entries
            .iter()
            .map(|(path, body)| (PathBuf::from(path), (*body).to_owned()))
            .collect();
        move |path| map.get(path).cloned()
    }

    #[test]
    fn recognises_cargo_offline_download_failure() {
        let stderr = b"error: failed to download `agent-bridle v0.7.10`\n\nCaused by:\n  attempting to make an HTTP request, but --offline was specified\n";
        assert!(needs_dependency_fetch(stderr));
        assert!(!needs_dependency_fetch(
            b"error[E0425]: cannot find value `x`"
        ));
    }

    #[test]
    fn crates_io_and_local_packages_are_fetchable() {
        assert!(lock_names_only_crates_io(CRATES_IO_LOCK));
        let sparse = CRATES_IO_LOCK.replace(
            "registry+https://github.com/rust-lang/crates.io-index",
            "sparse+https://index.crates.io/",
        );
        assert!(lock_names_only_crates_io(&sparse));
    }

    #[test]
    fn git_or_alternate_registry_sources_are_not() {
        let git = format!(
            "{CRATES_IO_LOCK}\n[[package]]\nname = \"g\"\nversion = \"0.1.0\"\nsource = \"git+https://example.com/g?rev=1#1\"\n"
        );
        assert!(!lock_names_only_crates_io(&git));
        let alt = CRATES_IO_LOCK.replace(
            "registry+https://github.com/rust-lang/crates.io-index",
            "registry+https://registry.example.com/index",
        );
        assert!(!lock_names_only_crates_io(&alt));
        assert!(!lock_names_only_crates_io("not toml ["));
    }

    #[test]
    fn nested_cwd_finds_the_workspace_lock() {
        let read = files(&[("/ws/Cargo.lock", CRATES_IO_LOCK)]);
        assert_eq!(
            fetch_refusal(Path::new("/ws"), Path::new("/ws/newt-core"), read),
            None
        );
    }

    #[test]
    fn a_repository_cargo_config_refuses() {
        let read = files(&[
            ("/ws/Cargo.lock", CRATES_IO_LOCK),
            (
                "/ws/.cargo/config.toml",
                "[source.crates-io]\nreplace-with = \"x\"\n",
            ),
        ]);
        let refusal = fetch_refusal(Path::new("/ws"), Path::new("/ws/newt-core"), read);
        assert!(refusal.is_some_and(|r| r.contains(".cargo/config")));
    }

    #[test]
    fn config_and_lock_outside_the_workspace_are_ignored() {
        let read = files(&[
            ("/Cargo.lock", CRATES_IO_LOCK),
            (
                "/.cargo/config.toml",
                "[source.crates-io]\nreplace-with = \"x\"\n",
            ),
        ]);
        let refusal = fetch_refusal(Path::new("/ws"), Path::new("/ws/sub"), read);
        assert_eq!(refusal, Some("there is no Cargo.lock to fetch against"));
    }

    struct Answer(bool, usize);
    impl PermissionGate for Answer {
        fn ask(&mut self, requests: &[PermissionRequest]) -> PermissionDecision {
            self.1 += 1;
            assert!(requests.iter().all(|r| r.kind == DenialKind::Net));
            if self.0 {
                PermissionDecision::Allow(Caveats::top())
            } else {
                PermissionDecision::Deny
            }
        }
        fn ask_question(&mut self, _: &str) -> HumanQuestionOutcome {
            HumanQuestionOutcome::Unavailable
        }
    }

    struct FetchApproval {
        allow: bool,
        calls: usize,
    }
    impl PermissionGate for FetchApproval {
        fn ask(&mut self, _: &[PermissionRequest]) -> PermissionDecision {
            panic!("#2731 macOS must ask for the exact fetch, not host grants")
        }
        fn ask_dependency_fetch(
            &mut self,
            prepared: &Caveats,
            request: &PermissionRequest,
        ) -> PermissionDecision {
            self.calls += 1;
            assert_eq!(request.tool, "lifecycle");
            assert_eq!(request.kind, DenialKind::Net);
            assert!(request.reason.contains("unrestricted"));
            assert!(request.reason.contains("cargo fetch --locked"));
            if self.allow {
                PermissionDecision::Allow(prepared.clone())
            } else {
                PermissionDecision::Deny
            }
        }
        fn ask_question(&mut self, _: &str) -> HumanQuestionOutcome {
            HumanQuestionOutcome::Unavailable
        }
    }

    /// #2731: approving a default-permission Mac fetch must produce a runnable
    /// operation fence without replacing the invocation's host-only authority.
    #[test]
    fn macos_fetch_approval_is_operation_scoped() {
        let invocation = no_net();
        let before = invocation.clone();
        let mut gate = FetchApproval {
            allow: true,
            calls: 0,
        };
        let mut slot: Option<&mut dyn PermissionGate> = Some(&mut gate);
        let network = fetch_network_scope_for_platform(
            Path::new("/ws"),
            Path::new("/ws/sub"),
            &invocation,
            &mut slot,
            true,
        )
        .unwrap();
        assert_eq!(network, crate::Scope::All);
        assert_eq!(invocation, before);
        assert_eq!(gate.calls, 1);
    }

    /// #2731: declining the explicit operation approval never reaches fetch.
    #[test]
    fn macos_fetch_denial_does_not_fall_back_to_host_grants() {
        let mut gate = FetchApproval {
            allow: false,
            calls: 0,
        };
        let mut slot: Option<&mut dyn PermissionGate> = Some(&mut gate);
        assert!(fetch_network_scope_for_platform(
            Path::new("/ws"),
            Path::new("/ws"),
            &no_net(),
            &mut slot,
            true
        )
        .is_err());
        assert_eq!(gate.calls, 1);
    }

    fn no_net() -> Caveats {
        crate::confined_exec::build_tool_caveats(Path::new("/ws"))
    }

    #[test]
    fn dependency_fetch_does_not_restore_a_broader_gate_network_baseline() {
        // A legacy gate can return its broader session baseline on approval.
        // This invocation holds one crates.io host and an unrelated host; the
        // pending permission adds only the missing crates.io host, never All.
        let mut invocation = no_net();
        invocation.net = crate::Scope::only([
            CRATES_IO_FETCH_HOSTS[0].to_owned(),
            "existing.example.test".to_owned(),
        ]);
        let mut gate = Answer(true, 0);
        let mut slot: Option<&mut dyn PermissionGate> = Some(&mut gate);
        let network = fetch_network_scope_for_platform(
            Path::new("/ws"),
            Path::new("/ws"),
            &invocation,
            &mut slot,
            false,
        )
        .unwrap();
        let request = dependency_fetch_request(Path::new("/ws"), Path::new("/ws"), &network);
        assert_eq!(
            request.caveats().net,
            crate::Scope::only(CRATES_IO_FETCH_HOSTS.iter().map(|host| (*host).to_owned())),
            "approving the missing fetch host must not restore the gate's All scope"
        );
        assert!(!request.caveats().permits_net("unrequested.example.test"));
        assert_eq!(gate.1, 1);
    }

    #[test]
    fn dependency_fetch_keeps_an_existing_unrestricted_grant_without_prompting() {
        let mut invocation = no_net();
        invocation.net = crate::Scope::All;
        let mut gate = Answer(false, 0);
        let mut slot: Option<&mut dyn PermissionGate> = Some(&mut gate);
        let network = fetch_network_scope_for_platform(
            Path::new("/ws"),
            Path::new("/ws"),
            &invocation,
            &mut slot,
            false,
        )
        .unwrap();
        let request = dependency_fetch_request(Path::new("/ws"), Path::new("/ws"), &network);
        assert_eq!(request.caveats().net, crate::Scope::All);
        assert_eq!(gate.1, 0);
    }

    #[tokio::test]
    async fn operator_denial_fails_closed_before_any_fetch() {
        let mut gate = Answer(false, 0);
        let mut slot: Option<&mut dyn PermissionGate> = Some(&mut gate);
        let read = files(&[("/ws/Cargo.lock", CRATES_IO_LOCK)]);
        let result = fetch_locked_dependencies(
            Path::new("/ws"),
            Path::new("/ws"),
            &no_net(),
            &mut slot,
            read,
        )
        .await;
        assert_eq!(
            result,
            Err(if cfg!(target_os = "macos") { "the operator declined operation-scoped dependency-fetch egress or its authority ceiling forbids it" } else { "the operator declined network access to crates.io" }.into())
        );
        assert_eq!(gate.1, usize::from(!cfg!(target_os = "macos")));
    }

    #[tokio::test]
    async fn no_operator_fails_closed() {
        let mut slot: Option<&mut dyn PermissionGate> = None;
        let read = files(&[("/ws/Cargo.lock", CRATES_IO_LOCK)]);
        let result = fetch_locked_dependencies(
            Path::new("/ws"),
            Path::new("/ws"),
            &no_net(),
            &mut slot,
            read,
        )
        .await;
        assert!(result.is_err_and(|reason| reason.contains("no operator")));
    }

    #[tokio::test]
    async fn a_refused_plan_never_asks_the_operator() {
        let mut gate = Answer(true, 0);
        let mut slot: Option<&mut dyn PermissionGate> = Some(&mut gate);
        let result = fetch_locked_dependencies(
            Path::new("/ws"),
            Path::new("/ws"),
            &no_net(),
            &mut slot,
            files(&[]),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(gate.1, 0);
    }

    #[test]
    fn blocked_note_reports_the_failure_and_keeps_permission_recovery_available() {
        let note = blocked_note(
            "the operator declined network access to crates.io",
            Path::new("/ws/newt-core"),
        );
        assert!(note.contains("cargo fetch --locked"));
        assert!(note.contains("/ws/newt-core"));
        assert!(note.contains("the operator declined network access to crates.io"));
        assert!(note.contains("Request the needed permission or report the setup failure"));
        assert!(!note.contains("Nothing run from inside the sandbox can fix this"));
        assert!(!note.contains("Stop and tell the operator"));
    }
    /// #2731 real-resource proof of the mocked approval contract: Seatbelt
    /// admits the approved fixed fetch, fills a missing crate archive, and an
    /// offline Cargo check then succeeds. Run the compiled test binary with a
    /// disposable CARGO_HOME containing only a copied public registry index.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "native macOS, explicit scratch CARGO_HOME, crates.io downloads"]
    async fn native_macos_cold_cache_fetch_and_offline_rebuild() {
        let cache = std::env::var("CARGO_HOME").expect("isolated CARGO_HOME");
        assert_eq!(std::env::var("NEWT_COLD_CACHE_PROOF").unwrap(), cache);
        assert!(Path::new(&cache).is_absolute());
        assert!(!Path::new(&cache).join("registry/cache").exists());
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn value() -> String { itoa::Buffer::new().format(42).to_owned() }\n",
        )
        .unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname=\"fetch-proof\"\nversion=\"0.1.0\"\nedition=\"2021\"\n[dependencies]\nitoa=\"=1.0.18\"\n").unwrap();
        std::fs::write(
            root.join("Cargo.lock"),
            r#"version = 4
[[package]]
name = "fetch-proof"
version = "0.1.0"
dependencies = ["itoa"]
[[package]]
name = "itoa"
version = "1.0.18"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "8f42a60cbdf9a97f5d2305f08a87dc4e09308d1276d28c869c684d7777685682"
"#,
        )
        .unwrap();
        let invocation = crate::confined_exec::build_tool_caveats(&root);
        let build = crate::confined_exec::build_tool_request(
            &root,
            &root,
            "cargo",
            ["check", "--locked", "--offline"],
            &invocation.net,
        )
        .timeout(std::time::Duration::from_secs(90));
        let before = ConstrainedExecutor::run_async(build.clone()).await.unwrap();
        assert!(!before.success);
        assert!(
            needs_dependency_fetch(&before.stderr),
            "{}",
            String::from_utf8_lossy(&before.stderr)
        );
        let mut denied = FetchApproval {
            allow: false,
            calls: 0,
        };
        let mut slot: Option<&mut dyn PermissionGate> = Some(&mut denied);
        assert!(
            fetch_locked_dependencies(&root, &root, &invocation, &mut slot, |p| {
                std::fs::read_to_string(p).ok()
            })
            .await
            .is_err()
        );
        assert!(!Path::new(&cache).join("registry/cache").exists());
        let mut approved = FetchApproval {
            allow: true,
            calls: 0,
        };
        let mut slot: Option<&mut dyn PermissionGate> = Some(&mut approved);
        fetch_locked_dependencies(&root, &root, &invocation, &mut slot, |p| {
            std::fs::read_to_string(p).ok()
        })
        .await
        .expect("approved fetch must actually spawn and download");
        let after = ConstrainedExecutor::run_async(build).await.unwrap();
        assert!(after.success, "{}", String::from_utf8_lossy(&after.stderr));
        assert_eq!(approved.calls, 1);
        assert_eq!(denied.calls, 1);
        assert_eq!(invocation.net, crate::Scope::none());
        println!("cold offline check failed; denied fetch wrote no archives; approved locked fetch downloaded itoa; offline rebuild passed; session net unchanged");
    }
}

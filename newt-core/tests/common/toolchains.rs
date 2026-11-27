//! Serial host-resource grounding for the signed build-only policy. No real key/config.
use agent_mesh_protocol::UserKey;
use content_addressable::ContentAddressable;
use newt_core::confined_exec::{build_tool_caveats, build_tool_request, ConstrainedExecutor};
use newt_core::{execute_tool, Caveats, NoMcp, Scope};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const TESTS: [&str; 2] = [
    "script_parse_failure_inventory_only_decreases",
    "the_script_detector_catches_a_synthetic_known_positive",
];

fn argument(prefix: &str) -> PathBuf {
    std::env::args()
        .find_map(|arg| arg.strip_prefix(prefix).map(PathBuf::from))
        .unwrap_or_else(|| panic!("the explicit acceptance run requires {prefix}<path>"))
        .canonicalize()
        .unwrap()
}

async fn brush(workspace: &Path, source: &str, caveats: &Caveats) -> String {
    execute_tool(
        "run_command",
        &serde_json::json!({"command": source}),
        workspace.to_str().unwrap(),
        false,
        0,
        caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
}

fn argv(
    workspace: &Path,
    program: &str,
    args: &[&str],
    writes: &Scope<String>,
) -> Result<newt_core::confined_exec::ConfinedOutput, newt_core::confined_exec::ExecRefused> {
    let request = build_tool_request(
        workspace,
        workspace,
        program,
        args.iter().copied(),
        &Scope::none(),
    )
    .with_operator_toolchains(writes)?;
    ConstrainedExecutor::run(&request)
}

pub fn run() {
    let node = argument("--node-source=");
    let tests = argument("--node-test=");
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .canonicalize()
        .unwrap();
    // /var/tmp survives the build's private /tmp and /dev mounts. This owned,
    // automatically removed fixture is outside the entire model write scope.
    let fixture = tempfile::tempdir_in("/var/tmp").unwrap();
    std::fs::set_permissions(fixture.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let root = fixture.path().join("curated");
    let config = fixture.path().join("config");
    for dir in [root.join("bin"), config.join("ocap")] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::copy(node, root.join("bin/node")).unwrap();
    std::fs::copy("/usr/bin/printf", root.join("bin/toolchain-probe")).unwrap();
    for path in [&root, &root.join("bin"), &config, &config.join("ocap")] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let probe = root.join("bin/toolchain-probe");
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let baseline = build_tool_caveats(&workspace);
    let probe_source = format!(
        "cargo build --help >/dev/null && '{}' CONFIGURED_TOOL_OK",
        probe.display()
    );
    let node_path = format!("{}/bin:/usr/bin:/bin", root.display());
    runtime.block_on(async {
        let absent = argv(&workspace, probe.to_str().unwrap(), &["UNCONFIGURED_MUST_NOT_RUN"], &baseline.fs_write);
        assert!(absent.is_err() || !absent.unwrap().success);
        let text = brush(&workspace, &probe_source, &baseline).await;
        assert!(!text.contains("CONFIGURED_TOOL_OK"), "unconfigured tool ran: {text}");
        for test in TESTS {
            let out = ConstrainedExecutor::run(&build_tool_request(&workspace, &workspace, tests.to_str().unwrap(), [test, "--exact", "--nocapture", "--test-threads=1"], &Scope::none()).env("PATH", &node_path)).unwrap();
            assert!(!out.success, "unconfigured Node unexpectedly ran");
            println!("NODE_UNCONFIGURED argv {test}: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            let source = format!("cargo build --help >/dev/null && PATH='{node_path}' '{}' {test} --exact --nocapture --test-threads=1", tests.display());
            let text = brush(&workspace, &source, &baseline).await;
            assert!(text.contains("FAILED"), "baseline did not exercise the test: {text}");
            println!("NODE_UNCONFIGURED Brush {test}: {text}");
        }
    });
    let tree = newt_core::tool_tree::digest(&root).unwrap().cid;
    let body = newt_core::toolchains::Body {
        schema: "newt.build-toolchains/v1".into(),
        roots: vec![newt_core::toolchains::Root {
            path: root.to_string_lossy().into_owned(),
            expected_tree: Some(tree),
        }],
    };
    let key = UserKey::generate();
    let signature =
        newt_core::ocap_store::sign_entry(&body.canonical_form().unwrap(), &|payload| {
            key.sign(payload).to_bytes()
        });
    let policy_file = config.join("ocap/toolchains.toml");
    std::fs::write(
        &policy_file,
        format!("sig = \"{signature}\"\n{}", toml::to_string(&body).unwrap()),
    )
    .unwrap();
    key.save(&config.join("identity.pem")).unwrap();
    for file in [&policy_file, &config.join("identity.pem")] {
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    newt_core::toolchains::initialize(
        Some(config.clone()),
        Some(fixture.path().join("home")),
        |_| Ok(key.public().as_bytes()),
    );
    // A second initialization and every child-side override must be inert.
    newt_core::toolchains::initialize(Some(workspace.clone()), None, |_| panic!("policy reloaded"));
    assert_eq!(
        build_tool_caveats(&workspace).fs_read,
        baseline.fs_read,
        "ordinary/session defaults widened"
    );
    runtime.block_on(async {
        let out = argv(&workspace, "toolchain-probe", &["CONFIGURED_TOOL_OK"], &baseline.fs_write).unwrap();
        assert!(out.success, "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(out.toolchains.len(), 1);
        assert_eq!(out.toolchains[0].observed_tree, Some(tree));
        let text = brush(&workspace, "cargo build --help >/dev/null && toolchain-probe CONFIGURED_TOOL_OK", &baseline).await;
        assert!(text.contains("CONFIGURED_TOOL_OK") && text.contains("Build toolchains (configured, host verified)"), "{text}");
        println!("CONFIGURED_TOOL_BOTH_ROUTES: verified, read-only");
        let saved_policy = std::fs::read(&policy_file).unwrap();
        std::fs::write(&policy_file, "unsigned = true").unwrap();
        assert!(argv(&workspace, "toolchain-probe", &["MUST_NOT_RUN"], &baseline.fs_write).is_err());
        let changed = brush(&workspace, &probe_source, &baseline).await;
        assert!(changed.contains("policy changed since startup"), "{changed}");
        std::fs::write(&policy_file, saved_policy).unwrap();
        let unprepared = build_tool_request(&workspace, &workspace, "toolchain-probe", ["MUST_NOT_RUN"], &Scope::none());
        assert!(ConstrainedExecutor::run(&unprepared).is_err());
        let overridden = build_tool_request(&workspace, &workspace, "toolchain-probe", ["FROZEN_POLICY"], &Scope::none())
            .env("NEWT_CONFIG_DIR", "/model/injected").env("NEWT_CONFIG", "/model/config.toml")
            .with_operator_toolchains(&baseline.fs_write).unwrap();
        let out = ConstrainedExecutor::run(&overridden).unwrap();
        assert!(out.success && out.toolchains[0].policy == body.content_id().unwrap());
        println!("POLICY_REPLACEMENT_REFUSED_BOTH_ROUTES; CHILD_ENV_CANNOT_REPLACE_POLICY");
        if std::env::args().any(|arg| arg.starts_with("--signer-binary=")) {
            confined_signing_refused(&workspace, &config, &baseline).await;
        }


        for test in TESTS {
            let out = argv(&workspace, tests.to_str().unwrap(), &[test, "--exact", "--nocapture", "--test-threads=1"], &baseline.fs_write).unwrap();
            assert!(out.success, "{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            println!("NODE_CONFIGURED argv {test}: {}", String::from_utf8_lossy(&out.stdout));
            let source = format!("cargo build --help >/dev/null && '{}' {test} --exact --nocapture --test-threads=1", tests.display());
            let text = brush(&workspace, &source, &baseline).await;
            assert!(text.contains("1 passed; 0 failed") && text.contains("Build toolchains (configured, host verified)"), "{text}");
            println!("NODE_CONFIGURED Brush {test}: {text}");
        }
        let secret = fixture.path().join("unrelated-secret");
        std::fs::write(&secret, "MUST_NOT_READ").unwrap();
        let script = format!("const fs=require('node:fs'); for(const [p,write] of [[{},false],[{},true]]) {{ try {{ if(write) fs.writeFileSync(p,'bad'); else fs.readFileSync(p); process.exit(20); }} catch(e) {{ if(!['EACCES','EPERM'].includes(e.code)) throw e; }} }} console.log('ROOT_READONLY_AND_SECRET_DENIED')", serde_json::to_string(&secret).unwrap(), serde_json::to_string(&root.join("must-not-write")).unwrap());
        let out = argv(&workspace, "node", &["-e", &script], &baseline.fs_write).unwrap();
        assert!(out.success, "{}", String::from_utf8_lossy(&out.stderr));
        let script_file = tempfile::NamedTempFile::new_in(workspace.join("target")).unwrap();
        std::fs::write(script_file.path(), script).unwrap();
        let ordinary = brush(&workspace, "node --version", &baseline).await;
        assert!(ordinary.contains("Permission denied") && !ordinary.contains("Build toolchains (configured, host verified)"), "ordinary shell widened: {ordinary}");
        let makefile = tempfile::NamedTempFile::new_in(workspace.join("target")).unwrap();
        std::fs::write(makefile.path(), format!(".PHONY: probe\nprobe:\n\tnode '{}'\n", script_file.path().display())).unwrap();
        let text = brush(&workspace, &format!("make -f '{}' probe && printf BUILD_DESCENDANT_DONE", makefile.path().display()), &baseline).await;
        assert!(text.contains("ROOT_READONLY_AND_SECRET_DENIED") && text.contains("Build toolchains (configured, host verified)"), "{text}");
        assert!(!root.join("must-not-write").exists());
        let prepared = build_tool_request(&workspace, &workspace, probe.to_str().unwrap(), ["REPLACEMENT_MUST_NOT_RUN"], &Scope::none()).with_operator_toolchains(&baseline.fs_write).unwrap();
        let old = fixture.path().join("original");
        std::fs::rename(&root, &old).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::copy("/usr/bin/printf", &probe).unwrap();
        let replaced = ConstrainedExecutor::run(&prepared);
        assert!(replaced.is_err() || !replaced.unwrap().success, "held root redirected to replacement");
        std::fs::remove_dir_all(&root).unwrap();
        std::fs::rename(old, &root).unwrap();
        println!("HELD_ROOT_REPLACEMENT_REFUSED; ROOT_READONLY_AND_SECRET_DENIED_BOTH_ROUTES");
        for write in [Scope::All, Scope::only([fixture.path().to_string_lossy().into_owned()]), Scope::only([root.join("cache/not-created").to_string_lossy().into_owned()])] {
            assert!(argv(&workspace, "toolchain-probe", &["MUST_NOT_RUN"], &write).is_err());
            let mut writable = baseline.clone();
            writable.fs_write = match write {
                Scope::Only(mut roots) => { if let Scope::Only(base) = &baseline.fs_write { roots.extend(base.iter().cloned()); } Scope::Only(roots) },
                Scope::All => Scope::All,
            };
            let text = brush(&workspace, &probe_source, &writable).await;
            assert!(text.contains("refused before spawn"), "{text}");
        }
        for failure in ["absent", "writable", "identity"] {
            match failure {
                "absent" => std::fs::rename(&root, fixture.path().join("held-away")).unwrap(),
                "writable" => std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o770)).unwrap(),
                _ => {
                    std::fs::write(root.join("changed"), "changed tree").unwrap();
                    std::fs::set_permissions(root.join("changed"), std::fs::Permissions::from_mode(0o600)).unwrap();
                }
            }
            assert!(argv(&workspace, "toolchain-probe", &["MUST_NOT_RUN"], &baseline.fs_write).is_err());
            let text = brush(&workspace, &probe_source, &baseline).await;
            assert!(text.contains("refused before spawn"), "{failure}: {text}");
            if failure == "identity" { assert!(text.contains("content identity mismatch"), "{text}"); }
            println!("REFUSED_BOTH_ROUTES: {failure}");
            match failure {
                "absent" => std::fs::rename(fixture.path().join("held-away"), &root).unwrap(),
                "writable" => std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap(),
                _ => std::fs::remove_file(root.join("changed")).unwrap(),
            }
        }
    });
}

// A private PTY is deliberately available to builds; it cannot substitute for
// authority over the parent's protected policy and real disposable signing key.
async fn confined_signing_refused(workspace: &Path, config: &Path, caveats: &Caveats) {
    let signer = argument("--signer-binary=");
    let quote = |s: &str| format!("'{}'", s.replace('\'', "'\"'\"'"));
    let policy = config.join("ocap/toolchains.toml");
    let key = config.join("identity.pem");
    let access_probe = format!(
        "const fs=require('node:fs'); for (const [p,w] of [[{},false],[{},true]]) {{ try {{ if(w) fs.writeFileSync(p,'unauthorized'); else fs.readFileSync(p); process.exit(20); }} catch(e) {{ if(!['EACCES','EPERM'].includes(e.code)) throw e; }} }} console.log('KEY_POLICY_ACCESS_DENIED')",
        serde_json::to_string(&key).unwrap(), serde_json::to_string(&policy).unwrap(),
    );
    let command = format!(
        "test -t 0 && test -t 1 && node -e {} && {} --config-dir {} doctor --sign-toolchains",
        quote(&access_probe),
        quote(signer.to_str().unwrap()),
        quote(config.to_str().unwrap()),
    );
    let before_policy = std::fs::read(&policy).unwrap();
    let before_key = std::fs::read(&key).unwrap();
    for (inner, positive) in [
        (
            "test -t 0 && test -t 1 && printf PRIVATE_SIGNER_PTY_OK",
            true,
        ),
        (command.as_str(), false),
    ] {
        // Supply an affirmative answer to the private terminal as an attacker
        // could. Refusal must come from protected state, not stdin being a pipe.
        let source = format!(
            "printf 'y\\n' | /usr/bin/script -q -e -c {} /dev/null",
            quote(inner)
        );
        let request = build_tool_request(
            workspace,
            workspace,
            "/bin/sh",
            ["-c", &source],
            &Scope::none(),
        )
        .timeout(std::time::Duration::from_secs(30))
        .with_operator_toolchains(&caveats.fs_write)
        .unwrap();
        let out = ConstrainedExecutor::run(&request).unwrap();
        let argv_text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let brush_text = brush(
            workspace,
            &format!("cargo build --help >/dev/null && {source}"),
            caveats,
        )
        .await;
        for (route, text) in [("argv", argv_text), ("Brush", brush_text)] {
            if positive {
                assert!(text.contains("PRIVATE_SIGNER_PTY_OK"), "{route}: {text}");
            } else {
                assert!(
                    text.contains("Permission denied")
                        && text.contains("KEY_POLICY_ACCESS_DENIED")
                        && text.contains("cannot read operator toolchain policy")
                        && !text.contains("Signed toolchain policy"),
                    "{route}: {text}"
                );
                assert_eq!(std::fs::read(&policy).unwrap(), before_policy);
                assert_eq!(std::fs::read(&key).unwrap(), before_key);
                println!("MODEL_SIGNER_REFUSED {route}: private PTY works; policy read/write and key read denied; protected bytes unchanged");
            }
        }
        if positive {
            assert!(out.success, "private PTY control did not execute");
        } else {
            assert!(
                !out.success && !out.timed_out,
                "signer did not refuse promptly"
            );
        }
    }
}

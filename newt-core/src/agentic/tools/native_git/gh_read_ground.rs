// Included inside the existing PR-create broker's isolated resource fixture.

/// Grounds the in-memory dispatch, host pinning, argv, denial, and output
/// tests with authenticated executable resolution and a real child. The child
/// is a stand-in gh: no live forge is contacted. Serial weekly/release tier.
#[test]
#[ignore = "real filesystem and stand-in gh subprocess; weekly/release lane"]
fn gh_read_real_dispatch_pins_host_and_isolates_config() {
    let env = BrokerEnv::new();
    let repo = repo_on_feature_branch();
    let config = env.home.path().join(".config/gh");
    std::fs::create_dir_all(&config).unwrap();
    for dir in [&config, config.parent().unwrap()] {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    std::fs::write(
        config.join("hosts.yml"),
        "github.com:\n  oauth_token: fixture-token\notherhost:\n  oauth_token: wrong-token\n",
    )
    .unwrap();
    std::fs::write(
        config.join("config.yml"),
        "aliases:\n  issue: '!touch forbidden'\nhttp_unix_socket: /never/use\n",
    )
    .unwrap();
    std::fs::set_permissions(
        config.join("hosts.yml"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let record = env.home.path().join("read-argv");
    std::fs::write(
        env.gh(),
        format!(
            r#"#!/bin/sh
[ "$GH_HOST" = github.com ] || exit 21
[ "$GH_TOKEN" = fixture-token ] || exit 22
[ -z "$GH_ENTERPRISE_TOKEN" ] || exit 23
# macOS /tmp aliases can spell the same directory differently.
[ "$HOME" -ef "$PWD" ] || exit 24
[ "$GH_CONFIG_DIR" -ef "$PWD" ] || exit 25
[ -S "$GH_CONFIG_DIR/http.sock" ] || exit 26
# The authenticated PATH need not include /bin, where macOS installs cat.
[ "$(/bin/cat "$GH_CONFIG_DIR/config.yml")" = "http_unix_socket: $GH_CONFIG_DIR/http.sock" ] || exit 28
[ -z "$GH_REPO" ] || exit 27
printf '%s\n' "$@" > '{}'
echo 'issue 1'
echo '{}' >&2
"#,
            record.display(),
            CANARY
        ),
    )
    .unwrap();
    let mut gate = Gate {
        allow: false,
        requests: vec![],
    };
    let caveats = Caveats {
        net: Scope::only(["github.com".into(), "api.github.com".into()]),
        ..read_only(&[repo.path(), config.as_path()])
    };
    let (out, terminal) =
        dispatch_run_command_with_gate(repo.path(), "gh issue view 1", &caveats, Some(&mut gate));
    assert!(out.contains("issue 1"), "{out}");
    assert!(gate.requests.is_empty());
    assert!(!out.contains(CANARY) && !terminal.contains(CANARY));
    assert_eq!(
        std::fs::read_to_string(&record).unwrap(),
        "issue\nview\n1\n--repo\ngithub.com/o/r\n"
    );
    std::fs::remove_file(&record).unwrap();
    let (out, _) = dispatch_run_command_with_gate(
        repo.path(),
        "gh issue view 1 -R otherhost/o/r",
        &caveats,
        Some(&mut gate),
    );
    assert!(out.contains("refused"), "{out}");
    assert_eq!(gate.requests.len(), 1);
    assert_eq!(gate.requests[0].target, "otherhost");
    assert!(!record.exists(), "denied destination must not spawn gh");
    std::fs::write(
        env.gh(),
        format!(
            "#!/bin/sh\nprintf executed > '{}'\necho '{CANARY}' >&2\nexit 7\n",
            record.display()
        ),
    )
    .unwrap();
    let (out, terminal) = dispatch_run_command_with_gate(
        repo.path(),
        "gh issue view 1 -R o/r",
        &caveats,
        Some(&mut gate),
    );
    assert!(out.contains("refused"), "{out}");
    assert!(!out.contains(CANARY) && !terminal.contains(CANARY));
    assert_eq!(std::fs::read_to_string(&record).unwrap(), "executed");
    std::fs::remove_file(&record).unwrap();
    for code in [1, 8] {
        std::fs::write(
            env.gh(),
            format!(
            "#!/bin/sh\nprintf 'build pending\\nlint failed\\n'\necho '{CANARY}' >&2\nexit {code}\n"
        ),
        )
        .unwrap();
        for (wrapper, expected) in [
            ("", "build pending\nlint failed".to_string()),
            ("; echo status=$?", format!("status={code}")),
            (
                " | head -1; echo status=$?",
                "build pending\nstatus=0".into(),
            ),
        ] {
            let (out, terminal) = dispatch_run_command_with_gate(
                repo.path(),
                &format!("gh pr checks 1 -R o/r{wrapper}"),
                &caveats,
                Some(&mut gate),
            );
            assert!(out.contains(&expected), "{out}");
            assert!(out.contains(&format!("(exit {code})")), "{out}");
            assert!(!out.contains(CANARY) && !terminal.contains(CANARY));
            assert!(!out.contains("credentials"), "{out}");
        }
    }
    // Restore a marker-writing child before the writable-executable refusal.
    std::fs::write(
        env.gh(),
        format!("#!/bin/sh\nprintf executed > '{}'\n", record.display()),
    )
    .unwrap();
    // Resolver sees a real executable in a model-writable directory and refuses.
    let caveats = Caveats {
        fs_write: Scope::only([env.bin.path().to_string_lossy().into_owned()]),
        ..caveats
    };
    let (out, _) = dispatch_run_command_with_gate(
        repo.path(),
        "gh issue view 1 -R o/r",
        &caveats,
        Some(&mut gate),
    );
    assert!(out.contains("refused"), "{out}");
    assert!(!record.exists(), "writable gh must not execute");
}

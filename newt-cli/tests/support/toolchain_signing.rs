use crate::common;
use content_addressable::ContentAddressable;
use newt_core::ocap_policy::Ed25519ApproveVerifier;
use newt_core::toolchains::{Body, Root};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;

struct Fixture {
    _owner: tempfile::TempDir,
    home: PathBuf,
    config: PathBuf,
    policy: PathBuf,
    body: Body,
    original: String,
    key: newt_identity::UserKey,
}

fn private(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

impl Fixture {
    fn new() -> Self {
        // Curated roots must survive private /tmp and /dev mounts. Every path
        // and key belongs to this fixture; no operator configuration is read.
        let owner = tempfile::tempdir_in("/var/tmp").unwrap();
        let home = owner.path().canonicalize().unwrap();
        private(&home, 0o700);
        let config = home.join("config");
        for path in [config.clone(), config.join("ocap"), home.join("workspace")] {
            std::fs::create_dir(&path).unwrap();
            private(&path, 0o700);
        }
        let model_config = home.join("workspace/model.toml");
        std::fs::write(&model_config, "# model/session configuration\n").unwrap();
        private(&model_config, 0o600);
        let decoy_dir = home.join("workspace/ocap");
        std::fs::create_dir(&decoy_dir).unwrap();
        private(&decoy_dir, 0o700);
        let decoy = decoy_dir.join("toolchains.toml");
        std::fs::write(&decoy, "# never read or signed through --config\n").unwrap();
        private(&decoy, 0o600);
        let mut roots = Vec::new();
        for name in ["pinned", "unpinned"] {
            let path = home.join(name);
            std::fs::create_dir(&path).unwrap();
            std::fs::create_dir(path.join("bin")).unwrap();
            private(&path, 0o700);
            private(&path.join("bin"), 0o700);
            let tool = path.join("bin/tool");
            std::fs::write(&tool, "#!/bin/sh\nprintf approved\n").unwrap();
            private(&tool, 0o500);
            roots.push(Root {
                path: path.to_str().unwrap().to_owned(),
                expected_tree: (name == "pinned")
                    .then(|| newt_core::tool_tree::digest(&path).unwrap().cid),
            });
        }
        let body = Body {
            schema: "newt.build-toolchains/v1".into(),
            roots,
        };
        let key = newt_identity::UserKey::generate();
        key.save(&config.join("identity.pem")).unwrap();
        private(&config.join("identity.pem"), 0o600);
        let policy = config.join("ocap/toolchains.toml");
        let original = format!(
            "# preserve this exact operator note\n{}",
            toml::to_string(&body).unwrap()
        );
        std::fs::write(&policy, &original).unwrap();
        private(&policy, 0o600);
        Self {
            _owner: owner,
            home,
            config,
            policy,
            body,
            original,
            key,
        }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_newt"));
        common::isolate(&mut cmd, &self.home);
        cmd.current_dir(self.home.join("workspace"))
            .arg("--config-dir")
            .arg(&self.config)
            // Explicit model/session config must not select the signing store.
            .arg("--config")
            .arg(self.home.join("workspace/model.toml"))
            .args(["doctor", "--sign-toolchains"])
            .env("TERM", "xterm-256color")
            .kill_on_drop(true);
        cmd
    }

    fn backups(&self) -> Vec<PathBuf> {
        std::fs::read_dir(self.policy.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.file_name().unwrap().to_string_lossy().contains("backup-"))
            .collect()
    }

    fn unchanged(&self, expected: &str) {
        assert_eq!(
            std::fs::read_to_string(self.home.join("workspace/ocap/toolchains.toml")).unwrap(),
            "# never read or signed through --config\n"
        );
        assert_eq!(std::fs::read_to_string(&self.policy).unwrap(), expected);
        assert!(
            self.backups().is_empty(),
            "refusal must not create a backup"
        );
    }
}

async fn prompted(f: &Fixture, action: &str) -> (bool, String) {
    let pty = tests_pty::Pty::open_with_cursor_reply(1, 1);
    let mut cmd = f.command();
    cmd.stdin(pty.slave_stdio())
        .stdout(pty.slave_stdio())
        .stderr(pty.slave_stdio());
    let child = cmd.spawn().unwrap();
    drop(cmd);
    assert!(
        pty.wait_for_screen("Approve this toolchain policy?", Duration::from_secs(45)),
        "{}",
        pty.screen()
    );
    f.unchanged(&f.original);
    let shown = pty.screen();
    for root in &f.body.roots {
        assert!(shown.contains(&format!("{:?}", root.path)), "{shown}");
        assert!(
            shown.contains(&format!("{:?}", Path::new(&root.path).join("bin"))),
            "{shown}"
        );
    }
    for identity in [
        f.body.content_id().unwrap().to_string(),
        f.body.roots[0].expected_tree.unwrap().to_string(),
        f.key.fingerprint().to_string(),
    ] {
        assert!(shown.contains(&identity), "{shown}");
    }
    assert!(shown.contains("UNPINNED"), "{shown}");
    let answer = match action {
        "decline" => "n\n",
        "blank" => "\n",
        "policy-changed" => {
            std::fs::write(&f.policy, format!("{}\n", f.original)).unwrap();
            "y\n"
        }
        "tree-changed" => {
            let file = Path::new(&f.body.roots[0].path).join("new-member");
            std::fs::write(&file, "changed after review").unwrap();
            private(&file, 0o600);
            "y\n"
        }
        "key-changed" => {
            let replacement = f.config.join("replacement-identity.pem");
            newt_identity::UserKey::generate()
                .save(&replacement)
                .unwrap();
            std::fs::rename(replacement, f.config.join("identity.pem")).unwrap();
            "y\n"
        }
        "approve" => "y\n",
        _ => panic!("unknown fixture action"),
    };
    pty.type_in(answer);
    let output = tokio::time::timeout(Duration::from_secs(45), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    (output.status.success(), pty.screen_to_eof())
}

pub async fn run() {
    let f = Fixture::new();
    let (ok, text) = prompted(&f, "approve").await;
    assert!(ok, "{text}");
    let installed = std::fs::read_to_string(&f.policy).unwrap();
    let verified = newt_core::toolchains::verify(
        &installed,
        &Ed25519ApproveVerifier {
            verifying_key: f.key.public().as_bytes(),
        },
    )
    .unwrap();
    assert_eq!(verified.content_id().unwrap(), f.body.content_id().unwrap());
    let backups = f.backups();
    assert_eq!(backups.len(), 1);
    assert_eq!(std::fs::read_to_string(&backups[0]).unwrap(), f.original);
    for path in [&f.policy, &backups[0]] {
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert!(text.contains("Restart Newt sessions"), "{text}");
    // Ground the inverse: the backup restores even comments and a missing sig.
    std::fs::write(&f.policy, std::fs::read(&backups[0]).unwrap()).unwrap();
    assert_eq!(std::fs::read_to_string(&f.policy).unwrap(), f.original);
    println!("SIGN_APPROVED: ordered roots/pins/key shown; production verifier accepts; private backup restores exact bytes");

    for (action, reason) in [
        ("decline", "not approved"),
        ("blank", "not approved"),
        ("policy-changed", "changed during confirmation"),
        ("tree-changed", "content identity mismatch"),
        ("key-changed", "key changed during confirmation"),
    ] {
        let f = Fixture::new();
        let (ok, text) = prompted(&f, action).await;
        assert!(!ok && text.contains(reason), "{action}: {text}");
        let expected = if action == "policy-changed" {
            format!("{}\n", f.original)
        } else {
            f.original.clone()
        };
        f.unchanged(&expected);
        println!("SIGN_REFUSED: {action}; no policy replacement or backup");
    }
    for action in ["piped-yes", "missing-key", "unsafe-root"] {
        let f = Fixture::new();
        match action {
            "missing-key" => std::fs::remove_file(f.config.join("identity.pem")).unwrap(),
            "unsafe-root" => private(Path::new(&f.body.roots[0].path), 0o770),
            _ => (),
        }
        let mut child = f
            .command()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"y\n").await.unwrap();
        let output = tokio::time::timeout(Duration::from_secs(45), child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output.status.success(), "{action}: {text}");
        if action == "piped-yes" {
            assert!(text.contains("requires an operator terminal"), "{text}");
        }
        if action == "missing-key" {
            assert!(
                !f.config.join("identity.pem").exists(),
                "must never generate an identity"
            );
        }
        f.unchanged(&f.original);
        println!("SIGN_REFUSED: {action}; no policy replacement or key generation");
    }
    println!("TOOLCHAIN_SIGNING_ACCEPTANCE_OK");
}

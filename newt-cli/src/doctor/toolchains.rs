//! Explicit operator signing; no startup repair or model-tool entrypoint.
use anyhow::{ensure, Context, Result};
use content_addressable::{ContentAddressable, RawContentId};
use newt_core::atomic_fs::{acquire_lock, ResolvedPath};
use newt_core::ocap_policy::Ed25519ApproveVerifier;
use newt_core::toolchains::{self, Body};
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};

pub(crate) fn run() -> Result<()> {
    let config =
        newt_core::Config::user_config_dir().context("cannot resolve operator config directory")?;
    let source = config.join("ocap/toolchains.toml");
    let original =
        std::fs::read_to_string(&source).context("cannot read operator toolchain policy")?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let body = toolchains::review_for_signing(&config, home.clone(), &original)?;
    let key_path = config.join("identity.pem");
    let key = newt_identity::load_user_key(&key_path)
        .context("existing operator identity required; no key created")?;
    let review = review_text(&source, &body, &key.fingerprint().to_string())?;
    ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "--sign-toolchains requires an operator terminal and explicit approval; nothing signed"
    );
    let form = newt_core::interaction_form::confirm(
        review,
        "A private backup preserves the original file, including comments. Blank declines.",
        "sign this policy",
        "leave unchanged",
    );
    let window = newt_core::tty::Terminal::suspend_for_prompt(
        newt_core::tty::TerminalTaker::PlainCliConfirm,
    );
    let approved = newt_core::interaction_terminal::confirmed_on_terminal(&window, &form, false);
    drop(window);
    ensure!(approved, "toolchain policy not approved; nothing signed");

    let destination = ResolvedPath::resolve_entry(&source)?;
    let _lock = acquire_lock(&destination.lock_path())?;
    let current = std::fs::read_to_string(destination.as_path())?;
    ensure_unchanged(&original, &current)?;
    // Repeat trust and pin checks after approval, before any signing or backup.
    let body = toolchains::review_for_signing(&config, home, &current)?;
    ensure!(
        newt_identity::load_user_key(&key_path)?.public() == key.public(),
        "operator key changed during confirmation; nothing signed"
    );
    let signed = toolchains::signed_toml(&body, &|payload| key.sign(payload).to_bytes())?;
    let verifier = Ed25519ApproveVerifier {
        verifying_key: key.public().as_bytes(),
    };
    let policy = body.content_id()?;
    ensure!(
        toolchains::verify(&signed, &verifier)?.content_id()? == policy,
        "signed policy identity changed"
    );
    let backup = super::durable_grants::save_backup(
        destination.as_path(),
        &current,
        newt_core::atomic_fs::sync_parent,
    )
    .context("backup could not be durably published; policy unchanged")?;
    destination.atomic_write_private(signed.as_bytes())?;
    let installed = std::fs::read_to_string(destination.as_path())?;
    ensure!(
        toolchains::verify(&installed, &verifier)?.content_id()? == policy,
        "installed policy identity changed"
    );
    println!("Signed toolchain policy {policy}; backup: {backup:?}");
    println!("Restart Newt sessions to capture this policy. Each build rechecks trust, writes, and pins.");
    Ok(())
}

fn ensure_unchanged(expected: &str, current: &str) -> Result<()> {
    ensure!(
        RawContentId::from_content(expected.as_bytes())
            == RawContentId::from_content(current.as_bytes()),
        "toolchain policy changed during confirmation; nothing signed, rerun --sign-toolchains"
    );
    Ok(())
}

fn review_text(source: &Path, body: &Body, fingerprint: &str) -> Result<String> {
    let mut text = format!(
        "Operator toolchain policy: {source:?}\nPolicy identity: {}\nOperator key: {fingerprint}\nBuild-only read + executable runtime; no new writes.\nRoots in PATH order (quoted, escaped paths):\n",
        body.content_id()?,
    );
    for (index, root) in body.roots.iter().enumerate() {
        text.push_str(&format!(
            "{}. root: {:?}\n   PATH: {:?}\n   identity: {}\n",
            index + 1,
            root.path,
            Path::new(&root.path).join("bin"),
            root.expected_tree.map_or_else(
                || "UNPINNED (no content identity approved)".into(),
                |id| format!("{id} (verified)")
            ),
        ));
    }
    if body.roots.is_empty() {
        text.push_str("No additional roots.\n");
    }
    text.push_str("Approve this toolchain policy?");
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use newt_core::toolchains::Root;

    #[test]
    fn toolchain_signing_review_shows_exact_order_scope_and_identity_safely() {
        let pin = content_addressable::ContentId::from_canonical_bytes(&[0x80]);
        let body = Body {
            schema: "newt.build-toolchains/v1".into(),
            roots: vec![
                Root {
                    path: "/curated/with\ncontrol\u{1b}\u{202e}".into(),
                    expected_tree: Some(pin),
                },
                Root {
                    path: "/curated/second".into(),
                    expected_tree: None,
                },
            ],
        };
        let shown = review_text(Path::new("policy.toml"), &body, "disposable-key").unwrap();
        assert!(shown.contains(&body.content_id().unwrap().to_string()));
        assert!(shown.contains(&pin.to_string()) && shown.contains("UNPINNED"));
        let first = format!("{:?}", body.roots[0].path);
        let second = format!("{:?}", body.roots[1].path);
        assert!(shown.find(&first).unwrap() < shown.find(&second).unwrap());
        assert!(shown.contains("disposable-key") && shown.contains("no new writes"));
        assert!(!shown.contains(['\u{1b}', '\u{202e}']) && !shown.contains("with\ncontrol"));
    }

    #[test]
    fn toolchain_signing_refuses_even_a_comment_change_after_review() {
        assert!(ensure_unchanged("reviewed\n", "reviewed\n").is_ok());
        assert!(ensure_unchanged(
            "reviewed\n",
            "reviewed\n# same body, different reviewed bytes\n"
        )
        .is_err());
    }
}

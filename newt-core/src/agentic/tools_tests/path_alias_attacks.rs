//! PR #2828: a stored path must never become authority for a symlink target.
use super::*;
use crate::{Caveats, Scope};

async fn write(path: &std::path::Path, workspace: &str, grants: &Caveats) -> String {
    crate::execute_tool(
        "write_file",
        &serde_json::json!({"path":path,"content":"corrupted"}),
        workspace,
        false,
        100,
        grants,
        &mut crate::NoMcp,
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

/// Previously the stored root was canonicalized again after replacement and
/// authorized a direct outside request, including a root missing at approval.
#[tokio::test]
async fn stored_root_replacement_cannot_authorize_outside_dispatch() {
    for missing in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let root = base.join("allowed");
        if !missing {
            std::fs::create_dir(&root).unwrap();
        }
        let outside = base.join("outside");
        std::fs::create_dir(&outside).unwrap();
        let sentinel = outside.join("sentinel");
        std::fs::write(&sentinel, "keep").unwrap();
        let authority = crate::widen_caveats(
            &Caveats {
                fs_write: Scope::none(),
                ..Caveats::top()
            },
            &[(DenialKind::FsWrite, root.to_str().unwrap().into())],
        );
        if !missing {
            std::fs::rename(&root, base.join("held")).unwrap();
        }
        std::os::unix::fs::symlink(&outside, &root).unwrap();
        let out = write(&sentinel, base.to_str().unwrap(), &authority).await;
        assert!(out.contains("capability denied"), "{out}");
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "keep");
    }
}

/// A signed legacy spelling is not permission for whatever it points at now.
#[test]
fn durable_stored_root_does_not_rebind_to_an_outside_request() {
    use crate::ocap_store::{CapabilityClass, Ed25519ApproveVerifier, PolicyFile, Verdict};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("missing");
    let outside = temp.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    let key = agent_mesh_protocol::UserKey::generate();
    let mut file = PolicyFile::parse(&format!(
        "[[fs]]\npath = {:?}\nwrite = true\n",
        root.to_str().unwrap()
    ))
    .unwrap();
    assert_eq!(
        crate::ocap_store::sign_approves(
            &mut file,
            |_, _| false,
            |payload| key.sign(payload).to_bytes()
        ),
        (1, vec![])
    );
    let (store, _) =
        crate::ocap_store::build_store(&[(Verdict::Approve, Some(file.to_toml().unwrap()))]);
    let (store, warnings) = crate::ocap_store::verify_approves(
        store,
        Some(&Ed25519ApproveVerifier {
            verifying_key: key.public().as_bytes(),
        }),
    );
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(
        store.evaluate(CapabilityClass::Fs, root.to_str().unwrap()),
        Some(Verdict::Approve)
    );
    std::os::unix::fs::symlink(&outside, &root).unwrap();
    assert_eq!(
        crate::ocap_store::evaluate_request(&store, DenialKind::FsWrite, outside.to_str().unwrap()),
        None
    );
}

/// macOS must reject a missing/renamed root and a replaced ancestor even for
/// requests through the originally approved name, including after admission.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn mac_stored_root_and_ancestor_swaps_fail_before_open() {
    for (missing, ancestor) in [(true, false), (false, false), (false, true)] {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let parent = base.join("parent");
        std::fs::create_dir(&parent).unwrap();
        let root = parent.join("root");
        if !missing {
            std::fs::create_dir(&root).unwrap();
        }
        let outside = base.join("outside");
        let target = if ancestor {
            outside.join("root")
        } else {
            outside.clone()
        };
        std::fs::create_dir_all(&target).unwrap();
        let sentinel = target.join("sentinel");
        std::fs::write(&sentinel, "keep").unwrap();
        let authority = crate::widen_caveats(
            &Caveats {
                fs_write: Scope::none(),
                ..Caveats::top()
            },
            &[(DenialKind::FsWrite, root.to_str().unwrap().into())],
        );
        let request = root.join("sentinel");
        let Some(Some((granted, relative))) =
            object_bound_target(&authority.fs_write, request.to_str().unwrap())
        else {
            panic!("pre-swap admission")
        };
        let swap = if ancestor { &parent } else { &root };
        if !missing {
            std::fs::rename(swap, base.join("held")).unwrap();
        }
        std::os::unix::fs::symlink(&outside, swap).unwrap();
        let out = write(&request, base.to_str().unwrap(), &authority).await;
        assert!(out.contains("capability denied"), "{out}");
        // Reuse the admitted binding after the swap: no second gate can hide
        // an unsafe acquisition. This is the actual file-dispatch open seam.
        assert!(crate::fs_cap::WorkspaceDir::create_granted_file(
            std::path::Path::new(granted),
            &relative
        )
        .is_err());
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "keep");
    }
}

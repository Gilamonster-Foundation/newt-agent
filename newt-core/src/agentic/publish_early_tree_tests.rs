use super::*;

fn git(root: &Path, args: &[&str]) {
    let out = crate::git_hardening::metadata_git(root, args, &crate::Scope::All)
        .unwrap()
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// #2831 P1: a repo writer changes conversion config AFTER screening. Ground
/// the advisory's no-exec guarantee in an actual Git filter and outside sentinel.
#[tokio::test]
async fn publish_early_filter_race_never_executes() {
    use crate::agentic::tools::disable_ocap_tests::{env_lock, EnvVar};
    let _lock = env_lock().await;
    let empty = tempfile::NamedTempFile::new().unwrap();
    let _global = EnvVar::set("GIT_CONFIG_GLOBAL", empty.path().to_str().unwrap());
    let _system = EnvVar::set("GIT_CONFIG_NOSYSTEM", "1");
    let _count = EnvVar::set("GIT_CONFIG_COUNT", "0");
    let _parameters = EnvVar::set("GIT_CONFIG_PARAMETERS", "");
    for (kind, storage) in [
        ("clean", "local"),
        ("process", "local"),
        ("clean", "worktree"),
        ("process", "worktree"),
        ("clean", "include"),
        ("process", "include"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "extensions.worktreeConfig", "true"]);
        std::fs::write(root.join("source"), "checked\n").unwrap();
        // The injected attribute is ignored, so absence of a sentinel cannot
        // be explained by the ordinary untracked-file rejection.
        std::fs::write(root.join(".gitignore"), ".gitattributes\n").unwrap();
        // Force Git's racy-index content check without sleeps or timing luck.
        let future = std::time::UNIX_EPOCH + std::time::Duration::from_secs(2_000_000_000);
        std::fs::File::options()
            .write(true)
            .open(root.join("source"))
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(future))
            .unwrap();
        git(&root, &["add", "."]);
        assert!(index_tree(&root, &crate::Scope::All).is_some());
        let witness = index_tree_with(&root, &crate::Scope::All, || {
            let key = format!("filter.race.{kind}");
            let script = "echo ran > ../sentinel; exit 1";
            match storage {
                "worktree" => git(&root, &["config", "--worktree", &key, script]),
                "include" => {
                    git(
                        &root,
                        &["config", "--file", ".git/race-config", &key, script],
                    );
                    git(&root, &["config", "include.path", "race-config"]);
                }
                _ => git(&root, &["config", &key, script]),
            }
            std::fs::write(root.join(".gitattributes"), "source filter=race\n").unwrap();
        });
        assert!(
            !temp.path().join("sentinel").exists(),
            "{storage} {kind} filter ran on the host"
        );
        assert!(witness.is_none());
    }
}

/// #2831: pure readers agree with native Git for both object hashes and index
/// formats, including packed HEAD objects. Conversion settings always decline.
#[tokio::test]
async fn publish_early_raw_content_and_conversion_controls() {
    use crate::agentic::tools::disable_ocap_tests::{env_lock, EnvVar};
    let _lock = env_lock().await;
    let empty = tempfile::NamedTempFile::new().unwrap();
    let _global = EnvVar::set("GIT_CONFIG_GLOBAL", empty.path().to_str().unwrap());
    let _system = EnvVar::set("GIT_CONFIG_NOSYSTEM", "1");
    let _count = EnvVar::set("GIT_CONFIG_COUNT", "0");
    let _parameters = EnvVar::set("GIT_CONFIG_PARAMETERS", "");
    for format in ["sha1", "sha256"] {
        for version in ["2", "4"] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            git(
                root,
                &[
                    "init",
                    "-q",
                    "-b",
                    "main",
                    &format!("--object-format={format}"),
                ],
            );
            std::fs::create_dir(root.join("nested")).unwrap();
            std::fs::write(root.join("nested/source name"), [0, 255, 10, 65]).unwrap();
            git(root, &["add", "."]);
            git(
                root,
                &["update-index", &format!("--index-version={version}")],
            );
            let checked = index_tree(root, &crate::Scope::All).expect("raw-content witness");
            git(
                root,
                &["-c", "commit.gpgsign=false", "commit", "-qm", "checked"],
            );
            assert_eq!(index_tree(root, &crate::Scope::All), Some(checked));
            assert!(index_is_head(root, &crate::Scope::All));
            git(root, &["repack", "-ad"]);
            git(root, &["prune-packed"]);
            assert!(
                index_is_head(root, &crate::Scope::All),
                "packed {format} HEAD"
            );
            for (key, value) in [
                ("core.autocrlf", "true"),
                ("core.autocrlf", "input"),
                ("core.eol", "lf"),
                ("core.attributesFile", "missing"),
            ] {
                git(root, &["config", key, value]);
                assert!(
                    index_tree(root, &crate::Scope::All).is_none(),
                    "{key}={value}"
                );
                git(root, &["config", "--unset", key]);
            }
            for path in [
                ".gitattributes",
                "nested/.gitattributes",
                ".git/info/attributes",
            ] {
                std::fs::write(
                    root.join(path),
                    "* text eol=lf ident working-tree-encoding=UTF-8\n",
                )
                .unwrap();
                assert!(index_tree(root, &crate::Scope::All).is_none(), "{path}");
                std::fs::remove_file(root.join(path)).unwrap();
            }
            assert!(index_tree(root, &crate::Scope::none()).is_none());
            std::fs::write(root.join("nested/source name"), [0, 254, 10, 65]).unwrap();
            assert!(index_tree(root, &crate::Scope::All).is_none());
        }
    }
}

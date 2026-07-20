//! Tests for `newt-skills`, in two tiers.
//!
//! * [`mocked`] — the **fully-mocked unit tier** (per CLAUDE.md "Testing
//!   strategy"): pure functions and discovery exercised through the
//!   [`crate::SkillFs`] seam with an in-memory [`crate::fs::MemFs`]. No disk, no
//!   `tempfile`, deterministic and parallel-safe. This tier gates every change.
//! * [`grounding`] — the **real-filesystem add-on tier**: a small set of tests
//!   against `tempfile` that verify the mocked tier is testing something real
//!   (symlink recreation, permission errors, symlink loops on an actual FS).
//!   Each test's doc comment names the mocked behavior it grounds.

use super::*;

fn skill_md(name: &str, desc: &str) -> String {
    format!("---\nname: {name}\ndescription: {desc}\n---\nBody of {name}.\n")
}

// ---------------------------------------------------------------------------
// Fully-mocked unit tier
// ---------------------------------------------------------------------------
mod mocked {
    use super::{skill_md, *};
    use crate::fs::MemFs;

    // --- frontmatter parsing (pure) ---------------------------------------

    const VALID: &str = "---\nname: commit-style\ndescription: How this project writes commits.\nwhen_to_use: Before any git commit.\nversion: 1.0.0\nlicense: Apache-2.0\n---\nUse the imperative mood. Wrap at 72 columns.\n";

    #[test]
    fn parses_valid_skill_into_fields() {
        let s = Skill::parse(VALID, "/tmp/commit-style").unwrap();
        assert_eq!(s.name, "commit-style");
        assert_eq!(s.description, "How this project writes commits.");
        assert_eq!(s.when_to_use.as_deref(), Some("Before any git commit."));
        assert_eq!(s.version.as_deref(), Some("1.0.0"));
        assert_eq!(s.license.as_deref(), Some("Apache-2.0"));
        assert!(s.body.starts_with("Use the imperative mood."));
        assert!(s.caveats.is_none());
    }

    #[test]
    fn parses_optional_caveats_block() {
        let text = "---\nname: deployer\ndescription: Deploys.\ncaveats:\n  exec: { only: [\"git\", \"cargo\"] }\n  fs_read: all\n  max_calls: { at_most: 5 }\n---\nbody\n";
        let s = Skill::parse(text, "").unwrap();
        let cav = s.caveats.expect("caveats parsed");
        match cav.exec.unwrap() {
            SkillScope::Only(set) => assert!(set.contains("git") && set.contains("cargo")),
            SkillScope::All => panic!("expected Only"),
        }
        assert_eq!(cav.fs_read, Some(SkillScope::All));
        assert_eq!(cav.max_calls, Some(SkillCountBound::AtMost(5)));
    }

    #[test]
    fn triggers_alias_maps_to_when_to_use() {
        let text = "---\nname: x\ndescription: d\ntriggers: when stuck\n---\nbody\n";
        let s = Skill::parse(text, "").unwrap();
        assert_eq!(s.when_to_use.as_deref(), Some("when stuck"));
    }

    #[test]
    fn tolerates_missing_trailing_newline_after_fence() {
        let s = Skill::parse("---\nname: x\ndescription: d\n---", "").unwrap();
        assert_eq!(s.name, "x");
        assert_eq!(s.body, "");
    }

    #[test]
    fn missing_opening_fence_is_frontmatter_error() {
        let err = Skill::parse("no frontmatter here\n", "").unwrap_err();
        assert!(matches!(err, SkillError::Frontmatter(_)), "got: {err:?}");
        assert!(err.to_string().contains("must start with a `---`"));
    }

    #[test]
    fn unterminated_frontmatter_is_frontmatter_error() {
        let err = Skill::parse("---\nname: x\ndescription: d\n", "").unwrap_err();
        assert!(matches!(err, SkillError::Frontmatter(_)), "got: {err:?}");
        assert!(err.to_string().contains("not terminated"));
    }

    #[test]
    fn empty_document_is_frontmatter_error() {
        assert!(matches!(
            Skill::parse("", "").unwrap_err(),
            SkillError::Frontmatter(_)
        ));
    }

    #[test]
    fn malformed_yaml_is_yaml_error() {
        // Missing the required `description` field.
        let err = Skill::parse("---\nname: x\n---\nbody\n", "").unwrap_err();
        assert!(matches!(err, SkillError::Yaml(_)), "got: {err:?}");
    }

    #[test]
    fn duplicate_yaml_keys_is_yaml_error() {
        // A duplicated frontmatter key is silently last-wins in some parsers;
        // serde_yaml rejects it, and we surface that as a typed Yaml error.
        let text = "---\nname: a\ndescription: first\ndescription: second\n---\nbody\n";
        assert!(matches!(
            Skill::parse(text, "").unwrap_err(),
            SkillError::Yaml(_)
        ));
    }

    // --- skill-name validation (the capability boundary) ------------------

    #[test]
    fn validate_accepts_ordinary_names() {
        for ok in ["commit-style", "three-cs", "a", "a_b.c", "dgx-spark-admin"] {
            assert!(validate_skill_name(ok).is_ok(), "{ok} should be valid");
        }
    }

    #[test]
    fn validate_rejects_unsafe_names_by_reason() {
        use crate::NameRejection as R;
        let cases = [
            ("", R::Empty),
            ("../etc", R::PathSeparator),
            ("a/b", R::PathSeparator),
            ("a\\b", R::PathSeparator),
            ("..", R::Traversal),
            (".", R::Traversal),
            (".hidden", R::Hidden),
            ("has space", R::DisallowedCharacter),
            ("bell\u{7}", R::DisallowedCharacter),
            ("colon:name", R::DisallowedCharacter),
        ];
        for (name, want) in cases {
            match validate_skill_name(name) {
                Err(SkillError::InvalidName { reason, .. }) => {
                    assert_eq!(reason, want, "name {name:?}");
                }
                other => panic!("name {name:?} expected {want:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn validate_rejects_overlong_name() {
        let long = "a".repeat(MAX_NAME_LEN + 1);
        assert!(matches!(
            validate_skill_name(&long).unwrap_err(),
            SkillError::InvalidName {
                reason: crate::NameRejection::TooLong,
                ..
            }
        ));
    }

    // --- discovery (mocked FS) --------------------------------------------

    #[test]
    fn discover_finds_sorts_and_lists_bundled_files() {
        let root = Path::new("/skills");
        let fs = MemFs::new()
            .skill(root, "beta", skill_md("beta", "Second"))
            .skill(root, "alpha", skill_md("alpha", "First"))
            .file(root.join("beta").join("deploy.sh"), "echo hi\n");

        let skills = discover_in(&fs, root);
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["alpha", "beta"]);
        assert!(skills[0].files.is_empty());
        assert_eq!(skills[1].files.len(), 1);
        assert!(skills[1].files[0].ends_with("deploy.sh"));
    }

    #[test]
    fn discover_missing_dir_is_empty_not_error() {
        assert!(discover_in(&MemFs::new(), Path::new("/nope")).is_empty());
    }

    #[test]
    fn discover_skips_broken_skill_keeps_good_one() {
        let root = Path::new("/skills");
        let fs = MemFs::new()
            .skill(root, "good", skill_md("good", "Good one"))
            .skill(root, "bad", "not valid frontmatter");
        let skills = discover_in(&fs, root);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "good");
    }

    #[test]
    fn discover_skips_hidden_directories() {
        // Regression: a dot-directory (e.g. `.git`, macOS junk) must not surface
        // as a skill even if it happens to contain a valid SKILL.md.
        let root = Path::new("/skills");
        let fs = MemFs::new()
            .skill(root, ".hidden", skill_md("hidden", "Sneaky"))
            .skill(root, "good", skill_md("good", "Real"));
        let skills = discover_in(&fs, root);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "good");
    }

    #[test]
    fn discover_skips_hidden_bundled_files() {
        // Regression: `.DS_Store` / editor swap files must not be listed as
        // bundled files.
        let root = Path::new("/skills");
        let fs = MemFs::new()
            .skill(root, "a", skill_md("a", "A"))
            .file(root.join("a").join(".DS_Store"), "junk");
        let skills = discover_in(&fs, root);
        assert_eq!(skills.len(), 1);
        assert!(
            skills[0].files.is_empty(),
            "hidden file leaked: {:?}",
            skills[0].files
        );
    }

    #[test]
    fn discover_skips_dir_without_manifest() {
        let root = Path::new("/skills");
        let fs = MemFs::new()
            .dir(root.to_path_buf())
            .dir(root.join("emptydir"))
            .skill(root, "good", skill_md("good", "Real"));
        let skills = discover_in(&fs, root);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "good");
    }

    #[test]
    fn discover_skips_unsafe_frontmatter_name() {
        // Regression (path traversal): a skill declaring `name: ../../etc/passwd`
        // must never enter the index — a name that cannot be safely *loaded*
        // must not be *shown* either.
        let root = Path::new("/skills");
        let fs = MemFs::new().skill(root, "innocent", skill_md("../../etc/passwd", "evil"));
        assert!(discover_in(&fs, root).is_empty());
    }

    #[test]
    fn discover_skips_unreadable_subdir() {
        // An unreadable skill dir (permission denied / symlink loop) is skipped,
        // not fatal. Grounded by `grounding::discover_skips_real_unreadable_dir`.
        let root = Path::new("/skills");
        let fs = MemFs::new()
            .dir(root.to_path_buf())
            .unreadable_dir(root.join("locked"))
            .skill(root, "good", skill_md("good", "Real"));
        let skills = discover_in(&fs, root);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "good");
    }

    // --- search-path discovery + shadows ----------------------------------

    #[test]
    fn discover_paths_unions_dirs_and_sorts() {
        let a = PathBuf::from("/a");
        let b = PathBuf::from("/b");
        let fs = MemFs::new()
            .skill(&a, "alpha", skill_md("alpha", "A"))
            .skill(&b, "beta", skill_md("beta", "B"));
        let (winners, shadowed) = discover_paths_in(&fs, &[a, b]);
        let names: Vec<&str> = winners.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["alpha", "beta"]);
        assert!(shadowed.is_empty());
    }

    #[test]
    fn discover_paths_first_dir_wins_and_reports_shadows() {
        let first = PathBuf::from("/first");
        let second = PathBuf::from("/second");
        let fs = MemFs::new()
            .skill(&first, "dup", skill_md("dup", "from-first"))
            .skill(&second, "dup", skill_md("dup", "from-second"))
            .skill(&second, "unique", skill_md("unique", "only-second"));

        let (winners, shadowed) = discover_paths_in(&fs, &[first.clone(), second.clone()]);
        let names: Vec<&str> = winners.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["dup", "unique"]);
        // The winning `dup` came from `first`.
        let dup = winners.iter().find(|s| s.name == "dup").unwrap();
        assert_eq!(dup.dir, first.join("dup"));
        // The `second` copy is reported as shadowed, not dropped.
        assert_eq!(shadowed.len(), 1);
        assert_eq!(shadowed[0].name, "dup");
        assert_eq!(shadowed[0].dir, second.join("dup"));
    }

    #[test]
    fn discover_paths_skips_missing_dirs() {
        let real = PathBuf::from("/real");
        let missing = PathBuf::from("/missing");
        let fs = MemFs::new().skill(&real, "alpha", skill_md("alpha", "A"));
        assert_eq!(discover_paths_in(&fs, &[missing, real]).0.len(), 1);
    }

    // --- body loading ------------------------------------------------------

    #[test]
    fn load_body_returns_body_for_known_skill() {
        let root = Path::new("/s");
        let fs = MemFs::new().skill(root, "alpha", skill_md("alpha", "A"));
        assert!(load_body_in(&fs, root, "alpha")
            .unwrap()
            .contains("Body of alpha."));
    }

    #[test]
    fn load_body_lists_bundled_files() {
        let root = Path::new("/s");
        let fs = MemFs::new()
            .skill(root, "beta", skill_md("beta", "B"))
            .file(root.join("beta").join("deploy.sh"), "echo hi\n");
        let body = load_body_in(&fs, root, "beta").unwrap();
        assert!(body.contains("Bundled files"));
        assert!(body.contains("deploy.sh"));
    }

    #[test]
    fn load_body_unknown_skill_errors() {
        let root = Path::new("/s");
        let fs = MemFs::new().skill(root, "alpha", skill_md("alpha", "A"));
        assert!(matches!(
            load_body_in(&fs, root, "nope").unwrap_err(),
            SkillError::UnknownSkill(_)
        ));
    }

    #[test]
    fn load_body_rejects_traversal_name_before_fs() {
        // Regression (path traversal): a `../`-style request is rejected by name
        // validation, before any filesystem access.
        let root = Path::new("/s");
        let fs = MemFs::new().skill(root, "alpha", skill_md("alpha", "A"));
        assert!(matches!(
            load_body_in(&fs, root, "../secret").unwrap_err(),
            SkillError::InvalidName { .. }
        ));
    }

    #[test]
    fn load_body_from_honours_first_dir_wins() {
        let first = PathBuf::from("/first");
        let second = PathBuf::from("/second");
        let fs = MemFs::new()
            .skill(
                &first,
                "dup",
                "---\nname: dup\ndescription: d.\n---\nFIRST BODY.\n",
            )
            .skill(
                &second,
                "dup",
                "---\nname: dup\ndescription: d.\n---\nSECOND BODY.\n",
            );
        let body = load_body_from_in(&fs, &[first, second], "dup").unwrap();
        assert!(body.contains("FIRST BODY."));
        assert!(!body.contains("SECOND BODY."));
    }

    #[test]
    fn load_body_from_errors_for_unknown_skill() {
        let dir = PathBuf::from("/d");
        let fs = MemFs::new().skill(&dir, "alpha", skill_md("alpha", "A"));
        assert!(matches!(
            load_body_from_in(&fs, &[dir], "missing").unwrap_err(),
            SkillError::UnknownSkill(_)
        ));
    }

    #[test]
    fn load_body_from_rejects_traversal_name() {
        let dir = PathBuf::from("/d");
        let fs = MemFs::new().skill(&dir, "alpha", skill_md("alpha", "A"));
        assert!(matches!(
            load_body_from_in(&fs, &[dir], "../../etc/passwd").unwrap_err(),
            SkillError::InvalidName { .. }
        ));
    }

    #[test]
    fn load_body_from_resolves_by_frontmatter_name_not_folder() {
        // Regression: the loader resolves by declared `name`, not folder name,
        // so what the agent was shown in the index is exactly what loads even
        // when the folder is named differently.
        let root = PathBuf::from("/s");
        let fs = MemFs::new().skill(&root, "folder-name", skill_md("real-name", "R"));
        assert!(load_body_from_in(&fs, std::slice::from_ref(&root), "real-name")
            .unwrap()
            .contains("Body of real-name."));
        // Asking for the folder name (a valid component, but not a declared
        // skill name) is a clean miss, not a match.
        assert!(matches!(
            load_body_from_in(&fs, &[root], "folder-name").unwrap_err(),
            SkillError::UnknownSkill(_)
        ));
    }

    // --- index block -------------------------------------------------------

    #[test]
    fn index_block_lists_names_and_descriptions_only() {
        let root = Path::new("/s");
        let fs = MemFs::new().skill(root, "alpha", skill_md("alpha", "First skill"));
        let block = index_block(&discover_in(&fs, root)).unwrap();
        assert!(block.contains("Available skills (call `use_skill` to load one):"));
        assert!(block.contains("alpha: First skill"));
        // The body must NOT leak into the index (progressive disclosure).
        assert!(!block.contains("Body of alpha."));
    }

    #[test]
    fn index_block_empty_when_no_skills() {
        assert!(index_block(&[]).is_none());
    }
}

// ---------------------------------------------------------------------------
// Real-filesystem grounding tier (add-on; see module docs / issue #514)
// ---------------------------------------------------------------------------
mod grounding {
    use super::{skill_md, *};
    use std::fs;
    use tempfile::tempdir;

    /// Write a minimal valid skill folder `<root>/<name>/` with a bundled file.
    fn make_skill(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), skill_md(name, "test skill")).unwrap();
        fs::write(dir.join("helper.sh"), "echo hi\n").unwrap();
        dir
    }

    /// Grounds `mocked::discover_finds_sorts_and_lists_bundled_files`: the MemFs
    /// listing/`is_file` behavior matches a real directory walk.
    #[test]
    fn discover_over_real_fs_matches_mock_shape() {
        let tmp = tempdir().unwrap();
        make_skill(tmp.path(), "alpha");
        let skills = discover(tmp.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "alpha");
        assert!(skills[0].files.iter().any(|f| f.ends_with("helper.sh")));
    }

    /// Grounds `mocked::discover_skips_unreadable_subdir`: a real `0o000`
    /// directory really does fail to read and is skipped, not fatal.
    #[cfg(unix)]
    #[test]
    fn discover_skips_real_unreadable_dir() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempdir().unwrap();
        make_skill(tmp.path(), "good");
        let locked = tmp.path().join("locked");
        fs::create_dir_all(&locked).unwrap();
        fs::write(locked.join("SKILL.md"), skill_md("locked", "x")).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let skills = discover(tmp.path());
        // Restore before assertions so the tempdir can be cleaned up.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["good"]
        );
    }

    /// Grounds discovery's "unreadable dir → skip" against a real symlink loop:
    /// `std::fs::read_dir` errors on the loop, and discovery treats that as
    /// "no skill here".
    #[cfg(unix)]
    #[test]
    fn discover_skips_real_symlink_loop() {
        let tmp = tempdir().unwrap();
        make_skill(tmp.path(), "good");
        std::os::unix::fs::symlink(tmp.path().join("loop"), tmp.path().join("loop")).unwrap();
        let skills = discover(tmp.path());
        assert_eq!(
            skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["good"]
        );
    }

    // --- install_skill -----------------------------------------------------

    #[test]
    fn install_copy_duplicates_folder_and_bundled_files() {
        let tmp = tempdir().unwrap();
        let src = make_skill(tmp.path(), "commit-style");
        let dest_root = tmp.path().join("dest");
        let dest = install_skill(&src, &dest_root, None, InstallMode::Copy, false).unwrap();

        assert_eq!(dest, dest_root.join("commit-style"));
        assert!(dest.join("SKILL.md").is_file());
        assert!(dest.join("helper.sh").is_file());
        assert!(!fs::symlink_metadata(&dest)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(discover(&dest_root).len(), 1);
    }

    #[test]
    fn install_honours_name_override() {
        let tmp = tempdir().unwrap();
        let src = make_skill(tmp.path(), "commit-style");
        let dest_root = tmp.path().join("dest");
        let dest =
            install_skill(&src, &dest_root, Some("renamed"), InstallMode::Copy, false).unwrap();
        assert_eq!(dest, dest_root.join("renamed"));
        assert!(dest.join("SKILL.md").is_file());
    }

    #[test]
    fn install_rejects_existing_dest_without_force() {
        let tmp = tempdir().unwrap();
        let src = make_skill(tmp.path(), "commit-style");
        let dest_root = tmp.path().join("dest");
        install_skill(&src, &dest_root, None, InstallMode::Copy, false).unwrap();
        let err = install_skill(&src, &dest_root, None, InstallMode::Copy, false).unwrap_err();
        assert!(matches!(err, SkillError::DestinationExists(_)));
    }

    #[test]
    fn install_force_replaces_existing() {
        let tmp = tempdir().unwrap();
        let src = make_skill(tmp.path(), "commit-style");
        let dest_root = tmp.path().join("dest");
        install_skill(&src, &dest_root, None, InstallMode::Copy, false).unwrap();
        fs::write(dest_root.join("commit-style").join("stale.txt"), "x").unwrap();
        install_skill(&src, &dest_root, None, InstallMode::Copy, true).unwrap();
        assert!(!dest_root.join("commit-style").join("stale.txt").exists());
        assert!(dest_root.join("commit-style").join("SKILL.md").is_file());
    }

    #[test]
    fn install_rejects_non_skill_source() {
        let tmp = tempdir().unwrap();
        let not_a_skill = tmp.path().join("nope");
        fs::create_dir_all(&not_a_skill).unwrap();
        let err = install_skill(
            &not_a_skill,
            &tmp.path().join("dest"),
            None,
            InstallMode::Copy,
            false,
        )
        .unwrap_err();
        assert!(matches!(err, SkillError::Io { .. }));
    }

    /// Regression (path traversal via install): a `--name ../escaped` override
    /// must be rejected by name validation and must not create anything outside
    /// `dest_root`.
    #[test]
    fn install_rejects_unsafe_name_override() {
        let tmp = tempdir().unwrap();
        let src = make_skill(tmp.path(), "commit-style");
        let dest_root = tmp.path().join("dest");
        fs::create_dir_all(&dest_root).unwrap();
        let err = install_skill(
            &src,
            &dest_root,
            Some("../escaped"),
            InstallMode::Copy,
            false,
        )
        .unwrap_err();
        assert!(matches!(err, SkillError::InvalidName { .. }), "got {err:?}");
        assert!(!tmp.path().join("escaped").exists(), "escaped dest_root!");
    }

    #[cfg(unix)]
    #[test]
    fn install_link_creates_symlink_to_source() {
        let tmp = tempdir().unwrap();
        let src = make_skill(tmp.path(), "commit-style");
        let dest_root = tmp.path().join("dest");
        let dest = install_skill(&src, &dest_root, None, InstallMode::Link, false).unwrap();
        assert!(fs::symlink_metadata(&dest)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(dest.join("SKILL.md").is_file());
        assert_eq!(discover(&dest_root).len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn install_force_replaces_a_symlink() {
        let tmp = tempdir().unwrap();
        let src = make_skill(tmp.path(), "commit-style");
        let dest_root = tmp.path().join("dest");
        install_skill(&src, &dest_root, None, InstallMode::Link, false).unwrap();
        let dest = install_skill(&src, &dest_root, None, InstallMode::Copy, true).unwrap();
        assert!(!fs::symlink_metadata(&dest)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    /// Grounds `copy_dir`'s symlink handling: a skill folder containing a
    /// symlinked subdirectory copies successfully (the link is recreated
    /// verbatim, not followed — which previously failed outright).
    #[cfg(unix)]
    #[test]
    fn install_copy_recreates_symlinked_subdir() {
        let tmp = tempdir().unwrap();
        let src = make_skill(tmp.path(), "commit-style");
        let target = tmp.path().join("target-dir");
        fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, src.join("linkdir")).unwrap();

        let dest = install_skill(
            &src,
            &tmp.path().join("dest"),
            None,
            InstallMode::Copy,
            false,
        )
        .unwrap();
        assert!(dest.join("SKILL.md").is_file());
        let link = dest.join("linkdir");
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_link(&link).unwrap(), target);
    }

    /// Grounds `copy_dir`'s loop-safety: a self-referential symlink inside the
    /// folder is recreated verbatim rather than recursed into, so the copy
    /// terminates.
    #[cfg(unix)]
    #[test]
    fn install_copy_survives_self_referential_symlink() {
        let tmp = tempdir().unwrap();
        let src = make_skill(tmp.path(), "commit-style");
        std::os::unix::fs::symlink(&src, src.join("selfie")).unwrap();
        let dest = install_skill(
            &src,
            &tmp.path().join("dest"),
            None,
            InstallMode::Copy,
            false,
        )
        .unwrap();
        assert!(dest.join("SKILL.md").is_file());
        assert!(fs::symlink_metadata(dest.join("selfie"))
            .unwrap()
            .file_type()
            .is_symlink());
    }
}

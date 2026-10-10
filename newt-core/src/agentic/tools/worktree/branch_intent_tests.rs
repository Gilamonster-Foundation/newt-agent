use super::*;

fn known(name: &str, checkout: bool) -> bool {
    name == "existing" || (checkout && name == "seed")
}

/// PR #2853: queries, existing switches and explicit paths are not creation.
#[test]
fn non_creating_wrappers_preserve_confined_execution() {
    for source in [
        "git branch --show-current | head",
        "git branch --list | head",
        "git branch -v | head",
        "git branch -vv | head",
        "git branch -a | head",
        "git branch -r | head",
        "git branch -avv | head",
        "git branch --contains HEAD | head",
        "git branch --merged=HEAD | head",
        "git checkout HEAD seed && git status",
        "git branch existing --list | head",
        "git rev-parse HEAD | head",
        "git rev-parse \"$revision\" | head",
        "git log --format=\"$format\" | head",
        "git switch existing && git status",
        "git checkout existing && git status",
        "git checkout seed && git status",
        "git checkout -- -b && git status",
        "git checkout HEAD -- seed | head",
        "git switch --detach HEAD && git status",
        "git switch --no-guess missing && git status",
        "echo 'git checkout -b task'",
        "printf '%s' 'git branch task'",
    ] {
        assert_eq!(classify(source, &known), None, "{source}");
    }
}

/// Every creation spelling remains refused; reset/orphan advice retains intent.
#[test]
fn creation_options_do_not_fall_through() {
    for source in [
        "git branch -v task | head",
        "git branch -vv task | head",
        "git checkout -b task | head",
        "git checkout -btask | head",
        "git switch -c task && echo done",
        "git switch --create task | tail -3",
        "git switch --create=task | head",
        "git -C. checkout -b task | tail -3",
        "git -C./ checkout -b task | tail -3",
        "git branch --no-track task HEAD | head",
        "git branch task; echo done",
        "LANG=C git checkout -b task; echo done",
    ] {
        assert_eq!(
            classify(source, &known),
            Some(Intent::Create("git checkout -b task".into())),
            "{source}"
        );
    }
    for command in [
        "git checkout -B task",
        "git switch -C task",
        "git switch --force-create task",
        "git checkout --orphan task",
        "git switch --orphan task",
        "git branch -C existing task",
        "git branch task other-start",
        "git switch --create task other-start",
        "git checkout -btask other-start",
        "git switch --track -c task",
    ] {
        assert_eq!(
            classify(&format!("{command}; echo done"), &known),
            Some(Intent::Create(command.into())),
            "{command}"
        );
    }
    assert_eq!(
        classify("git switch --create=task other-start; echo done", &known),
        Some(Intent::Create(
            "git switch '--create=task' other-start".into()
        ))
    );
}

/// Unknown flags/targets and opaque dispatch must not recommend new creation.
#[test]
fn ambiguous_commands_get_their_own_standalone_retry() {
    for command in [
        "git switch missing",
        "git checkout missing",
        "git switch --future-create-option",
        "git branch --set-upstream-to origin/main existing",
        "git branch --list -D existing",
        "git -C elsewhere switch existing",
    ] {
        assert_eq!(
            classify(&format!("{command}; echo done"), &known),
            Some(Intent::Ambiguous(Some(command.into()))),
            "{command}"
        );
    }
    assert_eq!(
        classify("env LANG=C git checkout -b task; echo done", &known),
        Some(Intent::Ambiguous(None))
    );
    assert!(matches!(
        classify("GIT_DIR=elsewhere git switch existing; echo done", &known),
        Some(Intent::Ambiguous(_))
    ));
    assert!(matches!(
        classify("cd elsewhere; git switch existing", &known),
        Some(Intent::Ambiguous(_))
    ));
    assert!(matches!(
        classify("git switch existing; git branch task", &known),
        Some(Intent::Create(_))
    ));
}

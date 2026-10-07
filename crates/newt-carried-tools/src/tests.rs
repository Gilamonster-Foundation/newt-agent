use super::translate;
use std::ffi::OsString;
fn translated(args: &[&str]) -> Vec<String> {
    translate(&args.iter().map(OsString::from).collect::<Vec<_>>())
        .unwrap()
        .args
        .into_iter()
        .map(|v| v.into_string().unwrap())
        .collect()
}
#[test]
fn issue_2776_recursive_cluster_is_not_replacement() {
    let args = translated(&["-rn", "alpha", "src"]);
    assert!(args.contains(&"--line-number".into()));
    assert!(!args.contains(&"-r".into()));
    assert!(args.ends_with(&["--".into(), "src".into()]));
}
#[test]
fn issue_2776_extended_is_not_encoding_and_count_includes_zero() {
    let args = translated(&["-Ec", "alpha|beta", "file"]);
    assert!(args.contains(&"--include-zero".into()));
    assert!(args.windows(2).any(|a| a == ["--regexp", "alpha|beta"]));
}
#[test]
fn option_arity_and_end_of_options_preserve_operands() {
    let args = translated(&["-ine", "-pattern", "-eother", "--", "-file"]);
    assert!(args.windows(2).any(|a| a == ["--regexp", "-pattern"]));
    assert!(args.windows(2).any(|a| a == ["--regexp", "other"]));
    assert!(args.ends_with(&["--".into(), "-file".into()]));
}
#[test]
fn refuses_unsupported_and_missing_options() {
    for args in [vec!["-q", "x"], vec!["--replace", "x"], vec!["-e"], vec![]] {
        assert!(translate(&args.into_iter().map(OsString::from).collect::<Vec<_>>()).is_err());
    }
}
#[test]
fn basic_regex_does_not_silently_become_extended() {
    let args = translated(&["alpha|beta+(x)"]);
    assert!(args
        .windows(2)
        .any(|a| a == ["--regexp", r"alpha\|beta\+\(x\)"]));
    assert_eq!(args.last().unwrap(), "-");
}
#[test]
fn all_supported_switches_have_explicit_translations() {
    let args = translated(&["-RFnviwoHh", "-e", "x", "dir"]);
    for flag in [
        "--follow",
        "--fixed-strings",
        "--line-number",
        "--invert-match",
        "--ignore-case",
        "--word-regexp",
        "--only-matching",
        "--with-filename",
        "--no-filename",
    ] {
        assert!(args.contains(&flag.into()), "{flag}");
    }
}

#[test]
fn counts_lines_even_when_only_matching_is_requested() {
    // #2776: rg -co counts occurrences, while grep -co counts lines.
    let args = translated(&["-co", "x", "file"]);
    assert!(!args.contains(&"--only-matching".into()));
    assert!(args.contains(&"--count".into()));
}
#[test]
fn files_with_matches_takes_priority_over_count_in_either_order() {
    for flags in ["-lc", "-cl"] {
        let args = translated(&[flags, "x", "file"]);
        assert!(!args.contains(&"--count".into()));
        assert!(args.contains(&"--files-with-matches".into()));
    }
}

#[test]
fn inverted_only_matching_suppresses_output_without_short_circuiting_errors() {
    let args = ["-vo", "x", "file"].map(OsString::from);
    assert!(translate(&args).unwrap().suppress_stdout);
    let args = ["-voc", "x", "file"].map(OsString::from);
    assert!(!translate(&args).unwrap().suppress_stdout);
}

#[test]
fn basic_brackets_preserve_negation_and_literal_closing_bracket() {
    for (pattern, expected) in [("[^^]", "[^^]"), ("[]x]", r"[\]x]"), ("[^]x]", r"[^\]x]")] {
        let args = translated(&[pattern]);
        assert!(args.windows(2).any(|a| a == ["--regexp", expected]));
    }
}
#[test]
fn invalid_bre_is_refused_instead_of_changing_regex_dialect() {
    for pattern in [r"\1", r"\(x\)", "[[:alpha:]]", "[a&&b]", "[", "x\\"] {
        assert!(translate(&[OsString::from(pattern)]).is_err(), "{pattern}");
    }
}

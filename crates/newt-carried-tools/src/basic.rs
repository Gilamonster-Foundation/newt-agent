//! A deliberately bounded BRE-to-rg translation; unsupported constructs fail.
pub(super) fn translate(pattern: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut chars = pattern.chars().peekable();
    let mut bracket = false;
    let mut bracket_first = false;
    let mut negated = false;
    let mut repeatable = false;
    let mut repeated = false;
    while let Some(c) = chars.next() {
        if c == '\\' {
            let next = chars.next().ok_or("trailing backslash in pattern")?;
            if next.is_ascii_alphanumeric() || "(){}+?|<>".contains(next) {
                return Err("unsupported BRE escape; use -E for extended expressions (backreferences are unsupported)".into());
            }
            if bracket {
                return Err("escapes in BRE bracket expressions are unsupported; use -E".into());
            }
            if ".*^$[]\\".contains(next) {
                out.push('\\');
            }
            out.push(next);
            repeatable = true;
            repeated = false;
        } else if bracket {
            // Nested/POSIX classes and set operations need a separate grammar.
            if c == '[' || ("&~-".contains(c) && chars.peek() == Some(&c)) {
                return Err("unsupported bracket expression; use -E".into());
            }
            if c == ']' && !bracket_first {
                bracket = false;
                repeatable = true;
                repeated = false;
            }
            if c == ']' && bracket_first {
                out.push('\\');
            }
            out.push(c);
            if c == '^' && bracket_first && !negated {
                negated = true;
            } else {
                bracket_first = false;
            }
        } else {
            if c == '[' {
                bracket = true;
                bracket_first = true;
                negated = false;
            }
            // Groups are refused above, so only the whole-pattern edges anchor.
            // An initial star (also immediately after the initial ^) is literal.
            match c {
                '^' if out.is_empty() => repeatable = false,
                '$' if chars.peek().is_none() => repeatable = false,
                '*' if repeatable => {
                    if repeated {
                        return Err("repeated BRE quantifiers are unsupported; use -E".into());
                    }
                    repeated = true;
                }
                _ => {
                    if "()+?{}|^$*".contains(c) {
                        out.push('\\');
                    }
                    repeatable = true;
                    repeated = false;
                }
            }
            out.push(c);
        }
    }
    if bracket {
        return Err("unterminated bracket expression".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::translate;

    #[test]
    fn issue_2776_bre_anchors_and_stars_are_positional() {
        // #2790 review: interior anchors and leading stars must not become rg operators.
        let mut mismatches = Vec::new();
        for (pattern, expected) in [
            ("a^b", r"a\^b"),
            ("a$b", r"a\$b"),
            ("*", r"\*"),
            ("*alpha", r"\*alpha"),
            ("^*", r"^\*"),
            ("^*alpha$", r"^\*alpha$"),
            ("^alpha$", "^alpha$"),
            ("^a*$", "^a*$"),
            ("a*b", "a*b"),
            ("^^alpha$$", r"^\^alpha\$$"),
            (r"\^*", r"\^*"),
            ("[a^$*]*", "[a^$*]*"),
        ] {
            let actual = translate(pattern).unwrap();
            if actual != expected {
                mismatches.push((pattern, expected, actual));
            }
        }
        assert!(
            mismatches.is_empty(),
            "positional BRE mismatches: {mismatches:?}"
        );
    }
    #[test]
    fn issue_2776_ambiguous_bre_repetition_and_groups_are_refused() {
        // Groups are outside the bounded grammar; do not guess the meaning of * after \(.
        for pattern in [r"\(*\)", "a**", "^a**$"] {
            let error = crate::translate(&[std::ffi::OsString::from(pattern)]).unwrap_err();
            assert!(error.contains("supported:"), "{error}");
        }
    }
}

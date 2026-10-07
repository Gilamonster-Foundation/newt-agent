//! A deliberately bounded BRE-to-rg translation; unsupported constructs fail.
pub(super) fn translate(pattern: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut chars = pattern.chars().peekable();
    let mut bracket = false;
    let mut bracket_first = false;
    let mut negated = false;
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
        } else if bracket {
            // Nested/POSIX classes and set operations need a separate grammar.
            if c == '[' || ("&~-".contains(c) && chars.peek() == Some(&c)) {
                return Err("unsupported bracket expression; use -E".into());
            }
            if c == ']' && !bracket_first {
                bracket = false;
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
            if "()+?{}|".contains(c) {
                out.push('\\');
            }
            out.push(c);
        }
    }
    if bracket {
        return Err("unterminated bracket expression".into());
    }
    Ok(out)
}

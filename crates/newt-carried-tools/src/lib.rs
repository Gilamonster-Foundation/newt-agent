//! Bounded grep compatibility over a sibling ripgrep executable.
mod basic;
use std::ffi::{OsStr, OsString};
use std::path::Path;

const SUPPORTED: &str = "supported: -r -R -E -F -n -c -v -i -l -w -o -H -h -e PATTERN and --";
fn refusal(reason: &str) -> String {
    format!("{reason}; {SUPPORTED}. Use rg directly for ripgrep-specific syntax.")
}

/// Transient invocation plan, never persisted or transmitted.
#[derive(Debug)]
pub struct Invocation {
    pub args: Vec<OsString>,
    pub suppress_stdout: bool,
}

/// Parse grep argv without a shell. File operands retain their OS-native bytes.
/// Default regex is the documented BRE subset; -E selects rg's ERE subset.
pub fn translate(args: &[OsString]) -> Result<Invocation, String> {
    let mut flags: Vec<OsString> = [
        "--no-config",
        "--color=never",
        "--no-heading",
        "--no-line-number",
        "--no-ignore",
        "--hidden",
        "--text",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    let mut patterns = Vec::new();
    let mut operands = Vec::new();
    let mut explicit_pattern = false;
    let mut recursive = false;
    let mut mode = 'B';
    let mut options = true;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if options && arg == "--" {
            options = false;
            continue;
        }
        let text = arg.to_str();
        if options && text.is_some_and(|s| s.starts_with('-') && s != "-") {
            let text = text.expect("checked UTF-8 option");
            let letters = text[1..].char_indices();
            for (index, flag) in letters {
                let translated = match flag {
                    'r' => {
                        recursive = true;
                        None
                    }
                    'R' => {
                        recursive = true;
                        Some("--follow")
                    }
                    'E' | 'F' => {
                        if mode != 'B' && mode != flag {
                            return Err(refusal("conflicting regex modes"));
                        }
                        mode = flag;
                        None
                    }
                    'n' => Some("--line-number"),
                    'c' => {
                        flags.push("--include-zero".into());
                        Some("--count")
                    }
                    'v' => Some("--invert-match"),
                    'i' => Some("--ignore-case"),
                    'l' => Some("--files-with-matches"),
                    'w' => Some("--word-regexp"),
                    'o' => Some("--only-matching"),
                    'H' => Some("--with-filename"),
                    'h' => Some("--no-filename"),
                    'e' => {
                        let remainder = &text[index + 2..];
                        let pattern = if remainder.is_empty() {
                            iter.next()
                                .ok_or_else(|| refusal("-e requires a pattern"))?
                                .clone()
                        } else {
                            remainder.into()
                        };
                        patterns.push(pattern);
                        explicit_pattern = true;
                        break;
                    }
                    _ => return Err(refusal(&format!("unsupported option {arg:?}"))),
                };
                if let Some(value) = translated {
                    flags.push(value.into());
                }
            }
        } else {
            operands.push(arg.clone());
        }
    }
    if !explicit_pattern {
        if operands.is_empty() {
            return Err(refusal("missing pattern"));
        }
        patterns.push(operands.remove(0));
    }
    // GNU grep's output modes have precedence independent of argv order.
    let lists = flags.iter().any(|f| f == "--files-with-matches");
    let counts = flags.iter().any(|f| f == "--count");
    let only = flags.iter().any(|f| f == "--only-matching");
    let invert = flags.iter().any(|f| f == "--invert-match");
    if lists {
        flags.retain(|f| f != "--count" && f != "--include-zero" && f != "--only-matching");
    } else if counts {
        flags.retain(|f| f != "--only-matching");
    }
    let suppress_stdout = only && invert && !lists && !counts;
    if mode == 'F' {
        flags.push("--fixed-strings".into());
    }
    for pattern in patterns {
        let text = pattern
            .to_str()
            .ok_or_else(|| refusal("patterns must be UTF-8"))?;
        // grep's -e value is a newline-separated list, even in fixed-string mode.
        for line in text.split('\n') {
            flags.push("--regexp".into());
            flags.push(if mode == 'B' {
                basic::translate(line).map_err(|e| refusal(&e))?.into()
            } else {
                line.into()
            });
        }
    }
    if operands.is_empty() {
        operands.push(if recursive { "." } else { "-" }.into());
    }
    if !recursive
        && operands
            .iter()
            .any(|v| v != OsStr::new("-") && Path::new(v).is_dir())
    {
        return Err(refusal("directory operand requires -r or -R"));
    }
    flags.push("--".into());
    flags.extend(operands);
    Ok(Invocation {
        args: flags,
        suppress_stdout,
    })
}

#[cfg(test)]
mod tests;

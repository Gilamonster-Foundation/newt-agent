//! Conservative sentence boundaries and reversible inline host presentation.
use std::{ops::Range, sync::LazyLock};
static CODE: LazyLock<regex::Regex> = LazyLock::new(|| super::regex(r"`+[^`\n]*`+"));

/// Semicolons and sentence ends are safe boundaries outside inline code. An
/// inseparable mixed clause is handled conservatively by the family classifiers.
pub(super) fn claims(line: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = 0;
    let code: Vec<_> = CODE.find_iter(line).map(|m| m.range()).collect();
    for (i, c) in line.char_indices() {
        let end = i + c.len_utf8();
        if !code.iter().any(|range| range.contains(&i))
            && (c == ';'
                || (matches!(c, '.' | '!' | '?') && line[end..].starts_with(char::is_whitespace)))
        {
            let boundary = if c == ';' { i } else { end };
            push_trimmed(&mut ranges, line, start..boundary);
            start = end;
        }
    }
    push_trimmed(&mut ranges, line, start..line.len());
    ranges
}
fn push_trimmed(ranges: &mut Vec<Range<usize>>, text: &str, range: Range<usize>) {
    let chunk = &text[range.clone()];
    let start = range.start + chunk.len() - chunk.trim_start().len();
    let end = range.end - (chunk.len() - chunk.trim_end().len());
    if start < end {
        ranges.push(start..end);
    }
}

pub(super) fn escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('[', "\\[")
        .replace(']', "\\]")
}

/// Generated marker bodies escape brackets. Authored marker introducers are
/// escaped separately; remove exactly that presentation escape on replay.
pub(super) fn project(line: &str) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < line.len() {
        let rest = &line[i..];
        if let Some(code) = rest
            .starts_with('`')
            .then(|| CODE.find(rest))
            .flatten()
            .filter(|m| m.start() == 0)
        {
            out.push_str(code.as_str());
            i += code.end();
            continue;
        }
        let slash_count = rest.bytes().take_while(|b| *b == b'\\').count();
        if slash_count > 0 && super::marker(&rest[slash_count..]) {
            out.push_str(&rest[1..slash_count + 1]);
            i += slash_count + 1;
            continue;
        }
        if super::marker(rest) {
            let mut escaped = false;
            if let Some(end) = rest.char_indices().skip(1).find_map(|(j, c)| {
                if escaped {
                    escaped = false;
                    return None;
                }
                if c == '\\' {
                    escaped = true;
                    return None;
                }
                (c == ']').then_some(j + 1)
            }) {
                i += end;
                continue;
            }
        }
        let c = rest.chars().next().expect("nonempty suffix");
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// Locate authored marker introducers outside inline code for reversible
/// escaping; ranges cover existing backslashes as well as the opening bracket.
pub(super) fn authored(line: &str) -> Vec<Range<usize>> {
    let mut ranges = vec![];
    let code: Vec<_> = CODE.find_iter(line).map(|m| m.range()).collect();
    for (i, _) in line.char_indices() {
        if !code.iter().any(|range| range.contains(&i)) && super::marker(&line[i..]) {
            let slashes = line[..i].bytes().rev().take_while(|b| *b == b'\\').count();
            ranges.push(i - slashes..i + 1);
        }
    }
    ranges
}

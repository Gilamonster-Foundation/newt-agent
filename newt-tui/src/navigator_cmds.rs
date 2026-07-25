//! #1387 Code Navigator slash-command parse/format helpers (Phases 2–4).
//! Execution (indexes, IO) stays in `chat.rs`; this module is pure.

/// Parsed navigator slash command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NavCommand {
    Def(String),
    Text(String),
    Uses(String),
    Tests(String),
    Map {
        expand: Option<String>,
    },
    Callers(String),
    Callees(String),
    Implementations(String),
    Hierarchy(String),
    Type(String),
    Impact(String),
    Retrieval {
        turn: Option<u64>,
        view: RetrievalView,
    },
    CompareSemanticLexical,
    CompareTurns(u64, u64),
    CompareIndex,
    ExportJson,
    ExportMarkdown,
    Help(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetrievalView {
    Human,
    Model,
    Diff,
}

/// Parse a `/def` / `/text` / … navigator command. `None` when not a nav verb.
#[must_use]
pub(crate) fn parse_nav_command(input: &str) -> Option<Result<NavCommand, String>> {
    let body = input.trim().trim_start_matches('/').trim();
    let (verb, rest) = match body.split_once(char::is_whitespace) {
        Some((v, r)) => (v, r.trim()),
        None => (body, ""),
    };
    let help = |msg: &'static str| Some(Ok(NavCommand::Help(msg)));
    match verb {
        "def" | "goto" => {
            if rest.is_empty() || rest == "help" {
                return help("usage: /def <symbol>");
            }
            Some(Ok(NavCommand::Def(rest.to_string())))
        }
        "text" | "grep" => {
            if rest.is_empty() || rest == "help" {
                return help("usage: /text <regex>");
            }
            Some(Ok(NavCommand::Text(rest.to_string())))
        }
        "uses" | "refs" => {
            if rest.is_empty() {
                return help("usage: /uses <symbol>");
            }
            Some(Ok(NavCommand::Uses(rest.to_string())))
        }
        "tests" => {
            if rest.is_empty() {
                return help("usage: /tests <symbol>");
            }
            Some(Ok(NavCommand::Tests(rest.to_string())))
        }
        "map" => {
            let expand = if rest.is_empty() {
                None
            } else {
                Some(rest.to_string())
            };
            Some(Ok(NavCommand::Map { expand }))
        }
        "callers" => {
            if rest.is_empty() {
                return help("usage: /callers <symbol>");
            }
            Some(Ok(NavCommand::Callers(rest.to_string())))
        }
        "callees" => {
            if rest.is_empty() {
                return help("usage: /callees <symbol>");
            }
            Some(Ok(NavCommand::Callees(rest.to_string())))
        }
        "implementations" | "impls" => {
            if rest.is_empty() {
                return help("usage: /implementations <symbol>");
            }
            Some(Ok(NavCommand::Implementations(rest.to_string())))
        }
        "hierarchy" => {
            if rest.is_empty() {
                return help("usage: /hierarchy <symbol>");
            }
            Some(Ok(NavCommand::Hierarchy(rest.to_string())))
        }
        "type" | "inspect" => {
            if rest.is_empty() {
                return help("usage: /type <symbol>");
            }
            Some(Ok(NavCommand::Type(rest.to_string())))
        }
        "impact" => {
            if rest.is_empty() {
                return help("usage: /impact <unit>");
            }
            Some(Ok(NavCommand::Impact(rest.to_string())))
        }
        "retrieval" => Some(parse_retrieval(rest)),
        "compare" => Some(parse_compare(rest)),
        "export" => Some(parse_export(rest)),
        _ => None,
    }
}

fn parse_retrieval(rest: &str) -> Result<NavCommand, String> {
    if rest.is_empty() || rest == "help" {
        return Ok(NavCommand::Help(
            "usage: /retrieval [turn N] [human|model|diff]",
        ));
    }
    let mut turn = None;
    let mut view = RetrievalView::Human;
    let mut parts = rest.split_whitespace().peekable();
    while let Some(p) = parts.next() {
        match p {
            "turn" => {
                let n = parts
                    .next()
                    .ok_or_else(|| "usage: /retrieval turn <N>".to_string())?
                    .parse::<u64>()
                    .map_err(|_| "usage: /retrieval turn <N>".to_string())?;
                turn = Some(n);
            }
            "human" => view = RetrievalView::Human,
            "model" => view = RetrievalView::Model,
            "diff" => view = RetrievalView::Diff,
            other if other.chars().all(|c| c.is_ascii_digit()) => {
                turn = Some(other.parse().map_err(|_| "bad turn number".to_string())?);
            }
            other => return Err(format!("unknown /retrieval arg '{other}'")),
        }
    }
    Ok(NavCommand::Retrieval { turn, view })
}

fn parse_compare(rest: &str) -> Result<NavCommand, String> {
    let parts: Vec<&str> = rest.split_whitespace().collect();
    match parts.as_slice() {
        [] | ["help"] => Ok(NavCommand::Help(
            "usage: /compare semantic lexical | /compare turn A B | /compare index",
        )),
        ["semantic", "lexical"] | ["lexical", "semantic"] => Ok(NavCommand::CompareSemanticLexical),
        ["index"] | ["previous"] | ["current"] => Ok(NavCommand::CompareIndex),
        ["turn", a, b] => {
            let a = a.parse().map_err(|_| "bad turn A".to_string())?;
            let b = b.parse().map_err(|_| "bad turn B".to_string())?;
            Ok(NavCommand::CompareTurns(a, b))
        }
        [a, b]
            if a.chars().all(|c| c.is_ascii_digit()) && b.chars().all(|c| c.is_ascii_digit()) =>
        {
            Ok(NavCommand::CompareTurns(
                a.parse().unwrap(),
                b.parse().unwrap(),
            ))
        }
        _ => Err(format!("unknown /compare args: {rest}")),
    }
}

fn parse_export(rest: &str) -> Result<NavCommand, String> {
    match rest.trim() {
        "" | "help" => Ok(NavCommand::Help(
            "usage: /export json | /export markdown  (retrieval ledger)",
        )),
        "json" => Ok(NavCommand::ExportJson),
        "markdown" | "md" => Ok(NavCommand::ExportMarkdown),
        other => Err(format!("unknown /export arg '{other}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_def_and_text() {
        assert_eq!(
            parse_nav_command("/def run_crew").unwrap().unwrap(),
            NavCommand::Def("run_crew".into())
        );
        assert_eq!(
            parse_nav_command("/text foo.*bar").unwrap().unwrap(),
            NavCommand::Text("foo.*bar".into())
        );
    }

    #[test]
    fn parse_retrieval_and_compare() {
        assert_eq!(
            parse_nav_command("/retrieval turn 2 model")
                .unwrap()
                .unwrap(),
            NavCommand::Retrieval {
                turn: Some(2),
                view: RetrievalView::Model,
            }
        );
        assert_eq!(
            parse_nav_command("/compare semantic lexical")
                .unwrap()
                .unwrap(),
            NavCommand::CompareSemanticLexical
        );
        assert_eq!(
            parse_nav_command("/compare turn 1 3").unwrap().unwrap(),
            NavCommand::CompareTurns(1, 3)
        );
        assert_eq!(
            parse_nav_command("/export json").unwrap().unwrap(),
            NavCommand::ExportJson
        );
        assert_eq!(
            parse_nav_command("/retrieval turn 3 diff")
                .unwrap()
                .unwrap(),
            NavCommand::Retrieval {
                turn: Some(3),
                view: RetrievalView::Diff,
            }
        );
    }

    #[test]
    fn parse_map_expand() {
        assert_eq!(
            parse_nav_command("/map").unwrap().unwrap(),
            NavCommand::Map { expand: None }
        );
        assert_eq!(
            parse_nav_command("/map newt-core").unwrap().unwrap(),
            NavCommand::Map {
                expand: Some("newt-core".into())
            }
        );
    }

    #[test]
    fn unknown_verb_is_none() {
        assert!(parse_nav_command("/models").is_none());
    }
}

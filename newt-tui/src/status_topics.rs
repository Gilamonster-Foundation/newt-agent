//! **`/status <topic>` — one read verb with eight topics** (#2009 PR3).
//!
//! Nine commands printed nine kinds of read-only state, each with its own
//! top-level verb, its own help line and its own place in the operator's
//! memory. The radical cut folds them into one verb with topics, which is the
//! answer criterion 1 gives for a read: it performs, so it stays a verb — but
//! it does not need nine names.
//!
//! # Why this is a router and not nine moved handlers
//!
//! The handlers live in `chat.rs`, inside the session loop, reading locals
//! that have not been relocated to core yet (§5's precondition gate). Moving
//! them is PR7/PR9/PR10 work with its own relocation step; folding their
//! NAMES needs none of it.
//!
//! So `/status models` rewrites to `/models` and runs the existing arm. That
//! is not a shortcut — it is the property worth having: **one implementation,
//! reached by two doors**, which cannot drift because there is nothing to
//! drift from. A second renderer for `/status models` would be exactly the
//! sprawl `CLAUDE.md` measures in spinners.
//!
//! # Why the retired reads keep printing
//!
//! `/thinking` retired by redirecting and mutating NOTHING, because a
//! half-working mutator shim never gets to die. A read is not that: printing
//! twice harms no one, and §3.3's headless rule is explicit that **reads
//! print; they never open**. `newt solve`, the eval harness and wyvern all
//! read `/version` and `/workspace` off a pipe today.
//!
//! So the rule this module encodes, and that `slash_registry` tests:
//! **a retired MUTATOR must not mutate; a retired READ may still read.** The
//! verbs go on working through the deprecation window and are deleted
//! together in PR14b; what retires now is their claim on the top-level
//! surface and their nine help lines.

/// One topic of `/status`, and the command line it is a name for.
///
/// `routes_to` is the existing command, argument included — data, not a match
/// arm, so adding a topic is a row rather than a branch (`CLAUDE.md`'s three
/// Cs) and so the ONE list here is also what the help text and the refusal
/// enumerate. Three copies of this vocabulary is how it drifts.
pub(crate) struct Topic {
    pub(crate) name: &'static str,
    pub(crate) routes_to: &'static str,
}

/// The eight topics. `info` is the default view and is reached by bare
/// `/status` as well as by name.
pub(crate) const TOPICS: &[Topic] = &[
    Topic {
        name: "info",
        routes_to: "info",
    },
    Topic {
        name: "config",
        routes_to: "config show",
    },
    Topic {
        name: "version",
        routes_to: "version",
    },
    Topic {
        name: "workspace",
        routes_to: "workspace",
    },
    Topic {
        name: "loadout",
        routes_to: "loadout",
    },
    Topic {
        name: "byline",
        routes_to: "byline",
    },
    Topic {
        name: "memory",
        routes_to: "memory",
    },
    Topic {
        name: "models",
        routes_to: "models",
    },
];

/// What `route` decided about an operator's line.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Route {
    /// A `/status <topic>` line, rewritten to the command it names.
    Topic(String),
    /// `/status <something-else>` — refuse by NAME rather than falling
    /// through to a generic "unknown command", which would send the operator
    /// to `/help` to look for a verb that no longer exists.
    Unknown(String),
    /// Not a `/status <topic>` line; the caller's input stands unchanged.
    /// Bare `/status` lands here deliberately — the default view is the
    /// existing arm, and rewriting it would only add a hop.
    Passthrough,
}

/// Rewrite `/status <topic> [rest]` into the command that topic names.
///
/// The trailing arguments ride along, so `/status memory stats` reaches the
/// same handler `/memory stats` does. A topic is matched whole: `/status
/// modelsx` is unknown, not `models` with a stray suffix.
pub(crate) fn route(input: &str) -> Route {
    let trimmed = input.trim();
    let Some(body) = trimmed.strip_prefix('/') else {
        return Route::Passthrough;
    };
    let body = body.trim_start_matches('/');
    let Some(rest) = body.strip_prefix("status") else {
        return Route::Passthrough;
    };
    // `/statusx` is not `/status` with an argument.
    let rest = match rest.chars().next() {
        None => return Route::Passthrough, // bare `/status`: the default view
        Some(c) if c.is_whitespace() => rest.trim(),
        Some(_) => return Route::Passthrough,
    };
    if rest.is_empty() {
        return Route::Passthrough;
    }
    let (topic, tail) = rest
        .split_once(char::is_whitespace)
        .map_or((rest, ""), |(t, r)| (t, r.trim()));
    match TOPICS.iter().find(|t| t.name == topic) {
        Some(found) => {
            let mut out = format!("/{}", found.routes_to);
            if !tail.is_empty() {
                out.push(' ');
                out.push_str(tail);
            }
            Route::Topic(out)
        }
        None => Route::Unknown(topic.to_string()),
    }
}

/// The refusal for an unrecognized topic, naming every topic that exists.
///
/// Built from `TOPICS` so it cannot list a topic the router does not accept —
/// the failure mode of a hand-written list beside a match.
pub(crate) fn unknown_topic_message(topic: &str) -> String {
    let names: Vec<&str> = TOPICS.iter().map(|t| t.name).collect();
    format!(
        "/status has no `{topic}` topic. Try: {}  (bare /status shows the default view)",
        names.join(" · ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_topic_rewrites_to_the_command_it_names() {
        for topic in TOPICS {
            assert_eq!(
                route(&format!("/status {}", topic.name)),
                Route::Topic(format!("/{}", topic.routes_to)),
                "topic `{}` did not route",
                topic.name
            );
        }
    }

    /// Bare `/status` is the default view, not a rewrite — the existing arm
    /// already renders it, and a hop through the router would only add a way
    /// for the two to disagree.
    #[test]
    fn bare_status_passes_through() {
        assert_eq!(route("/status"), Route::Passthrough);
        assert_eq!(route("  /status  "), Route::Passthrough);
        assert_eq!(route("//status"), Route::Passthrough);
    }

    /// Arguments ride along, so a topic is a NAME for a command rather than a
    /// narrower version of it.
    #[test]
    fn trailing_arguments_reach_the_handler() {
        assert_eq!(
            route("/status memory stats"),
            Route::Topic("/memory stats".to_string())
        );
        assert_eq!(
            route("/status models capabilities"),
            Route::Topic("/models capabilities".to_string())
        );
        // ...including onto a topic that already carries one.
        assert_eq!(
            route("/status config"),
            Route::Topic("/config show".to_string())
        );
    }

    /// An unknown topic is refused BY NAME. Falling through would reach the
    /// generic "unknown command" and send the operator to `/help` to hunt for
    /// a verb this very PR retired.
    #[test]
    fn an_unknown_topic_is_named_and_the_real_ones_are_listed() {
        assert_eq!(
            route("/status frobnicate"),
            Route::Unknown("frobnicate".to_string())
        );
        let msg = unknown_topic_message("frobnicate");
        assert!(msg.contains("frobnicate"), "{msg}");
        for topic in TOPICS {
            assert!(msg.contains(topic.name), "{msg} omits {}", topic.name);
        }
    }

    /// Whole-word matching, in both directions.
    #[test]
    fn near_misses_are_not_topics() {
        assert_eq!(route("/statusx"), Route::Passthrough);
        assert_eq!(route("/statusx models"), Route::Passthrough);
        assert_eq!(
            route("/status modelsx"),
            Route::Unknown("modelsx".to_string())
        );
        assert_eq!(route("/version"), Route::Passthrough);
        assert_eq!(route("hello /status models"), Route::Passthrough);
    }

    /// **Every topic names a REGISTERED command, and every retired read is
    /// reachable as a topic.** The join, both ways.
    ///
    /// A topic that routes to an unregistered verb is a dead door; a retired
    /// read with no topic is a command the cut removed from the help without
    /// giving it a new name. Both are silent failures — the router would go
    /// on compiling, and the operator would find out.
    #[test]
    fn the_topics_and_the_retired_reads_are_the_same_set() {
        for topic in TOPICS {
            let verb = topic
                .routes_to
                .split_whitespace()
                .next()
                .expect("a topic routes somewhere");
            assert!(
                crate::slash_registry::lookup(verb).is_some(),
                "topic `{}` routes to `/{verb}`, which is not registered",
                topic.name
            );
        }

        // ...and the other direction, over the rows that declare `/status` as
        // their destination.
        for command in crate::slash_registry::COMMANDS {
            let crate::slash_registry::Surface::Retired(dest) = command.surface else {
                continue;
            };
            let Some(topic) = dest.strip_prefix("/status ") else {
                continue;
            };
            assert!(
                TOPICS.iter().any(|t| t.name == topic),
                "`/{}` retires to `/status {topic}`, which is not a topic",
                command.name
            );
        }
    }

    /// Anti-vacuous twin: the loop above proves nothing if no row retires to
    /// `/status`.
    #[test]
    fn something_actually_retired_into_status() {
        let count = crate::slash_registry::COMMANDS
            .iter()
            .filter(|c| {
                matches!(c.surface, crate::slash_registry::Surface::Retired(d)
                    if d.starts_with("/status "))
            })
            .count();
        assert!(
            count >= 7,
            "only {count} rows retired into /status; the fold is eight topics"
        );
    }

    /// Anti-drift: the table is the only list, so nothing here may name a
    /// topic that routes nowhere.
    #[test]
    fn no_topic_routes_to_an_empty_command() {
        for topic in TOPICS {
            assert!(!topic.name.is_empty());
            assert!(!topic.routes_to.is_empty(), "{} routes nowhere", topic.name);
        }
        assert_eq!(TOPICS.len(), 8, "the doc's fold is eight topics");
    }
}

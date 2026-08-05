use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Action<A> {
    pub value: A,
    pub key: String,
    pub label: String,
    /// Hidden input aliases that also select this action (exact match, never
    /// rendered as menu entries). Used to restore ASCII-case-insensitive
    /// confirmation (e.g. `Y` alongside key `y`) WITHOUT globally case-folding
    /// [`Question::parse`] — the general permission menu deliberately keeps
    /// case-distinct keys (`a`/`A`, `d`/`D`), so folding there would let a
    /// weaker answer select a stronger grant. Both serde attrs are load-bearing
    /// together: `default` keeps already-persisted forms (no `aliases`)
    /// deserializable; `skip_serializing_if` keeps the published web/DB JSON
    /// byte-identical when there are no aliases.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}

impl<A> Action<A> {
    pub fn new(value: A, key: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value,
            key: key.into(),
            label: label.into(),
            aliases: Vec::new(),
        }
    }

    /// Attach hidden parse aliases to this action. Chainable on [`Action::new`].
    /// Aliases are matched exactly by [`Question::parse`] and never rendered by
    /// [`Question::terminal_text`].
    #[must_use]
    pub fn with_aliases<S: Into<String>>(mut self, aliases: impl IntoIterator<Item = S>) -> Self {
        self.aliases.extend(aliases.into_iter().map(Into::into));
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question<A> {
    pub markdown: String,
    pub actions: Vec<Action<A>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl<A: AsRef<str> + Clone> Question<A> {
    /// Parse an operator answer to an action, or `None`. This is an authorization
    /// parser, so it is DETERMINISTIC and fails closed on ambiguity — never
    /// "first action wins":
    ///
    /// 1. A canonical match (an action's `key` OR its stable wire `value`) takes
    ///    precedence over any alias. If exactly one action matches canonically,
    ///    return it; if MORE than one does, return `None` (ambiguous).
    /// 2. Only when NO action matches canonically are hidden `aliases`
    ///    considered — again returning `None` if more than one alias matches.
    ///
    /// Consequences: an alias can never shadow another action's real key or wire
    /// value (canonical is checked first), and duplicate keys/values/aliases can
    /// never silently select the earlier action — they deny.
    pub fn parse(&self, input: &str) -> Option<A> {
        let input = input.trim();
        let mut canonical = self
            .actions
            .iter()
            .filter(|a| a.key == input || a.value.as_ref() == input);
        if let Some(first) = canonical.next() {
            return canonical.next().is_none().then(|| first.value.clone());
        }
        let mut aliased = self
            .actions
            .iter()
            .filter(|a| a.aliases.iter().any(|alias| alias == input));
        match (aliased.next(), aliased.next()) {
            (Some(only), None) => Some(only.value.clone()),
            _ => None,
        }
    }
}

impl<A> Question<A> {
    pub fn terminal_text(&self) -> String {
        let choices = self
            .actions
            .iter()
            .map(|a| a.label.replacen(&a.key, &format!("[{}]", a.key), 1))
            .collect::<Vec<_>>()
            .join("   ");
        [&self.markdown, self.note.as_deref().unwrap_or(""), &choices]
            .into_iter()
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A tiny case-distinct action set mirroring the real permission menu, which
    // uses `a`(AllowOnce) vs `A`(AllowPermanent) and `d`(Deny) vs `D`(DenyAlways).
    fn menu() -> Question<&'static str> {
        Question {
            markdown: "pick".into(),
            actions: vec![
                Action::new("allow_once", "a", "allow once"),
                Action::new("allow_permanent", "A", "Allow permanently"),
                Action::new("deny", "d", "deny"),
                Action::new("deny_always", "D", "Deny always"),
            ],
            note: None,
        }
    }

    // The mutation-confirm form: y/Y confirm, n/N deny, via hidden aliases.
    fn confirm() -> Question<&'static str> {
        Question {
            markdown: "confirm?".into(),
            actions: vec![
                Action::new("allow_once", "y", "y to confirm").with_aliases(["Y"]),
                Action::new("deny", "n", "n to skip").with_aliases(["N"]),
            ],
            note: None,
        }
    }

    #[test]
    fn lowercase_y_confirms() {
        assert_eq!(confirm().parse("y"), Some("allow_once"));
    }

    #[test]
    fn uppercase_y_confirms_via_alias() {
        assert_eq!(confirm().parse("Y"), Some("allow_once"));
    }

    #[test]
    fn lower_and_upper_n_deny() {
        assert_eq!(confirm().parse("n"), Some("deny"));
        assert_eq!(confirm().parse("N"), Some("deny"));
    }

    #[test]
    fn empty_unknown_and_malformed_input_parse_to_nothing() {
        for bad in ["", "q", "yes", "y n", "\u{1b}", "yy"] {
            assert_eq!(confirm().parse(bad), None, "input {bad:?} must not parse");
        }
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        assert_eq!(confirm().parse("  Y  "), Some("allow_once"));
        assert_eq!(confirm().parse("\tn\n"), Some("deny"));
    }

    #[test]
    fn general_menu_parsing_stays_case_sensitive() {
        // Aliases must NOT fold case on the general menu: a/A and d/D distinct.
        assert_eq!(menu().parse("a"), Some("allow_once"));
        assert_eq!(menu().parse("A"), Some("allow_permanent"));
        assert_eq!(menu().parse("d"), Some("deny"));
        assert_eq!(menu().parse("D"), Some("deny_always"));
    }

    #[test]
    fn value_wire_name_still_matches() {
        assert_eq!(menu().parse("allow_permanent"), Some("allow_permanent"));
    }

    #[test]
    fn an_alias_cannot_select_an_action_absent_from_the_question() {
        // A deny-only form: "Y" (AllowOnce's alias) must not select anything.
        let deny_only = Question {
            markdown: "x".into(),
            actions: vec![Action::new("deny", "n", "n to skip").with_aliases(["N"])],
            note: None,
        };
        assert_eq!(deny_only.parse("Y"), None);
        assert_eq!(deny_only.parse("y"), None);
    }

    #[test]
    fn aliases_are_not_rendered_in_terminal_text() {
        assert!(!confirm().terminal_text().contains('Y'));
        assert!(!confirm().terminal_text().contains('N'));
    }

    #[test]
    fn empty_aliases_are_omitted_from_the_wire_and_round_trip() {
        // A form without aliases serializes with no `aliases` key at all, and a
        // JSON payload that predates the field still deserializes (serde default).
        let no_alias = Action::new("deny".to_string(), "n", "n to skip");
        let json = serde_json::to_string(&no_alias).unwrap();
        assert!(
            !json.contains("aliases"),
            "empty aliases must not hit the wire: {json}"
        );
        let legacy = r#"{"value":"deny","key":"n","label":"n to skip"}"#;
        let back: Action<String> = serde_json::from_str(legacy).unwrap();
        assert_eq!(back, no_alias);
        assert!(back.aliases.is_empty());
    }

    #[test]
    fn aliased_action_round_trips() {
        let a = Action::new("allow_once".to_string(), "y", "y to confirm").with_aliases(["Y"]);
        let back: Action<String> =
            serde_json::from_str(&serde_json::to_string(&a).unwrap()).unwrap();
        assert_eq!(back, a);
    }

    // ---- deterministic, fail-closed parsing (no "first action wins") ---------

    #[test]
    fn an_alias_never_shadows_another_actions_canonical_key() {
        // "n" is action B's KEY and action A's alias. Canonical must win → Deny.
        let q = Question {
            markdown: "x".into(),
            actions: vec![
                Action::new("allow_once", "y", "Allow").with_aliases(["n"]),
                Action::new("deny", "n", "Deny"),
            ],
            note: None,
        };
        assert_eq!(
            q.parse("n"),
            Some("deny"),
            "canonical key beats an earlier alias"
        );
    }

    #[test]
    fn an_alias_never_shadows_another_actions_wire_value() {
        // "deny" is action B's stable wire value and action A's alias.
        let q = Question {
            markdown: "x".into(),
            actions: vec![
                Action::new("allow_once", "y", "Allow").with_aliases(["deny"]),
                Action::new("deny", "n", "Deny"),
            ],
            note: None,
        };
        assert_eq!(
            q.parse("deny"),
            Some("deny"),
            "canonical value beats an earlier alias"
        );
    }

    #[test]
    fn duplicate_canonical_keys_fail_closed() {
        let q = Question {
            markdown: "x".into(),
            actions: vec![
                Action::new("allow_once", "x", "A"),
                Action::new("deny", "x", "B"),
            ],
            note: None,
        };
        assert_eq!(q.parse("x"), None, "ambiguous canonical key must deny");
    }

    #[test]
    fn key_value_collision_between_actions_fails_closed() {
        // "deny" is action A's key and action B's wire value → ambiguous.
        let q = Question {
            markdown: "x".into(),
            actions: vec![
                Action::new("allow_once", "deny", "A"),
                Action::new("deny", "n", "B"),
            ],
            note: None,
        };
        assert_eq!(q.parse("deny"), None, "key-vs-value collision must deny");
    }

    #[test]
    fn duplicate_aliases_fail_closed() {
        let q = Question {
            markdown: "x".into(),
            actions: vec![
                Action::new("allow_once", "y", "A").with_aliases(["z"]),
                Action::new("deny", "n", "B").with_aliases(["z"]),
            ],
            note: None,
        };
        assert_eq!(q.parse("z"), None, "ambiguous alias must deny");
    }

    #[test]
    fn a_canonical_match_wins_over_an_unrelated_alias() {
        let q = Question {
            markdown: "x".into(),
            actions: vec![
                Action::new("allow_once", "y", "A"),
                Action::new("deny", "n", "B").with_aliases(["y"]),
            ],
            note: None,
        };
        assert_eq!(
            q.parse("y"),
            Some("allow_once"),
            "canonical y wins over deny's alias"
        );
    }
}

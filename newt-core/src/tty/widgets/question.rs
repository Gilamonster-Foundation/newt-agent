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
    /// Aliases are matched exactly by [`Question::parse`] and never rendered —
    /// rendering left this type in C0a (#1856); the plain projection is
    /// `newt_core::markup::plain::render`, and its
    /// `tests::aliases_are_never_rendered` is BHV-PROMPT-005's ref.
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

#[cfg(test)]
mod tests {
    use super::*;

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
}

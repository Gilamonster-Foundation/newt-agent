//! **newt's meta prefix** — `ctrl+space` by default, tmux's and herdr's
//! `ctrl+b` semantics on a chord that collides with nothing in newt.
//!
//! Pure: a sequencer over `newtui`'s own [`Key`] vocabulary and a binding
//! table read from `assets/prefix_keys.toml`. It decodes no terminal bytes and
//! owns no terminal, so it can graduate into `newtui` unchanged; the chord
//! and the table are the host's (config and data here).

use std::sync::LazyLock;

use newtui::Key;

/// newt's own operations, reachable as `prefix` then one key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MetaAction {
    Zoom,
    Resize,
    Redraw,
    Help,
}

impl MetaAction {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "zoom" => Some(Self::Zoom),
            "resize" => Some(Self::Resize),
            "redraw" => Some(Self::Redraw),
            "help" => Some(Self::Help),
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Zoom => "zoom",
            Self::Resize => "resize",
            Self::Redraw => "redraw",
            Self::Help => "help",
        }
    }
}

/// The shipped default chord.
pub(crate) const DEFAULT_PREFIX: &str = "ctrl+space";

/// Parse a chord: `ctrl+<letter>` or `ctrl+space`. Only control chords: a
/// printable prefix would eat a character the operator meant to type.
pub(crate) fn parse_chord(text: &str) -> Option<Key> {
    let rest = text.trim().to_ascii_lowercase();
    let rest = rest
        .strip_prefix("ctrl+")
        .or_else(|| rest.strip_prefix("ctrl-"))?;
    match rest {
        "space" => Some(Key::Ctrl(' ')),
        one if one.len() == 1 && one.as_bytes()[0].is_ascii_lowercase() => {
            Some(Key::Ctrl(char::from(one.as_bytes()[0])))
        }
        _ => None,
    }
}

/// A chord as the operator writes it, e.g. `ctrl+space`.
pub(crate) fn chord_label(key: Key) -> String {
    match key {
        Key::Ctrl(' ') => "ctrl+space".to_string(),
        Key::Ctrl(c) => format!("ctrl+{c}"),
        other => format!("{other:?}"),
    }
}

/// The binding table: one key after the prefix names one operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Bindings(Vec<(char, MetaAction)>);

impl Bindings {
    pub(crate) fn from_toml(text: &str) -> Result<Self, String> {
        #[derive(serde::Deserialize)]
        struct File {
            bindings: std::collections::BTreeMap<String, String>,
        }
        let file: File = toml::from_str(text).map_err(|e| e.to_string())?;
        file.bindings
            .into_iter()
            .map(|(key, action)| {
                let mut chars = key.chars();
                let (Some(c), None) = (chars.next(), chars.next()) else {
                    return Err(format!("binding `{key}` is not one key"));
                };
                MetaAction::parse(&action)
                    .map(|action| (c, action))
                    .ok_or_else(|| format!("binding `{key}`: unknown operation `{action}`"))
            })
            .collect::<Result<_, _>>()
            .map(Self)
    }

    fn action(&self, c: char) -> Option<MetaAction> {
        self.0
            .iter()
            .find(|(k, _)| *k == c)
            .map(|(_, action)| *action)
    }

    /// `z zoom · r resize · …`, for the hint shown while the prefix is armed.
    pub(crate) fn describe(&self) -> String {
        self.0
            .iter()
            .map(|(k, action)| format!("{k} {}", action.name()))
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

/// The shipped table. `expect` is honest: the file is compiled in, and
/// `the_shipped_table_parses_and_binds_every_operation` proves this string.
pub(crate) static BINDINGS: LazyLock<Bindings> = LazyLock::new(|| {
    Bindings::from_toml(include_str!("../assets/prefix_keys.toml"))
        .expect("assets/prefix_keys.toml is compiled in and checked by this module's tests")
});

/// What one key press means to the sequencer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// Not ours: hand the key to whatever has focus.
    Pass(Key),
    /// The prefix: the next key is a meta key.
    Armed,
    /// A bound key after the prefix.
    Act(MetaAction),
    /// An unbound key after the prefix: swallowed, and the prefix disarms.
    Cancelled,
}

/// Idle ⇄ armed. Pressing the prefix twice passes the prefix chord through,
/// so an application that wants `ctrl+space` itself can still get it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sequencer {
    prefix: Key,
    armed: bool,
}

impl Sequencer {
    pub(crate) fn new(prefix: Key) -> Self {
        Self {
            prefix,
            armed: false,
        }
    }

    pub(crate) fn armed(self) -> bool {
        self.armed
    }

    pub(crate) fn feed(&mut self, key: Key, bindings: &Bindings) -> Step {
        if !self.armed {
            if key == self.prefix {
                self.armed = true;
                return Step::Armed;
            }
            return Step::Pass(key);
        }
        self.armed = false;
        if key == self.prefix {
            return Step::Pass(key);
        }
        match key {
            Key::Char(c) => bindings.action(c).map_or(Step::Cancelled, Step::Act),
            _ => Step::Cancelled,
        }
    }
}

/// The configured prefix, parsed from the one setting resolver
/// (`settings_form::prefix_setting`). An unparseable value falls back to the
/// default rather than leaving the operator with no prefix at all.
pub(crate) fn configured() -> Key {
    parse_chord(&crate::settings_form::prefix_setting())
        .unwrap_or_else(|| parse_chord(DEFAULT_PREFIX).expect("the default chord parses"))
}

/// The prefix in effect, resolved once and cached: the composer asks on every
/// key, and a config read per keystroke is not free. `/settings prefix`
/// clears it ([`invalidate`]) so a change applies to the very next key.
static CURRENT: std::sync::RwLock<Option<Key>> = std::sync::RwLock::new(None);

pub(crate) fn current() -> Key {
    if let Some(key) = CURRENT.read().ok().and_then(|current| *current) {
        return key;
    }
    let key = configured();
    if let Ok(mut current) = CURRENT.write() {
        *current = Some(key);
    }
    key
}

pub(crate) fn invalidate() {
    if let Ok(mut current) = CURRENT.write() {
        *current = None;
    }
}

/// `ctrl+space`, or whatever the prefix is now — for hint lines.
pub(crate) fn active_label() -> String {
    chord_label(current())
}

#[cfg(test)]
#[path = "prefix_tests.rs"]
mod tests;

//! Session tabs — the pure, TTY-free model (#1669 PR-A, ADR
//! `docs/decisions/session_tabs.md`).
//!
//! # What a tab is
//!
//! One tab is one live Newt **session**, holding one **conversation**. Those are
//! two different identities and this module keeps them apart, per the #1662
//! separation:
//!
//! | | stable for | changes when |
//! |---|---|---|
//! | [`SessionId`] | the tab's whole life | never — minted once when the tab opens |
//! | conversation id | until the tab resumes another conversation | `/resume` inside the tab |
//!
//! So `/resume` inside a tab replaces the conversation and **keeps** the
//! session id; opening a tab mints a new session id; switching tabs mints
//! nothing. The ADR describes tabs as held conversations and identifies them by
//! conversation id — that predates the #1662 split. Identity here is the
//! session id, because a tab that swaps its conversation is still the same tab,
//! and a lifecycle event attributed to "the conversation" would follow the
//! wrong thing the moment a tab resumes.
//!
//! # Ownership, and why an ambient owner is still right
//!
//! N tabs coexist, but the ADR preserves a **synchronous single-turn REPL**:
//! there is no legal tab switch while a model turn is in flight. So exactly one
//! session owns the REPL at any instant, and "the active tab's session" is a
//! well-defined process-wide fact. That is what makes
//! `newt_core::lifecycle::set_active_session` sound here — the deep ambient
//! emitters (the TTY arbiter's Blocked/Unblocked, tool activity) have no
//! session handle to receive, and under the single-turn invariant they cannot
//! be running for an inactive tab.
//!
//! **This module does not touch that global.** It is pure state; the caller
//! performs the ownership handoff at the activation seam, so the rule stays
//! testable here without a process global in the way. See
//! [`TabSet::activate`], which reports the handoff rather than performing it.
//!
//! # What lives here vs. on the conversation row
//!
//! Durable per-tab state lives on the conversation row (turns, persona,
//! scratchpad, plan, receipts, the #1668 `PosturePin`). What this module holds
//! is the **sidecar** — session-shaped state the row does not persist — plus
//! the unsubmitted input. Both are in-memory and lost on crash by design.

use newt_core::lifecycle::SessionId;
use newt_core::prompt::TurnPromptContext;

/// Session-shaped state that the conversation row does not persist, stashed
/// while a tab is inactive.
///
/// `interrupted_objective` is the load-bearing one: it is set when a turn is
/// interrupted and consumed to upgrade a later bare "continue". It is NOT
/// restored by the conversation-restore path, so without stashing it per tab,
/// tab A's interrupted objective would upgrade tab B's "continue" — one tab
/// silently finishing another tab's work.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TabSidecar {
    /// Turns taken in this tab's current conversation; feeds close-time
    /// extraction.
    pub turns_this_conversation: u32,
    /// The last `/resume` listing shown in this tab, so its ordinals stay
    /// meaningful after a switch away and back.
    pub last_resume_listing: Vec<String>,
    /// The roadmap this tab is walking, if any.
    pub active_roadmap_id: Option<String>,
    /// An interrupted turn's objective, awaiting a "continue" IN THIS TAB.
    pub interrupted_objective: Option<TurnPromptContext>,
}

/// One tab: one session, holding one conversation.
#[derive(Debug, Clone)]
pub struct TabState {
    /// Stable for this tab's entire life. Never reassigned — not by a
    /// conversation switch, not by a tab move, not by another tab closing.
    session_id: SessionId,
    /// The conversation this tab currently holds. Replaceable via `/resume`.
    conversation_id: String,
    /// Session-shaped state the row does not persist.
    pub sidecar: TabSidecar,
    /// Unsubmitted prompt text, restored when this tab is reactivated.
    pub input_stash: String,
}

impl TabState {
    /// This tab's stable session identity.
    #[must_use]
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// The conversation this tab currently holds.
    #[must_use]
    pub fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    /// Point this tab at a different conversation (the `/resume`-in-tab case).
    ///
    /// The session id is deliberately untouched: resuming another conversation
    /// does not make this a different session, and anything already attributed
    /// to this tab must stay attributed to it.
    pub fn hold_conversation(&mut self, conversation_id: impl Into<String>) {
        self.conversation_id = conversation_id.into();
    }
}

/// Why an operation on the tab set was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabError {
    /// The index named no open tab. Carries how many tabs are open, so the
    /// caller can say `1..=n` rather than "invalid".
    OutOfRange { open: usize },
    /// Closing the only tab is refused — `:q` is how you leave newt, and a
    /// zero-tab state has no meaning (there would be no session to own the
    /// REPL).
    LastTab,
}

/// What the caller must do to make an activation truthful.
///
/// Returned rather than performed, so this module stays pure and the rule is
/// testable without a process global. The caller applies it at the seam by
/// declaring `to` the ambient lifecycle owner.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "an activation that never applies its handoff leaves lifecycle \
              events attributed to the tab the operator just left"]
pub struct OwnerHandoff {
    /// The session that owned the REPL before this call, if it changed.
    pub from: Option<SessionId>,
    /// The session that owns the REPL now — the activated tab's.
    pub to: SessionId,
}

impl OwnerHandoff {
    /// Declare the activated tab's session the ambient lifecycle owner.
    ///
    /// The ONE place this module touches process-global state, kept here so
    /// every front door goes through the same seam and `#[must_use]` on the
    /// handoff makes forgetting it a warning rather than a silent
    /// misattribution.
    ///
    /// Sound because the ADR preserves a synchronous single-turn REPL: no tab
    /// switch is legal while a model turn is in flight, so at the instant this
    /// runs there is no other tab's turn whose ambient emitters could be
    /// misdirected. If that invariant is ever broken — parallel in-flight turns
    /// across tabs — this becomes wrong and the deep emitters
    /// (`tty::arbiter::notify_prompt_observer`, `agentic::announce_tool_activity`)
    /// need real per-session handles instead.
    pub fn apply(&self) {
        newt_core::lifecycle::set_active_session(&self.to);
        // The Herdr projection rule for #1669: **a pane reports the ACTIVE
        // Newt tab.** Announcing the activated session re-anchors the pane, so
        // it converges on the tab the operator is actually looking at rather
        // than keeping the identity of whichever tab happened to start first.
        //
        // The adapter already treats `SessionStarted` as adopt-or-re-anchor and
        // converges desired-vs-delivered, so a burst of fast switches coalesces
        // to the newest instead of walking Herdr through every intermediate
        // tab — no transport change is needed for this.
        //
        // Inactive tabs keep their own `SessionId` and their own lifecycle
        // identity; they simply do not claim the pane. Nothing here pretends
        // the process and the tab are the same identity.
        if self.from.as_ref() != Some(&self.to) {
            newt_core::lifecycle::emit_for(
                Some(self.to.to_string()),
                newt_core::lifecycle::LifecycleEvent::SessionStarted {
                    session_id: self.to.to_string(),
                },
            );
        }
    }
}

/// The open tabs and which one is active.
///
/// Invariants, upheld by construction: there is always at least one tab, and
/// `active` always indexes an open tab.
#[derive(Debug, Clone)]
pub struct TabSet {
    tabs: Vec<TabState>,
    active: usize,
}

// A `TabSet` is never empty by construction, so `is_empty` would be a
// function that can only return `false` — noise rather than API.
#[allow(clippy::len_without_is_empty)]
impl TabSet {
    /// Start a tab set with the session's first tab.
    #[must_use]
    pub fn new(session_id: SessionId, conversation_id: impl Into<String>) -> Self {
        Self {
            tabs: vec![TabState {
                session_id,
                conversation_id: conversation_id.into(),
                sidecar: TabSidecar::default(),
                input_stash: String::new(),
            }],
            active: 0,
        }
    }

    /// How many tabs are open. Never zero.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tabs.len()
    }

    /// The active tab's index (0-based; the operator sees `idx + 1`).
    #[must_use]
    pub fn active_index(&self) -> usize {
        self.active
    }

    /// The active tab.
    #[must_use]
    pub fn active(&self) -> &TabState {
        &self.tabs[self.active]
    }

    /// The active tab, mutably — where deactivation stashes live state.
    pub fn active_mut(&mut self) -> &mut TabState {
        &mut self.tabs[self.active]
    }

    /// Every open tab, in bar order.
    #[must_use]
    pub fn tabs(&self) -> &[TabState] {
        &self.tabs
    }

    /// The tab at `index`, if it is open.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&TabState> {
        self.tabs.get(index)
    }

    /// The tab holding `conversation_id`, if any is.
    ///
    /// This is what turns `/resume <target>` into an **activation** when the
    /// target is already open in another tab, rather than claiming a
    /// conversation twice in one process.
    #[must_use]
    pub fn find_by_conversation(&self, conversation_id: &str) -> Option<usize> {
        self.tabs
            .iter()
            .position(|tab| tab.conversation_id == conversation_id)
    }

    /// Open a new tab holding `conversation_id` and make it active.
    ///
    /// Inserted directly after the active tab, as `:tabnew` does in vim, and
    /// the caller mints `session_id` — one stable identity for the life of this
    /// tab. Returns the new tab's index and the ownership handoff.
    pub fn open(
        &mut self,
        session_id: SessionId,
        conversation_id: impl Into<String>,
    ) -> (usize, OwnerHandoff) {
        let from = self.active().session_id.clone();
        let at = self.active + 1;
        self.tabs.insert(
            at,
            TabState {
                session_id: session_id.clone(),
                conversation_id: conversation_id.into(),
                sidecar: TabSidecar::default(),
                input_stash: String::new(),
            },
        );
        self.active = at;
        (
            at,
            OwnerHandoff {
                from: Some(from),
                to: session_id,
            },
        )
    }

    /// Make the tab at `index` active.
    ///
    /// Pure: it moves the active pointer and reports the handoff the caller
    /// must apply. Activating the already-active tab is allowed and reports a
    /// handoff with `from == Some(to)` — callers that want to skip the work
    /// should compare, rather than this refusing, because `/tab 1` while on tab
    /// 1 is not an error.
    pub fn activate(&mut self, index: usize) -> Result<OwnerHandoff, TabError> {
        if index >= self.tabs.len() {
            return Err(TabError::OutOfRange {
                open: self.tabs.len(),
            });
        }
        let from = self.active().session_id.clone();
        self.active = index;
        Ok(OwnerHandoff {
            from: Some(from),
            to: self.tabs[index].session_id.clone(),
        })
    }

    /// Close the tab at `index`.
    ///
    /// Refuses the last tab. Closing the ACTIVE tab activates a neighbor first
    /// — the returned [`Closed::handoff`] is `Some`, and the caller must apply
    /// it BEFORE releasing the closed tab's claim, so no window exists in which
    /// the process has no owner or still names a tab that is gone.
    ///
    /// Numbering is positional, so remaining tabs renumber on close exactly as
    /// vim does. `<n>gt` is positional-absolute, so a stored monotonic number
    /// would let the bar disagree with the count prefix.
    pub fn close(&mut self, index: usize) -> Result<Closed, TabError> {
        if index >= self.tabs.len() {
            return Err(TabError::OutOfRange {
                open: self.tabs.len(),
            });
        }
        if self.tabs.len() == 1 {
            return Err(TabError::LastTab);
        }
        let was_active = index == self.active;
        let removed = self.tabs.remove(index);
        // Fix the active index BEFORE reporting, so the handoff names a tab
        // that is actually open.
        if self.active > index || self.active == self.tabs.len() {
            self.active -= 1;
        }
        let handoff = was_active.then(|| OwnerHandoff {
            from: Some(removed.session_id.clone()),
            to: self.tabs[self.active].session_id.clone(),
        });
        Ok(Closed {
            tab: removed,
            handoff,
        })
    }

    /// Move the tab at `from` to index `to`, preserving which tab is active.
    ///
    /// Identity-preserving: the active tab is tracked by SESSION id across the
    /// move, not by position, so reordering never silently activates a
    /// different tab. Session id rather than conversation id because a tab that
    /// resumed another conversation is still the same tab.
    pub fn move_tab(&mut self, from: usize, to: usize) -> Result<(), TabError> {
        let open = self.tabs.len();
        if from >= open || to >= open {
            return Err(TabError::OutOfRange { open });
        }
        let active_session = self.active().session_id.clone();
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        self.active = self
            .tabs
            .iter()
            .position(|t| t.session_id == active_session)
            .unwrap_or(self.active.min(open - 1));
        Ok(())
    }

    /// Every conversation this session currently claims, in bar order.
    ///
    /// The exit path releases exactly these. Returned rather than released here
    /// so the model stays free of the store.
    #[must_use]
    pub fn claimed_conversations(&self) -> Vec<&str> {
        self.tabs
            .iter()
            .map(|tab| tab.conversation_id.as_str())
            .collect()
    }
}

/// The outcome of closing a tab.
#[derive(Debug, Clone)]
pub struct Closed {
    /// The tab that left the bar. Its conversation stays open and
    /// `/resume`-able — close releases the claim, it does not end the
    /// conversation.
    pub tab: TabState,
    /// Present only when the CLOSED tab was active: the neighbor that must be
    /// made the lifecycle owner before the closed tab's claim is released.
    pub handoff: Option<OwnerHandoff>,
}

/// What the operator asked the tab engine to do.
///
/// One enum, four front doors (the `/tab` slash family, the vi ex-line and
/// `gt`/`gT` in PR-C, and the bar menu in PR-D), so no front door can drift
/// into its own semantics. `Goto` is **1-based positional** because that is
/// what the operator types and what `<n>gt` means; everything inside
/// [`TabSet`] is 0-based, and the conversion happens once, here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabAction {
    /// Show the open tabs. Slash-only — also THE piped-visible view of tab
    /// state, since there are no pixels in this slice.
    List,
    /// Open a fresh conversation in a new tab.
    New,
    /// The next tab, wrapping.
    Next,
    /// `n` tabs back, wrapping.
    Prev(usize),
    /// Activate tab `n` (1-based).
    Goto(usize),
    /// Close tab `n`, or the active tab when `None`.
    Close(Option<usize>),
    /// Reorder, preserving which tab is active (1-based).
    Move { from: usize, to: usize },
    /// Retitle tab `n`'s conversation, or the active tab's when `None`.
    /// Slash-only; targets a non-active tab, which is why the rename helper is
    /// extracted rather than inlined in the `/rename` arm.
    Rename { index: Option<usize>, title: String },
}

/// Parse the argument tail of `/tab …`.
///
/// `Err` carries the operator-facing message, so the caller prints it verbatim
/// rather than inventing a second vocabulary for the same mistakes.
pub fn parse_tab_command(args: &str) -> Result<TabAction, String> {
    let args = args.trim();
    if args.is_empty() {
        return Ok(TabAction::List);
    }
    let mut parts = args.split_whitespace();
    let head = parts.next().unwrap_or_default();
    let rest: Vec<&str> = parts.collect();

    // A bare number is the common case and reads as `/tab 2`.
    if let Ok(n) = head.parse::<usize>() {
        return if rest.is_empty() {
            positional(n).map(TabAction::Goto)
        } else {
            Err(format!("usage: /tab {n} (no extra arguments)"))
        };
    }

    match head {
        "new" => Ok(TabAction::New),
        "next" => Ok(TabAction::Next),
        "prev" => Ok(TabAction::Prev(1)),
        "close" => match rest.as_slice() {
            [] => Ok(TabAction::Close(None)),
            [n] => positional(parse_index(n)?).map(|i| TabAction::Close(Some(i))),
            _ => Err("usage: /tab close [n]".to_string()),
        },
        "move" => match rest.as_slice() {
            [from, to] => Ok(TabAction::Move {
                from: positional(parse_index(from)?)?,
                to: positional(parse_index(to)?)?,
            }),
            _ => Err("usage: /tab move <from> <to>".to_string()),
        },
        "rename" => {
            // `/tab rename <title…>` retitles the active tab; a leading number
            // targets another one. A title that IS a number therefore needs the
            // explicit form — noted in the usage line rather than guessed at.
            if rest.is_empty() {
                return Err("usage: /tab rename [n] <title>".to_string());
            }
            match rest[0].parse::<usize>() {
                Ok(n) if rest.len() > 1 => Ok(TabAction::Rename {
                    index: Some(positional(n)?),
                    title: rest[1..].join(" "),
                }),
                _ => Ok(TabAction::Rename {
                    index: None,
                    title: rest.join(" "),
                }),
            }
        }
        other => Err(format!(
            "unknown: /tab {other} — try /tab, /tab new, /tab <n>, \
             /tab next|prev, /tab close [n], /tab move <a> <b>, /tab rename [n] <title>"
        )),
    }
}

fn parse_index(raw: &str) -> Result<usize, String> {
    raw.parse::<usize>()
        .map_err(|_| format!("'{raw}' is not a tab number"))
}

/// 1-based operator input to a 0-based index. Tab 0 does not exist, and
/// silently treating it as tab 1 would make `<n>gt` disagree with the bar.
fn positional(n: usize) -> Result<usize, String> {
    n.checked_sub(1)
        .ok_or_else(|| "tabs are numbered from 1".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(n: u8) -> SessionId {
        SessionId::from_issued(format!("session-{n}"))
    }

    fn three_tabs() -> TabSet {
        let mut set = TabSet::new(sid(1), "conv-1");
        let _ = set.open(sid(2), "conv-2");
        let _ = set.open(sid(3), "conv-3");
        let _ = set.activate(0).unwrap();
        set
    }

    // ── the /tab text engine ──────────────────────────────────────────────

    #[test]
    fn bare_tab_lists() {
        assert_eq!(parse_tab_command(""), Ok(TabAction::List));
        assert_eq!(parse_tab_command("   "), Ok(TabAction::List));
    }

    #[test]
    fn a_bare_number_activates_that_tab_one_based() {
        assert_eq!(parse_tab_command("2"), Ok(TabAction::Goto(1)));
        assert_eq!(parse_tab_command(" 1 "), Ok(TabAction::Goto(0)));
    }

    #[test]
    fn tab_zero_is_refused_rather_than_silently_meaning_tab_one() {
        // `<n>gt` is positional-absolute; quietly mapping 0→1 would make the
        // count prefix disagree with what the operator counted.
        assert!(parse_tab_command("0").is_err());
        assert!(parse_tab_command("close 0").is_err());
        assert!(parse_tab_command("move 0 1").is_err());
    }

    #[test]
    fn the_verb_forms_parse() {
        assert_eq!(parse_tab_command("new"), Ok(TabAction::New));
        assert_eq!(parse_tab_command("next"), Ok(TabAction::Next));
        assert_eq!(parse_tab_command("prev"), Ok(TabAction::Prev(1)));
        assert_eq!(parse_tab_command("close"), Ok(TabAction::Close(None)));
        assert_eq!(parse_tab_command("close 3"), Ok(TabAction::Close(Some(2))));
        assert_eq!(
            parse_tab_command("move 1 3"),
            Ok(TabAction::Move { from: 0, to: 2 })
        );
    }

    #[test]
    fn rename_targets_the_active_tab_unless_a_number_leads() {
        assert_eq!(
            parse_tab_command("rename ship the thing"),
            Ok(TabAction::Rename {
                index: None,
                title: "ship the thing".to_string()
            })
        );
        assert_eq!(
            parse_tab_command("rename 2 ship the thing"),
            Ok(TabAction::Rename {
                index: Some(1),
                title: "ship the thing".to_string()
            })
        );
        // A title that is ONLY a number renames the active tab — there is no
        // second word to be the title, so it cannot be the index form.
        assert_eq!(
            parse_tab_command("rename 42"),
            Ok(TabAction::Rename {
                index: None,
                title: "42".to_string()
            })
        );
        assert!(parse_tab_command("rename").is_err());
    }

    #[test]
    fn an_unknown_verb_names_the_whole_family_rather_than_just_refusing() {
        let err = parse_tab_command("frobnicate").unwrap_err();
        assert!(err.contains("/tab new"), "{err}");
        assert!(err.contains("/tab move"), "{err}");
    }

    #[test]
    fn arity_mistakes_are_refused_with_usage() {
        assert!(parse_tab_command("2 3").is_err());
        assert!(parse_tab_command("move 1").is_err());
        assert!(parse_tab_command("close 1 2").is_err());
        assert!(parse_tab_command("move a b").is_err());
    }

    // ── the tab set ───────────────────────────────────────────────────────

    #[test]
    fn each_tab_gets_a_distinct_stable_session_id() {
        let set = three_tabs();
        let ids: std::collections::BTreeSet<_> =
            set.tabs().iter().map(|t| t.session_id().clone()).collect();
        assert_eq!(ids.len(), 3, "one identity per tab, all distinct");
    }

    #[test]
    fn opening_a_tab_inserts_after_the_active_one_and_activates_it() {
        let mut set = TabSet::new(sid(1), "conv-1");
        let (idx, handoff) = set.open(sid(2), "conv-2");
        assert_eq!(idx, 1);
        assert_eq!(set.active_index(), 1);
        assert_eq!(handoff.from, Some(sid(1)));
        assert_eq!(handoff.to, sid(2));

        // A third opened from tab 1 lands between, vim-style.
        let _ = set.activate(0).unwrap();
        let (idx, _) = set.open(sid(3), "conv-3");
        assert_eq!(idx, 1, "inserted directly after the active tab");
        assert_eq!(
            set.tabs()
                .iter()
                .map(|t| t.conversation_id())
                .collect::<Vec<_>>(),
            vec!["conv-1", "conv-3", "conv-2"]
        );
    }

    #[test]
    fn switching_tabs_does_not_mint_a_new_session_id() {
        let mut set = three_tabs();
        let before: Vec<_> = set.tabs().iter().map(|t| t.session_id().clone()).collect();
        let _ = set.activate(2).unwrap();
        let _ = set.activate(1).unwrap();
        let _ = set.activate(0).unwrap();
        let after: Vec<_> = set.tabs().iter().map(|t| t.session_id().clone()).collect();
        assert_eq!(before, after, "switching is not creating");
    }

    #[test]
    fn a_to_b_then_back_restores_a_s_same_session_id() {
        let mut set = three_tabs();
        let a = set.active().session_id().clone();
        let to_b = set.activate(1).unwrap();
        assert_eq!(to_b.from, Some(a.clone()));
        let back = set.activate(0).unwrap();
        assert_eq!(
            back.to, a,
            "A→B→A returns to the SAME session, not a new one"
        );
    }

    #[test]
    fn a_conversation_switch_inside_a_tab_keeps_the_tab_s_session_id() {
        let mut set = TabSet::new(sid(1), "conv-1");
        let before = set.active().session_id().clone();
        set.active_mut().hold_conversation("conv-99");
        assert_eq!(set.active().conversation_id(), "conv-99");
        assert_eq!(
            *set.active().session_id(),
            before,
            "resuming another conversation does not make this a different session"
        );
    }

    #[test]
    fn activation_is_bounds_checked() {
        let mut set = three_tabs();
        assert_eq!(
            set.activate(3),
            Err(TabError::OutOfRange { open: 3 }),
            "the error carries the open count so the caller can say 1..=3"
        );
        assert_eq!(set.active_index(), 0, "a refused switch moves nothing");
    }

    #[test]
    fn activating_the_active_tab_is_allowed() {
        let mut set = three_tabs();
        let handoff = set
            .activate(0)
            .expect("/tab 1 while on tab 1 is not an error");
        assert_eq!(handoff.from, Some(handoff.to.clone()));
    }

    #[test]
    fn closing_the_last_tab_is_refused() {
        let mut set = TabSet::new(sid(1), "conv-1");
        assert_eq!(set.close(0).unwrap_err(), TabError::LastTab);
        assert_eq!(set.len(), 1, "the refusal changed nothing");
    }

    #[test]
    fn closing_the_active_tab_hands_off_to_a_neighbor_before_release() {
        let mut set = three_tabs();
        let _ = set.activate(1).unwrap(); // active = conv-2
        let closed = set.close(1).unwrap();
        assert_eq!(closed.tab.conversation_id(), "conv-2");
        let handoff = closed
            .handoff
            .expect("closing the ACTIVE tab must hand ownership off");
        assert_eq!(handoff.from, Some(sid(2)));
        assert_eq!(
            handoff.to,
            *set.active().session_id(),
            "the handoff names the tab that is now active — and it is still open"
        );
        assert!(
            !set.tabs().iter().any(|t| *t.session_id() == sid(2)),
            "the closed tab is gone from the set"
        );
    }

    #[test]
    fn closing_an_inactive_tab_does_not_disturb_the_active_one() {
        let mut set = three_tabs();
        let _ = set.activate(2).unwrap();
        let active = set.active().session_id().clone();
        let closed = set.close(0).unwrap();
        assert!(
            closed.handoff.is_none(),
            "no ownership change — the active tab did not move"
        );
        assert_eq!(
            *set.active().session_id(),
            active,
            "closing another tab cannot end or re-anchor this one"
        );
        assert_eq!(set.active_index(), 1, "the index shifted with the removal");
    }

    #[test]
    fn closing_the_last_positional_tab_while_active_steps_back() {
        let mut set = three_tabs();
        let _ = set.activate(2).unwrap();
        let closed = set.close(2).unwrap();
        assert!(closed.handoff.is_some());
        assert_eq!(set.active_index(), 1, "active never dangles past the end");
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn tabs_renumber_positionally_on_close() {
        let mut set = three_tabs();
        set.close(0).unwrap();
        assert_eq!(
            set.tabs()
                .iter()
                .map(|t| t.conversation_id())
                .collect::<Vec<_>>(),
            vec!["conv-2", "conv-3"],
            "what was tab 2 is now tab 1 — <n>gt is positional-absolute"
        );
    }

    #[test]
    fn moving_a_tab_preserves_which_tab_is_active() {
        let mut set = three_tabs();
        let _ = set.activate(1).unwrap();
        let active = set.active().session_id().clone();
        set.move_tab(0, 2).unwrap();
        assert_eq!(
            *set.active().session_id(),
            active,
            "reordering never silently activates a different tab"
        );
    }

    #[test]
    fn moving_tracks_identity_by_session_not_conversation() {
        // A tab that resumed another conversation is still the same tab. If the
        // move tracked conversation id, a tab that had swapped conversations
        // would be un-findable and the active pointer would fall back.
        let mut set = three_tabs();
        let _ = set.activate(1).unwrap();
        set.active_mut().hold_conversation("conv-swapped");
        let active = set.active().session_id().clone();
        set.move_tab(1, 0).unwrap();
        assert_eq!(*set.active().session_id(), active);
        assert_eq!(set.active_index(), 0);
    }

    #[test]
    fn move_is_bounds_checked() {
        let mut set = three_tabs();
        assert_eq!(set.move_tab(0, 9), Err(TabError::OutOfRange { open: 3 }));
        assert_eq!(set.move_tab(9, 0), Err(TabError::OutOfRange { open: 3 }));
    }

    #[test]
    fn a_conversation_open_in_a_tab_is_found_so_resume_becomes_activation() {
        let set = three_tabs();
        assert_eq!(set.find_by_conversation("conv-2"), Some(1));
        assert_eq!(set.find_by_conversation("conv-absent"), None);
    }

    #[test]
    fn every_open_tab_s_conversation_is_reported_for_release() {
        let set = three_tabs();
        assert_eq!(
            set.claimed_conversations(),
            vec!["conv-1", "conv-2", "conv-3"],
            "exit releases exactly the claims the tabs hold"
        );
    }

    #[test]
    fn the_sidecar_and_input_stash_are_per_tab() {
        // The interrupted objective is the load-bearing one: without per-tab
        // stashing, tab A's objective upgrades tab B's bare "continue".
        let mut set = three_tabs();
        set.active_mut().input_stash = "half-typed A".into();
        set.active_mut().sidecar.turns_this_conversation = 7;
        set.active_mut().sidecar.active_roadmap_id = Some("road-a".into());

        let _ = set.activate(1).unwrap();
        assert_eq!(
            set.active().input_stash,
            "",
            "B starts with its own empty stash"
        );
        assert_eq!(set.active().sidecar, TabSidecar::default());
        set.active_mut().input_stash = "half-typed B".into();

        let _ = set.activate(0).unwrap();
        assert_eq!(set.active().input_stash, "half-typed A");
        assert_eq!(set.active().sidecar.turns_this_conversation, 7);
        assert_eq!(
            set.active().sidecar.active_roadmap_id.as_deref(),
            Some("road-a")
        );
    }

    #[test]
    fn activation_is_independent_of_where_it_was_reached_from() {
        // ADR normative property 1: activate(B) lands identical state whether
        // reached from A, from C, or cold.
        let snapshot = |set: &TabSet| {
            (
                set.active().session_id().clone(),
                set.active().conversation_id().to_string(),
                set.active().input_stash.clone(),
                set.active().sidecar.clone(),
            )
        };
        let mut from_a = three_tabs();
        let _ = from_a.activate(1).unwrap();

        let mut from_c = three_tabs();
        let _ = from_c.activate(2).unwrap();
        let _ = from_c.activate(1).unwrap();

        assert_eq!(
            snapshot(&from_a),
            snapshot(&from_c),
            "A→B and C→B land byte-identical tab-projected state"
        );
    }
}

/// #1669 PR-A — the lifecycle-ownership contract, against the REAL process
/// global rather than a stand-in.
///
/// [`tests`] above proves the pure model. These prove the half that can only be
/// observed through `newt_core::lifecycle`: that ownership follows the active
/// tab, that it never follows anything else, and that an inactive tab can
/// neither receive another tab's events nor end it.
#[cfg(test)]
mod ownership_tests {
    use super::*;
    use newt_core::lifecycle::{self, new_session_id, LifecycleEnvelope, LifecycleEvent};
    use std::sync::{Arc, Mutex};

    /// Collect every envelope, so a test can assert what a given session was
    /// attributed — including events it should NOT have received.
    fn collector() -> (lifecycle::Subscription, Arc<Mutex<Vec<LifecycleEnvelope>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let sub = lifecycle::subscribe(move |envelope| {
            sink.lock().unwrap().push(envelope.clone());
        });
        (sub, seen)
    }

    fn attributed_to(seen: &[LifecycleEnvelope], session: &SessionId) -> Vec<LifecycleEvent> {
        seen.iter()
            .filter(|e| e.session_id.as_deref() == Some(session.as_str()))
            .map(|e| e.event.clone())
            .collect()
    }

    #[test]
    fn a_to_b_moves_lifecycle_ownership_and_b_to_a_moves_it_back() {
        let _g = newt_core::test_guard::GlobalSettingsGuard::acquire();
        let a = new_session_id();
        let b = new_session_id();
        let mut set = TabSet::new(a.clone(), "conv-a");
        let (_i, open) = set.open(b.clone(), "conv-b");
        open.apply();
        assert_eq!(lifecycle::active_session().as_deref(), Some(b.as_str()));

        set.activate(0).unwrap().apply();
        assert_eq!(
            lifecycle::active_session().as_deref(),
            Some(a.as_str()),
            "A→B→A returns ownership to A"
        );
    }

    #[test]
    fn an_event_after_a_switch_is_attributed_to_the_newly_active_tab() {
        let _g = newt_core::test_guard::GlobalSettingsGuard::acquire();
        let (sub, seen) = collector();
        let a = new_session_id();
        let b = new_session_id();
        let mut set = TabSet::new(a.clone(), "conv-a");

        // Ownership starts on A; the ambient emitters attribute there.
        OwnerHandoff {
            from: None,
            to: a.clone(),
        }
        .apply();
        lifecycle::emit(LifecycleEvent::Thinking);

        let (_i, open) = set.open(b.clone(), "conv-b");
        open.apply();
        lifecycle::emit(LifecycleEvent::ToolActivity {
            tool: "run_command".into(),
        });

        set.activate(0).unwrap().apply();
        lifecycle::emit(LifecycleEvent::TurnCompleted);
        drop(sub);

        let seen = seen.lock().unwrap();
        let to_a = attributed_to(&seen, &a);
        let to_b = attributed_to(&seen, &b);
        assert!(
            to_a.contains(&LifecycleEvent::Thinking),
            "the pre-switch Thinking belongs to A: {to_a:?}"
        );
        assert!(
            to_b.contains(&LifecycleEvent::ToolActivity {
                tool: "run_command".into()
            }),
            "tool activity while B was active belongs to B, not A: {to_b:?}"
        );
        assert!(
            !to_a.contains(&LifecycleEvent::ToolActivity {
                tool: "run_command".into()
            }),
            "an INACTIVE tab must not receive the active tab's events: {to_a:?}"
        );
        assert!(
            to_a.contains(&LifecycleEvent::TurnCompleted),
            "after switching back, events belong to A again: {to_a:?}"
        );
        assert!(
            !to_b.contains(&LifecycleEvent::TurnCompleted),
            "and B, now inactive, receives nothing further: {to_b:?}"
        );
    }

    #[test]
    fn a_conversation_switch_inside_a_tab_does_not_change_lifecycle_ownership() {
        let _g = newt_core::test_guard::GlobalSettingsGuard::acquire();
        let a = new_session_id();
        let mut set = TabSet::new(a.clone(), "conv-a");
        OwnerHandoff {
            from: None,
            to: a.clone(),
        }
        .apply();

        // `/resume` inside the tab replaces the conversation.
        set.active_mut().hold_conversation("conv-other");
        assert_eq!(
            lifecycle::active_session().as_deref(),
            Some(a.as_str()),
            "resuming another conversation is not a session change"
        );
        assert_eq!(*set.active().session_id(), a);
    }

    #[test]
    fn closing_a_tab_cannot_end_or_re_anchor_another_live_tab() {
        let _g = newt_core::test_guard::GlobalSettingsGuard::acquire();
        let a = new_session_id();
        let b = new_session_id();
        let mut set = TabSet::new(a.clone(), "conv-a");
        let (_i, open) = set.open(b.clone(), "conv-b");
        open.apply();

        // Close the INACTIVE tab A while B is active.
        let closed = set.close(0).unwrap();
        assert!(
            closed.handoff.is_none(),
            "closing an inactive tab is not an ownership event"
        );
        assert_eq!(
            lifecycle::active_session().as_deref(),
            Some(b.as_str()),
            "B still owns the REPL — closing A cannot end or re-anchor it"
        );
        assert_eq!(*set.active().session_id(), b);
    }

    #[test]
    fn closing_the_active_tab_activates_the_neighbor_before_ownership_is_cleared() {
        let _g = newt_core::test_guard::GlobalSettingsGuard::acquire();
        let a = new_session_id();
        let b = new_session_id();
        let mut set = TabSet::new(a.clone(), "conv-a");
        let (_i, open) = set.open(b.clone(), "conv-b");
        open.apply();
        assert_eq!(lifecycle::active_session().as_deref(), Some(b.as_str()));

        // Close B, the ACTIVE tab.
        let closed = set.close(1).unwrap();
        let handoff = closed.handoff.expect("closing the active tab hands off");
        handoff.apply();
        assert_eq!(
            lifecycle::active_session().as_deref(),
            Some(a.as_str()),
            "the neighbor owns the REPL — there is never an unowned instant"
        );
        // The closed tab's identity is gone from the set, so nothing can be
        // attributed to it through the tab model afterwards.
        assert!(!set.tabs().iter().any(|t| *t.session_id() == b));
    }

    #[test]
    fn activation_announces_the_tab_so_a_herdr_pane_reports_the_active_one() {
        // The projection rule: a pane reports the ACTIVE Newt tab. Without the
        // announcement the pane would keep whichever tab started first, and an
        // operator switching tabs would watch another tab's state.
        let _g = newt_core::test_guard::GlobalSettingsGuard::acquire();
        let (sub, seen) = collector();
        let a = new_session_id();
        let b = new_session_id();
        let mut set = TabSet::new(a.clone(), "conv-a");
        let (_i, open) = set.open(b.clone(), "conv-b");
        open.apply();
        set.activate(0).unwrap().apply();
        drop(sub);

        let starts: Vec<String> = seen
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match &e.event {
                LifecycleEvent::SessionStarted { session_id } => Some(session_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            starts,
            vec![b.to_string(), a.to_string()],
            "each activation re-anchors the pane onto the tab now in front"
        );
    }

    #[test]
    fn re_activating_the_same_tab_does_not_re_announce() {
        // `/tab 1` while already on tab 1 should not churn the pane.
        let _g = newt_core::test_guard::GlobalSettingsGuard::acquire();
        let (sub, seen) = collector();
        let a = new_session_id();
        let mut set = TabSet::new(a.clone(), "conv-a");
        set.activate(0).unwrap().apply();
        drop(sub);
        let starts = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e.event, LifecycleEvent::SessionStarted { .. }))
            .count();
        assert_eq!(starts, 0, "a no-op activation announces nothing");
    }
}

/// #1669 PR-A — the single-turn invariant the whole ownership model rests on.
///
/// The ADR preserves a **synchronous single-turn REPL**: there is no legal tab
/// switch while a model turn is in flight. That is what makes one ambient owner
/// correct instead of per-session plumbing, so it deserves to be stated where
/// someone changing it will read it.
///
/// **It is enforced by the borrow checker, not by convention.** A tab switch
/// needs `TabSwitchCtx`, which holds `&mut` to the live session state a turn is
/// already using — `memory`, `system`, `active_persona`, the backend quintet.
/// While a turn holds those, no switch can be constructed; the code that would
/// violate the invariant does not compile. The `/tab` arm therefore runs only
/// from the command dispatch at the prompt, after the previous turn released
/// everything.
///
/// The runtime test below pins the observable consequence: across a turn's
/// whole emit sequence, ownership does not move.
#[cfg(test)]
mod single_turn_invariant {
    use super::*;
    use newt_core::lifecycle::{self, new_session_id, LifecycleEvent};

    #[test]
    fn there_is_no_supported_tab_switch_during_an_in_flight_turn() {
        let _g = newt_core::test_guard::GlobalSettingsGuard::acquire();
        let a = new_session_id();
        let b = new_session_id();
        let mut set = TabSet::new(a.clone(), "conv-a");
        let (_i, open) = set.open(b.clone(), "conv-b");
        open.apply();
        set.activate(0).unwrap().apply();

        // A turn, start to finish. Every one of these emitters is ambient —
        // none carries a session — so all of them resolve through the owner.
        for event in [
            LifecycleEvent::TurnStarted,
            LifecycleEvent::Thinking,
            LifecycleEvent::ToolActivity {
                tool: "run_command".into(),
            },
            LifecycleEvent::Blocked,
            LifecycleEvent::Unblocked,
            LifecycleEvent::TurnCompleted,
        ] {
            lifecycle::emit(event);
            assert_eq!(
                lifecycle::active_session().as_deref(),
                Some(a.as_str()),
                "ownership cannot move mid-turn — a switch would need &mut to \
                 state this turn already holds, so it cannot even be built"
            );
        }
        // And the tab set itself never moved.
        assert_eq!(set.active_index(), 0);
        assert_eq!(*set.active().session_id(), a);
    }
}

//! What a tab looks like once it has been projected for display.
//!
//! This module holds the *data* that crosses the session→terminal boundary,
//! and deliberately nothing else. The layout and rendering that consume it
//! (#1669 PR-B / roadmap 16.2) land separately and depend on this, not the
//! other way round: the surface protocol must not have to know how a bar is
//! drawn in order to carry one.
//!
//! Why a projection rather than the live `TabSet`: the tabs are session state,
//! the bar is terminal chrome, and after the execution relocation (#1718)
//! those live on different threads. Sending a snapshot keeps the terminal from
//! reaching into a session's mutable state to draw a frame — the same reason
//! `set_runtime_context` sends values rather than lending a handle.

/// One tab, projected for rendering.
///
/// Labels are carried, never referenced: they are recomputed by the session
/// each loop head from the conversation store, so a `/rename` shows up on the
/// next prompt and a title cannot go stale on the far side of the channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TabCell {
    /// 1-based, what the operator types and what `<n>gt` means.
    pub number: usize,
    /// Freshly computed title, or `#shortid`.
    pub label: String,
    pub active: bool,
    /// This tab's pin could not be established, so it is refusing turns.
    /// Carried because the bar is the only always-visible surface, so a
    /// degraded tab must be legible there and not only on the switch that
    /// produced it.
    pub degraded: bool,
    /// Work arrived for an inactive tab since it was last visited.
    pub pending: bool,
}

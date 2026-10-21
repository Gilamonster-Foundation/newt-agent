//! Replies: the Ack, Result and Verdict a task earns, sent through the bound
//! session and confirmed through the verified log.
//!
//! Newt signs nothing. A reply is one `flock_emit` tool call on the member's
//! own session (an `amesh mcp --flock-session` child, reached through any
//! [`ToolCaller`]); the session signs and the server's admission decides. The
//! call's answer is not trusted as proof: a reply counts only once the
//! verified log shows it ([`Replies::reconcile`]).
//!
//! A reply that was sent but not answered clearly is never sent again by
//! this module. Every send is journalled first (fsync'd), so a crash between
//! the send and its answer leaves an intent that restart resolves from the
//! log, not by emitting twice.

use agent_mesh_flock::{Body, Kind};
use agent_mesh_flock_server::Follower;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::task::{Evidence, TaskKind};

/// Which of a task's two replies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// After durable acceptance.
    Ack,
    /// After execution: a Result or a Verdict.
    Finish,
}

/// One reply, fully determined by the task's evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reply {
    /// The flock.
    pub flock: String,
    /// The brief or review being answered.
    pub event: String,
    /// Which reply.
    pub step: Step,
    /// `ack`, `result` or `verdict`.
    pub kind: Kind,
    /// The reference role (`of`, `brief`, `review`).
    pub role: String,
    /// A verdict's exact head.
    pub head: Option<String>,
    /// The `flock_emit` body.
    pub body: Value,
}

impl Reply {
    /// The Ack for a task, sent after it is durably accepted.
    #[must_use]
    pub fn ack(ev: &Evidence) -> Self {
        Self::of(ev, Step::Ack, Kind::Ack, "of", None, json!("ack"))
    }

    /// A brief's Result.
    ///
    /// # Errors
    /// The task is a review.
    pub fn result(ev: &Evidence, completed: bool) -> Result<Self, String> {
        if ev.kind != TaskKind::Brief {
            return Err("a result answers a brief".into());
        }
        let outcome = if completed { "completed" } else { "failed" };
        Ok(Self::of(
            ev,
            Step::Finish,
            Kind::Result,
            "brief",
            None,
            json!({"result": {"outcome": outcome}}),
        ))
    }

    /// A review's Verdict, on the exact head the review cites.
    ///
    /// # Errors
    /// The task is a brief, or cites no head.
    pub fn verdict(ev: &Evidence, approved: bool) -> Result<Self, String> {
        let (TaskKind::Review, Some(head)) = (ev.kind, &ev.head) else {
            return Err("a verdict answers a review that cites a head".into());
        };
        Ok(Self::of(
            ev,
            Step::Finish,
            Kind::Verdict,
            "review",
            Some(head.clone()),
            json!({"verdict": {"head": head, "approved": approved}}),
        ))
    }

    fn of(
        ev: &Evidence,
        step: Step,
        kind: Kind,
        role: &str,
        head: Option<String>,
        body: Value,
    ) -> Self {
        Self {
            flock: ev.flock.clone(),
            event: ev.event.clone(),
            step,
            kind,
            role: role.into(),
            head,
            body,
        }
    }

    /// The journal key: one reply per (flock, event, step).
    #[must_use]
    pub fn key(&self) -> String {
        format!("{}/{}/{:?}", self.flock, self.event, self.step)
    }

    fn args(&self) -> Value {
        json!({
            "kind": self.kind,
            "refs": { self.role.as_str(): self.event },
            "body": self.body,
        })
    }
}

/// What the tool said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolReply {
    /// The tool's text.
    Text(String),
    /// The tool reported an error (`isError`).
    Error(String),
}

/// The call got no answer: the request may or may not have been admitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lost(pub String);

impl ToolReply {
    /// Read an MCP `tools/call` result.
    ///
    /// # Errors
    /// The result has no text content: nothing can be said of the call.
    pub fn from_mcp(result: &Value) -> Result<Self, Lost> {
        let text = result
            .get("content")
            .and_then(Value::as_array)
            .and_then(|c| c.iter().find_map(|p| p.get("text").and_then(Value::as_str)))
            .ok_or_else(|| Lost("the tool result has no text".into()))?
            .to_owned();
        if result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            Ok(Self::Error(text))
        } else {
            Ok(Self::Text(text))
        }
    }
}

/// How a reply reaches the session: a call of one MCP tool. Newt's launcher
/// backs it with `newt-mcp-client` on a confined `amesh mcp --flock-session`
/// child; tests back it with a real session.
pub trait ToolCaller {
    /// Call `name` with `args`.
    ///
    /// # Errors
    /// [`Lost`] when the transport failed after the request may have gone.
    fn call_tool(&mut self, name: &str, args: Value) -> Result<ToolReply, Lost>;
}

/// A [`ToolCaller`] from a closure.
pub struct FnCaller<F>(pub F);

impl<F: FnMut(&str, Value) -> Result<ToolReply, Lost>> ToolCaller for FnCaller<F> {
    fn call_tool(&mut self, name: &str, args: Value) -> Result<ToolReply, Lost> {
        (self.0)(name, args)
    }
}

/// Where a reply stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Sent {
    /// Written down before the call; no answer recorded. After a crash this
    /// is as uncertain as [`Sent::Ambiguous`].
    Intent,
    /// The tool reported it admitted; the log has not yet shown it.
    Admitted {
        /// The event id the session reported.
        event: String,
        /// Its admission index.
        index: u64,
    },
    /// The session or server refused it: it is not in the log.
    Refused {
        /// The refusal, as text.
        why: String,
    },
    /// Sent with no clear answer: it may be in the log. Never re-sent here.
    Ambiguous {
        /// What was seen.
        why: String,
    },
    /// The verified log shows this member's reply.
    Confirmed {
        /// The reply's event id in the log.
        event: String,
    },
}

#[derive(Serialize, Deserialize)]
struct Row {
    reply: Reply,
    sent: Sent,
}

/// The durable record of replies: one JSON line per change, fsync'd, the
/// last line for a key wins. A torn final line (a crash mid-append) is
/// dropped; the line before it stands.
struct Journal {
    path: PathBuf,
    rows: BTreeMap<String, Row>,
}

impl Journal {
    fn open(dir: &Path) -> Result<Self, String> {
        {
            use std::os::unix::fs::DirBuilderExt as _;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(|e| format!("reply journal: {e}"))?;
        }
        let path = dir.join("replies.jsonl");
        let mut rows = BTreeMap::new();
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let lines: Vec<&str> = text.lines().collect();
                for (i, line) in lines.iter().enumerate() {
                    match serde_json::from_str::<Row>(line) {
                        Ok(row) => {
                            rows.insert(row.reply.key(), row);
                        }
                        Err(_) if i + 1 == lines.len() && !text.ends_with('\n') => {
                            // a torn final append: drop it so the next line
                            // does not glue onto it
                            let keep = text.rfind('\n').map_or(0, |n| n + 1);
                            let f = std::fs::OpenOptions::new()
                                .write(true)
                                .open(&path)
                                .map_err(|e| format!("reply journal: {e}"))?;
                            f.set_len(keep as u64)
                                .and_then(|()| f.sync_all())
                                .map_err(|e| format!("reply journal: {e}"))?;
                        }
                        Err(e) => return Err(format!("reply journal line {}: {e}", i + 1)),
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("reply journal: {e}")),
        }
        Ok(Self { path, rows })
    }

    fn put(&mut self, reply: &Reply, sent: Sent) -> Result<(), String> {
        let row = Row {
            reply: reply.clone(),
            sent,
        };
        let mut line = serde_json::to_string(&row).map_err(|e| e.to_string())?;
        line.push('\n');
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| format!("reply journal: {e}"))?;
        f.write_all(line.as_bytes())
            .and_then(|()| f.sync_all())
            .map_err(|e| format!("reply journal: {e}"))?;
        self.rows.insert(reply.key(), row);
        Ok(())
    }
}

/// Sends replies and tracks them to the log.
pub struct Replies<C> {
    member: String,
    journal: Journal,
    caller: C,
}

impl<C: ToolCaller> Replies<C> {
    /// Replies for `member`, journalled in the private directory `dir`.
    ///
    /// # Errors
    /// The journal cannot be read or created.
    pub fn open(dir: &Path, member: impl Into<String>, caller: C) -> Result<Self, String> {
        Ok(Self {
            member: member.into(),
            journal: Journal::open(dir)?,
            caller,
        })
    }

    /// Send `reply` once. A reply with any recorded state is not sent again:
    /// the recorded state is returned.
    ///
    /// # Errors
    /// The journal could not record the intent; nothing was sent.
    pub fn send(&mut self, reply: &Reply) -> Result<Sent, String> {
        if let Some(row) = self.journal.rows.get(&reply.key()) {
            return Ok(row.sent.clone());
        }
        // written, and durable, before the request can leave
        self.journal.put(reply, Sent::Intent)?;
        let sent = match self.caller.call_tool("flock_emit", reply.args()) {
            Ok(ToolReply::Text(t)) => parse_admitted(&t).map_or_else(
                || Sent::Ambiguous {
                    why: format!("the tool answered something else: {}", clip(&t)),
                },
                |(event, index)| Sent::Admitted { event, index },
            ),
            Ok(ToolReply::Error(t)) if t.contains("refused:") => Sent::Refused { why: clip(&t) },
            Ok(ToolReply::Error(t)) => Sent::Ambiguous { why: clip(&t) },
            Err(Lost(w)) => Sent::Ambiguous { why: clip(&w) },
        };
        self.journal.put(reply, sent.clone())?;
        Ok(sent)
    }

    /// The member these replies are sent as.
    #[must_use]
    pub fn member(&self) -> &str {
        &self.member
    }

    /// Where a task's reply stands, if one was ever started.
    #[must_use]
    pub fn state(&self, flock: &str, event: &str, step: Step) -> Option<&Sent> {
        self.journal
            .rows
            .get(&format!("{flock}/{event}/{step:?}"))
            .map(|r| &r.sent)
    }

    /// Settle every unconfirmed reply against the verified log: a reply this
    /// member signed that answers the event (same kind, reference and, for a
    /// verdict, head) is `Confirmed`. Nothing else changes; a reply the log
    /// does not show stays as it was and is never re-sent. Returns how many
    /// were confirmed.
    ///
    /// # Errors
    /// The journal could not record a confirmation.
    pub fn reconcile(&mut self, follower: &Follower) -> Result<usize, String> {
        let board = follower.board();
        let pending: Vec<Reply> = self
            .journal
            .rows
            .values()
            .filter(|r| {
                !matches!(r.sent, Sent::Confirmed { .. } | Sent::Refused { .. })
                    && r.reply.flock == board.flock().to_string()
            })
            .map(|r| r.reply.clone())
            .collect();
        let mut n = 0;
        for reply in pending {
            let found = board.entries().find(|e| {
                e.conflict.is_none()
                    && e.member.as_deref() == Some(self.member.as_str())
                    && e.event.kind == reply.kind
                    && e.event
                        .refs
                        .get(&reply.role)
                        .map(ToString::to_string)
                        .as_deref()
                        == Some(reply.event.as_str())
                    && match (&e.event.body, &reply.head) {
                        (Body::Verdict { head, .. }, Some(want)) => head == want,
                        (Body::Verdict { .. }, None) => false,
                        _ => true,
                    }
            });
            if let Some(e) = found {
                self.journal.put(
                    &reply,
                    Sent::Confirmed {
                        event: e.id.to_string(),
                    },
                )?;
                n += 1;
            }
        }
        Ok(n)
    }
}

fn parse_admitted(text: &str) -> Option<(String, u64)> {
    let rest = text.strip_prefix("admitted ")?;
    let (event, at) = rest.split_once(" at ")?;
    let index = at.trim().parse().ok()?;
    event
        .parse::<content_addressable::ContentId>()
        .ok()
        .map(|id| (id.to_string(), index))
}

/// Text from the session, kept short and inert for the journal and screen.
fn clip(t: &str) -> String {
    let t = agent_mesh_flock::text::inert(t);
    t.chars().take(200).collect()
}

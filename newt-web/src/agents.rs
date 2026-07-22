//! The agent registry — newt-web's only stateful piece (W2, decision record
//! `docs/decisions/newt_web_htmx.md`).
//!
//! Composition, not implementation: each spawned agent is a
//! [`newt_core::TurnDriver`] owned by its own tokio task (the "pump"), driven
//! exactly the way the cowork ratatui consumer drives it — `submit`/`poll` —
//! and observed through a `watch` channel of transcript snapshots the SSE
//! route streams from. newt-web contains no agent logic at all.
//!
//! v0 authority note: spawned agents run under `TurnDriverConfig`'s default
//! caveats. Tightening the web-spawned grant (a caveat form / preset picker)
//! is W8 hardening, per D2/D3.

use newt_core::{BackendKind, Role, TurnDriver, TurnDriverConfig};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch};

/// What a tab needs to render one agent, snapshotted per pump tick.
#[derive(Clone, Default, PartialEq)]
pub(crate) struct Snapshot {
    /// `(role, content)` pairs, in order. Content is raw model/user text —
    /// the renderer escapes it.
    pub messages: Vec<(String, String)>,
    /// A turn is in flight.
    pub busy: bool,
    /// The pump exited (agent deleted / driver dead); the tab shows it inert.
    pub closed: bool,
}

pub(crate) enum Cmd {
    Prompt(String),
    Shutdown,
}

/// One live agent as the routes see it: metadata + the command inbox + the
/// snapshot feed. The `TurnDriver` itself lives inside the pump task.
pub(crate) struct AgentHandle {
    pub name: String,
    pub model: String,
    /// W4: a store-follow tab — read-only mirror of a conversation on the box;
    /// prompts are refused, "delete" merely unfollows.
    pub readonly: bool,
    pub cmd: mpsc::UnboundedSender<Cmd>,
    pub snapshots: watch::Receiver<Snapshot>,
}

#[derive(Default)]
pub(crate) struct Registry {
    next_id: AtomicU64,
    agents: Mutex<HashMap<u64, AgentHandle>>,
}

/// Spawn parameters, straight off the HTMX form.
pub(crate) struct Spec {
    pub name: String,
    pub url: String,
    pub model: String,
    pub kind: BackendKind,
    pub workspace: String,
}

impl Registry {
    /// Spawn an agent: build the driver config, start the pump task, register
    /// the handle. Returns the new agent id.
    pub(crate) fn spawn(self: &Arc<Self>, spec: Spec) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (snap_tx, snap_rx) = watch::channel(Snapshot::default());
        let config = TurnDriverConfig::new(&spec.url, &spec.model, spec.kind, &spec.workspace);
        tokio::spawn(pump(config, cmd_rx, snap_tx));
        self.agents.lock().unwrap().insert(
            id,
            AgentHandle {
                name: spec.name,
                model: spec.model,
                readonly: false,
                cmd: cmd_tx,
                snapshots: snap_rx,
            },
        );
        id
    }

    /// Send a prompt to an agent. `false` if the id is unknown or the tab is
    /// a read-only follow (D2: the running session stays the sole writer).
    pub(crate) fn prompt(&self, id: u64, text: String) -> bool {
        match self.agents.lock().unwrap().get(&id) {
            Some(a) if !a.readonly => a.cmd.send(Cmd::Prompt(text)).is_ok(),
            _ => false,
        }
    }

    /// W4: follow a conversation in the shared ConversationStore, read-only.
    /// A dedicated OS thread polls the store (its own connection — the store
    /// is multi-process-safe by design) and publishes the same Snapshot shape
    /// the agent pumps use, so the whole tab surface is reused unchanged.
    pub(crate) fn spawn_follow(
        self: &Arc<Self>,
        state_dir: std::path::PathBuf,
        workspace: std::path::PathBuf,
        conv_id: String,
        title: String,
    ) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let (snap_tx, snap_rx) = watch::channel(Snapshot::default());
        std::thread::spawn(move || {
            let store = match newt_core::ConversationStore::new(&state_dir, &workspace, 1000) {
                Ok(s) => s,
                Err(e) => {
                    let _ = snap_tx.send(Snapshot {
                        messages: vec![("system".into(), format!("store unavailable: {e}"))],
                        busy: false,
                        closed: true,
                    });
                    return;
                }
            };
            loop {
                // Shutdown (or registry drop) ends the follow.
                match cmd_rx.try_recv() {
                    Ok(Cmd::Shutdown) | Err(mpsc::error::TryRecvError::Disconnected) => {
                        let _ = snap_tx.send_if_modified(|s| {
                            s.closed = true;
                            true
                        });
                        return;
                    }
                    _ => {}
                }
                let snap = match store.load(&conv_id) {
                    Ok(rec) => Snapshot {
                        messages: rec
                            .turns
                            .iter()
                            .flat_map(|t| {
                                [
                                    ("user".to_string(), t.user.clone()),
                                    ("assistant".to_string(), t.assistant.clone()),
                                ]
                            })
                            .collect(),
                        busy: false,
                        closed: false,
                    },
                    Err(e) => Snapshot {
                        messages: vec![("system".into(), format!("cannot load: {e}"))],
                        busy: false,
                        closed: false,
                    },
                };
                snap_tx.send_if_modified(|s| {
                    if *s != snap {
                        *s = snap;
                        true
                    } else {
                        false
                    }
                });
                // All views gone → stop polling the store.
                if snap_tx.is_closed() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(750));
            }
        });
        self.agents.lock().unwrap().insert(
            id,
            AgentHandle {
                name: title,
                model: "follow".to_string(),
                readonly: true,
                cmd: cmd_tx,
                snapshots: snap_rx,
            },
        );
        id
    }

    /// Remove an agent: signal shutdown (the pump cancels any in-flight turn)
    /// and drop the handle. `false` if the id is unknown.
    pub(crate) fn remove(&self, id: u64) -> bool {
        match self.agents.lock().unwrap().remove(&id) {
            Some(a) => {
                let _ = a.cmd.send(Cmd::Shutdown);
                true
            }
            None => false,
        }
    }

    /// Subscribe to an agent's snapshot feed (for the SSE route).
    pub(crate) fn subscribe(&self, id: u64) -> Option<watch::Receiver<Snapshot>> {
        self.agents
            .lock()
            .unwrap()
            .get(&id)
            .map(|a| a.snapshots.clone())
    }

    /// `(id, name, model, readonly, snapshot)` for every live tab, id-ordered
    /// — the render source for strip + panels.
    pub(crate) fn list(&self) -> Vec<(u64, String, String, bool, Snapshot)> {
        let mut v: Vec<_> = self
            .agents
            .lock()
            .unwrap()
            .iter()
            .map(|(id, a)| {
                (
                    *id,
                    a.name.clone(),
                    a.model.clone(),
                    a.readonly,
                    a.snapshots.borrow().clone(),
                )
            })
            .collect();
        v.sort_by_key(|(id, ..)| *id);
        v
    }
}

/// The pump: owns the `TurnDriver`, applies commands, and publishes a snapshot
/// whenever it changes. Polling cadence mirrors the cowork consumer (~10 Hz).
async fn pump(
    config: TurnDriverConfig,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    snap_tx: watch::Sender<Snapshot>,
) {
    let mut driver = TurnDriver::new(config);
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(Cmd::Prompt(text)) => {
                    // A submit while busy is refused by the driver; surface it
                    // as a snapshot the tab can render rather than dropping it
                    // silently.
                    if driver.submit(text).is_err() {
                        snap_tx.send_if_modified(|s| {
                            let note = (
                                "system".to_string(),
                                "busy — a turn is already running".to_string(),
                            );
                            if s.messages.last() != Some(&note) {
                                s.messages.push(note);
                                true
                            } else {
                                false
                            }
                        });
                    }
                }
                Some(Cmd::Shutdown) | None => {
                    driver.cancel();
                    snap_tx.send_if_modified(|s| {
                        s.closed = true;
                        s.busy = false;
                        true
                    });
                    return;
                }
            },
            _ = tick.tick() => {
                let _ = driver.poll();
                let snap = Snapshot {
                    messages: driver
                        .transcript()
                        .iter()
                        .map(|m| (role_name(&m.role).to_string(), m.content.clone()))
                        .collect(),
                    busy: driver.is_running(),
                    closed: false,
                };
                snap_tx.send_if_modified(|s| {
                    if *s != snap {
                        *s = snap;
                        true
                    } else {
                        false
                    }
                });
            }
        }
    }
}

fn role_name(role: &Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        _ => "system",
    }
}

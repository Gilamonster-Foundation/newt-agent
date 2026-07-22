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
                cmd: cmd_tx,
                snapshots: snap_rx,
            },
        );
        id
    }

    /// Send a prompt to an agent. `false` if the id is unknown.
    pub(crate) fn prompt(&self, id: u64, text: String) -> bool {
        match self.agents.lock().unwrap().get(&id) {
            Some(a) => a.cmd.send(Cmd::Prompt(text)).is_ok(),
            None => false,
        }
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

    /// `(id, name, model, snapshot)` for every live agent, id-ordered — the
    /// index page's render source.
    pub(crate) fn list(&self) -> Vec<(u64, String, String, Snapshot)> {
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

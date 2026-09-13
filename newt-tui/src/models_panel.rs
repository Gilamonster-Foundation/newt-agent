//! **The model picker** — a windowed list you walk with the arrow keys.
//!
//! `/models` printed fifty-two names and asked you to type one back. The list
//! itself was fine; what was missing was any way to ACT on it, so the reply to
//! "which model?" was a second command and an exact spelling.
//!
//! # Why a list and not the dial that already existed
//!
//! `settings_panel` has a `ModelRow` — a `‹ prev / next ›` dial over the same
//! options. That shape is right for a vocabulary of three or four (edit-mode,
//! tenacity) and wrong for fifty-two: reaching the last entry is fifty-one
//! keypresses past a value you cannot see coming. A dial asks "which of these
//! few", a list asks "which of these many", and the served-model list is
//! emphatically the second question.
//!
//! The dial stays where it is. This is not a replacement for it; a settings
//! form with a fifty-row list embedded in it would be a worse settings form.
//!
//! # What is reused
//!
//! All of it. The loop is `panel::drive` (#2024), shared with `/psyche`,
//! `/settings` and `/backends`; the chrome is `config_panel::render_panel`,
//! which renders whatever slice it is handed; the cursor-and-window
//! arithmetic is `list_cursor`, which exists so the five panels behind it do
//! not each grow their own.
//!
//! Residency comes from the backend probe. Router actions run between panel
//! visits, outside the draw/key loop, and the next visit re-reads server state.

use crate::config_panel::{render_panel_with_styles, status_line, ModelChoice, RowView};
use crate::list_cursor::ListCursor;
use crate::panel::{Flow, Key, Screen};

/// What the operator did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Enter on a row: switch to this model.
    Chose(String),
    /// Esc, or Enter on the model already active.
    Cancelled,
    Manage(Action, String),
    Refresh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Load,
    Unload,
    LoadOnly,
}

pub(crate) struct ModelsPanel {
    models: Vec<ModelChoice>,
    active: String,
    cursor: ListCursor,
    chose: Option<String>,
    management: bool,
    action: Option<Action>,
    confirm: Option<Action>,
    refresh: bool,
    status: String,
}

/// Body rows the picker shows at once.
///
/// Nine, not "as many as fit": `panel::drive` takes a fixed inline height, and
/// a panel that claimed the whole terminal would push the conversation out of
/// view to answer a question about it. Nine is enough to see a neighbourhood
/// and short enough to leave the transcript legible behind it.
const VISIBLE: usize = 9;

/// Bare model commands share the picker; arguments retain text dispatch.
pub(crate) fn requested(tokens: &[&str]) -> bool {
    matches!(tokens, ["model"] | ["models"])
}

/// Two border rows, the header, and the hint line, on top of the body.
pub(crate) fn panel_height() -> u16 {
    u16::try_from(VISIBLE + 4).unwrap_or(u16::MAX)
}

impl ModelsPanel {
    pub(crate) fn new(models: Vec<ModelChoice>, active: String) -> Self {
        // Open ON the active model rather than at the top. The question a
        // picker answers is "what else", and that is asked from where you are.
        let at = models
            .iter()
            .position(|m| m.name == active)
            .unwrap_or_default();
        Self {
            cursor: ListCursor::new(models.len(), VISIBLE, at),
            models,
            active,
            chose: None,
            management: false,
            action: None,
            confirm: None,
            refresh: false,
            status: String::new(),
        }
    }

    /// The visible window, as rows the shared renderer understands.
    fn window(&self) -> Vec<RowView> {
        self.models
            .iter()
            .enumerate()
            .skip(self.cursor.top())
            .take(VISIBLE)
            .map(|(i, model)| RowView {
                label: "",
                value: model.name.clone(),
                // Session selection and server residency are independent:
                // an active session may refer to an unloaded model.
                provenance: if model.tag == "[loaded]" {
                    " [loaded]".to_string()
                } else {
                    String::new()
                },
                selected: i == self.cursor.at(),
                editable: false,
            })
            .collect()
    }

    fn row_style(&self, row: &RowView) -> (ratatui::style::Style, ratatui::style::Style) {
        use crate::theme::{style, Role};
        use ratatui::style::Style;
        let value = if row.value == self.active {
            style(Role::ActiveModel)
        } else {
            Style::default()
        };
        (value, style(Role::LoadedModel))
    }

    fn title(&self) -> String {
        if self.models.is_empty() {
            " models — none served ".to_string()
        } else {
            format!(
                " models — {} of {} ",
                self.cursor.at() + 1,
                self.models.len()
            )
        }
    }

    pub(crate) fn outcome(self) -> Outcome {
        if self.refresh {
            return Outcome::Refresh;
        }
        if let (Some(action), Some(name)) = (self.action, self.chose.as_ref()) {
            return Outcome::Manage(action, name.clone());
        }
        match self.chose {
            // Enter on the model already running is a no-op, not a switch. It
            // would otherwise tear down and redial the session to arrive
            // exactly where it started.
            Some(name) if name != self.active => Outcome::Chose(name),
            _ => Outcome::Cancelled,
        }
    }
}

impl Screen for ModelsPanel {
    fn draw(&self, frame: &mut ratatui::Frame) {
        let hint = if let Some(action) = self.confirm {
            match action {
                Action::LoadOnly => {
                    "Unload all others; load highlighted model? Enter confirms · Esc back"
                        .to_string()
                }
                _ => "Unload highlighted model from memory? Enter confirms · Esc back".to_string(),
            }
        } else if !self.status.is_empty() {
            format!(
                "{} · ↑↓ move · Enter use · r refresh · Esc close",
                self.status
            )
        } else if self.management {
            "↑↓ choose · Enter use · l load · u unload · x load only this · r refresh · Esc close"
                .to_string()
        } else {
            "↑↓ choose · Enter use · ^u/^d page · g/G ends · r refresh · Esc close".to_string()
        };
        render_panel_with_styles(
            frame,
            &self.title(),
            &self.window(),
            status_line(&hint),
            0,
            0,
            |row| self.row_style(row),
        );
    }

    fn key(&mut self, key: Key) -> Flow {
        if let Some(action) = self.confirm {
            return match key {
                Key::Esc => {
                    self.confirm = None;
                    Flow::Stay
                }
                Key::Enter => {
                    self.chose = Some(self.models[self.cursor.at()].name.clone());
                    self.action = Some(action);
                    Flow::Close(true)
                }
                _ => Flow::Stay,
            };
        }
        self.status.clear();
        let page = self.cursor.page() as isize;
        match key {
            Key::Char('r') => {
                self.refresh = true;
                Flow::Close(true)
            }
            Key::Char('l' | 'u' | 'x') if self.management && !self.models.is_empty() => {
                let action = match key {
                    Key::Char('u') => Action::Unload,
                    Key::Char('x') => Action::LoadOnly,
                    _ => Action::Load,
                };
                if action == Action::Load {
                    self.chose = Some(self.models[self.cursor.at()].name.clone());
                    self.action = Some(action);
                    Flow::Close(true)
                } else {
                    self.confirm = Some(action);
                    Flow::Stay
                }
            }
            Key::Esc => Flow::Close(false),
            Key::Enter => {
                if let Some(model) = self.models.get(self.cursor.at()) {
                    self.chose = Some(model.name.clone());
                }
                Flow::Close(true)
            }
            // The vi vocabulary the rest of the crate already speaks —
            // `spill_view` and `transcript_pager` bind exactly these, so a
            // fourth scrolling surface should not invent a fifth set.
            Key::Up | Key::Char('k') => {
                self.cursor.step(-1);
                Flow::Stay
            }
            Key::Down | Key::Char('j') => {
                self.cursor.step(1);
                Flow::Stay
            }
            Key::Ctrl('u') => {
                self.cursor.step(-page);
                Flow::Stay
            }
            Key::Ctrl('d') => {
                self.cursor.step(page);
                Flow::Stay
            }
            Key::Char('g') => {
                self.cursor.home();
                Flow::Stay
            }
            Key::Char('G') => {
                self.cursor.end();
                Flow::Stay
            }
            _ => Flow::Stay,
        }
    }
}

/// Open the picker and report what the operator chose.
///
/// Assembled in ONE place, for `backend_chooser::choose`'s reason: a second
/// caller resolving the served list from config again would be a second answer
/// to "which models are choosable", and the two drift the first time one of
/// them learns something.
///
/// # Errors
///
/// The terminal could not be taken, built, polled, read or repainted.
pub(crate) fn choose(
    choice: &crate::BackendChoice,
    window: Option<crate::session_worker::PanelWindow>,
) -> anyhow::Result<Option<String>> {
    let active = choice.active_model.clone().unwrap_or_default();
    let mut focused = active.clone();
    let mut status = String::new();
    loop {
        let (models, management) = snapshot(choice)?;
        let mut panel = ModelsPanel::new(models, focused.clone());
        panel.active = active.clone();
        panel.management = management;
        panel.status = std::mem::take(&mut status);
        let applied = crate::panel::drive(&mut panel, panel_height(), window.as_ref())?;
        focused = panel
            .models
            .get(panel.cursor.at())
            .map(|m| m.name.clone())
            .unwrap_or_default();
        if !applied {
            return Ok(None);
        }
        match panel.outcome() {
            Outcome::Chose(name) => return Ok(Some(name)),
            Outcome::Cancelled => return Ok(None),
            Outcome::Refresh => {}
            Outcome::Manage(action, name) => {
                // The panel releases raw mode before network I/O. Refresh from
                // the server afterward; an HTTP success never invents residency.
                status = match manage(choice, action, &name) {
                    Ok(()) => "Request accepted".to_string(),
                    Err(e) => format!("Model action failed: {e}"),
                };
            }
        }
    }
}

pub(crate) fn snapshot(choice: &crate::BackendChoice) -> anyhow::Result<(Vec<ModelChoice>, bool)> {
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            use newt_core::backend_probe::{
                api_for_engine, detect_engine, fetch_llamacpp_model_states,
            };
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()?;
            let engine =
                detect_engine(&client, &choice.url, choice.kind, choice.api_key.as_deref()).await;
            if engine == Some(newt_core::config::Engine::LlamaCpp) {
                if let Ok(Some(states)) =
                    fetch_llamacpp_model_states(&client, &choice.url, choice.api_key.as_deref())
                        .await
                {
                    let models = states
                        .into_iter()
                        .map(|(name, state)| ModelChoice {
                            name,
                            tag: format!("[{state}]"),
                        })
                        .collect();
                    return Ok((models, true));
                }
            }
            let api = api_for_engine(choice.kind, engine);
            let names = api
                .list_models(&client, &choice.url, choice.api_key.as_deref())
                .await?;
            let warm = api
                .warm_models(&client, &choice.url, choice.api_key.as_deref())
                .await;
            let models = names
                .into_iter()
                .map(|name| {
                    let state = match &warm {
                        Some(loaded) if loaded.contains(&name) => "loaded",
                        Some(_) => "unloaded",
                        None => "state unknown",
                    };
                    ModelChoice {
                        name,
                        tag: format!("[{state}]"),
                    }
                })
                .collect();
            Ok((models, false))
        })
    })
}

fn manage(choice: &crate::BackendChoice, action: Action, name: &str) -> anyhow::Result<()> {
    let operation = match action {
        Action::Load => "Load",
        Action::Unload => "Unload",
        Action::LoadOnly => "Unload other resident models and load",
    };
    eprintln!("{operation} {name} — waiting for the server…");
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            use newt_core::backend_probe::{
                fetch_llamacpp_model_states, set_llamacpp_model_loaded,
            };
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()?;
            if action == Action::LoadOnly {
                // Re-read just before eviction; do not act on stale picker rows.
                let states =
                    fetch_llamacpp_model_states(&client, &choice.url, choice.api_key.as_deref())
                        .await?
                        .ok_or_else(|| {
                            anyhow::anyhow!("server no longer reports model residency")
                        })?;
                anyhow::ensure!(
                    states.iter().any(|(model, _)| model == name),
                    "selected model is no longer available"
                );
                for (other, state) in states {
                    if other != name && matches!(state.as_str(), "loaded" | "loading" | "sleeping")
                    {
                        set_llamacpp_model_loaded(
                            &client,
                            &choice.url,
                            choice.api_key.as_deref(),
                            &other,
                            false,
                        )
                        .await?;
                    }
                }
            }
            set_llamacpp_model_loaded(
                &client,
                &choice.url,
                choice.api_key.as_deref(),
                name,
                action != Action::Unload,
            )
            .await
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn load_only_evicts_other_residents_before_loading_and_stops_on_failure() {
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        for unload_status in [200, 503] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/models"))
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"data": [
                        {"id":"old", "status":{"value":"loaded"}},
                        {"id":"cold", "status":{"value":"unloaded"}},
                        {"id":"chosen", "status":{"value":"unloaded"}}
                    ]}),
                ))
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/models/unload"))
                .and(body_json(serde_json::json!({"model":"old"})))
                .respond_with(
                    ResponseTemplate::new(unload_status)
                        .set_body_json(serde_json::json!({"success":true})),
                )
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/models/load"))
                .and(body_json(serde_json::json!({"model":"chosen"})))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({"success":true})),
                )
                .expect(if unload_status == 200 { 1 } else { 0 })
                .mount(&server)
                .await;
            let choice = crate::BackendChoice::synthesized(
                "fixture",
                server.uri(),
                newt_core::BackendKind::Openai,
                Some("old".into()),
            );
            let result = manage(&choice, Action::LoadOnly, "chosen");
            assert_eq!(result.is_ok(), unload_status == 200);
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests[1].url.path(), "/models/unload");
            if unload_status == 200 {
                assert_eq!(requests[2].url.path(), "/models/load");
            }
            server.verify().await;
        }
    }

    #[test]
    fn eviction_requires_confirmation_and_escape_does_not_evict() {
        let mut p = panel(3, "model-00");
        p.management = true;
        p.key(Key::Down);
        assert_eq!(p.key(Key::Char('x')), Flow::Stay);
        assert_eq!(p.key(Key::Esc), Flow::Stay);
        assert_eq!(p.outcome(), Outcome::Cancelled);

        let mut p = panel(3, "model-00");
        p.management = true;
        p.key(Key::Down);
        p.key(Key::Char('x'));
        assert_eq!(p.key(Key::Enter), Flow::Close(true));
        assert_eq!(
            p.outcome(),
            Outcome::Manage(Action::LoadOnly, "model-01".into())
        );
    }

    #[test]
    fn load_state_is_visible_beside_the_name_and_controls_need_management() {
        let mut p = ModelsPanel::new(
            vec![ModelChoice {
                name: "resident".into(),
                tag: "[loaded]".into(),
            }],
            "resident".into(),
        );
        assert_eq!(p.window()[0].value, "resident");
        assert_eq!(p.window()[0].provenance, " [loaded]");
        assert_eq!(p.key(Key::Char('l')), Flow::Stay);
        assert_eq!(p.outcome(), Outcome::Cancelled);
    }

    #[test]
    fn unloaded_and_failed_rows_have_no_badges() {
        let p = ModelsPanel::new(
            vec![
                ModelChoice {
                    name: "cold".into(),
                    tag: "[unloaded]".into(),
                },
                ModelChoice {
                    name: "failed".into(),
                    tag: "[failed]".into(),
                },
            ],
            "cold".into(),
        );
        assert!(p.window().iter().all(|r| r.provenance.is_empty()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn optional_router_state_failure_keeps_the_model_list_usable() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/props"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"default_generation_settings":{}})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"data":[{"id":"fixture"}]})),
            )
            .mount(&server)
            .await;
        let choice = crate::BackendChoice::synthesized(
            "fixture",
            server.uri(),
            newt_core::BackendKind::Openai,
            None,
        );
        let (models, management) = snapshot(&choice).unwrap();
        assert_eq!(models[0].name, "fixture");
        assert!(!management);
    }

    #[test]
    fn both_bare_model_commands_open_the_picker() {
        assert!(requested(&["model"]));
        assert!(requested(&["models"]));
        for tokens in [
            vec![],
            vec!["model", "other"],
            vec!["models", "capabilities"],
            vec!["backends"],
        ] {
            assert!(!requested(&tokens));
        }
    }

    fn models(n: usize) -> Vec<ModelChoice> {
        (0..n)
            .map(|i| ModelChoice {
                name: format!("model-{i:02}"),
                tag: String::new(),
            })
            .collect()
    }

    fn panel(n: usize, active: &str) -> ModelsPanel {
        ModelsPanel::new(models(n), active.to_string())
    }

    /// The window invariant itself lives in `list_cursor`; what this asserts
    /// is that the picker's RENDERED rows track it — a correct cursor drawn
    /// through a wrong slice is still a picker you cannot use.
    #[test]
    fn the_rendered_window_always_contains_the_selected_row() {
        let mut p = panel(52, "model-00");
        for _ in 0..60 {
            p.key(Key::Down);
            assert_eq!(
                p.window().iter().filter(|r| r.selected).count(),
                1,
                "exactly one rendered row is selected"
            );
        }
        assert!(p
            .window()
            .iter()
            .any(|r| r.value == "model-51" && r.selected));
    }

    /// Opening ON the active model is the point: the question a picker answers
    /// is "what else", and that is asked from where you already are.
    #[test]
    fn it_opens_on_the_active_model_not_at_the_top() {
        let p = panel(52, "model-40");
        assert_eq!(p.cursor.at(), 40);
        assert!(
            p.window()
                .iter()
                .any(|r| r.value == "model-40" && r.selected),
            "the active model is visible AND selected on open"
        );
    }

    /// An active model the backend no longer serves must not silently point
    /// the cursor at a different one.
    #[test]
    fn an_unserved_active_model_opens_at_the_top_rather_than_guessing() {
        let p = panel(5, "a-model-that-went-away");
        assert_eq!(p.cursor.at(), 0);
    }

    /// Enter on the model already running is a no-op. Treating it as a switch
    /// would tear down and redial the session to arrive where it started.
    #[test]
    fn choosing_the_active_model_is_cancel_not_a_redial() {
        let mut p = panel(5, "model-02");
        assert_eq!(p.key(Key::Enter), Flow::Close(true));
        assert_eq!(p.outcome(), Outcome::Cancelled);

        let mut p = panel(5, "model-02");
        p.key(Key::Down);
        assert_eq!(p.key(Key::Enter), Flow::Close(true));
        assert_eq!(p.outcome(), Outcome::Chose("model-03".to_string()));
    }

    #[test]
    fn esc_cancels_without_choosing() {
        let mut p = panel(5, "model-00");
        p.key(Key::Down);
        assert_eq!(p.key(Key::Esc), Flow::Close(false));
        assert_eq!(p.outcome(), Outcome::Cancelled);
    }

    /// Paging and the ends, which is what makes fifty-two navigable at all.
    #[test]
    fn paging_and_the_ends_reach_the_whole_list() {
        let mut p = panel(52, "model-00");
        p.key(Key::Char('G'));
        assert_eq!(p.cursor.at(), 51);
        assert!(p.window().iter().any(|r| r.value == "model-51"));
        p.key(Key::Char('g'));
        assert_eq!(p.cursor.at(), 0);

        p.key(Key::Ctrl('d'));
        assert_eq!(
            p.cursor.at(),
            VISIBLE - 1,
            "a page is one window, less an overlap row"
        );
        p.key(Key::Ctrl('u'));
        assert_eq!(p.cursor.at(), 0);
    }

    /// A backend serving nothing must render and refuse, not panic. `/models`
    /// on an unreachable endpoint is a normal Tuesday.
    #[test]
    fn an_empty_list_is_survivable() {
        let mut p = ModelsPanel::new(Vec::new(), "whatever".to_string());
        assert_eq!(p.cursor.at(), 0);
        assert!(p.window().is_empty());
        assert!(p.title().contains("none served"));
        p.key(Key::Down);
        p.key(Key::Ctrl('d'));
        p.key(Key::Char('G'));
        assert_eq!(p.cursor.at(), 0);
        assert_eq!(p.key(Key::Enter), Flow::Close(true));
        assert_eq!(p.outcome(), Outcome::Cancelled, "nothing to choose");
    }

    /// A list shorter than the window must not scroll at all, or the last rows
    /// render against blank space the operator can walk into.
    #[test]
    fn a_short_list_never_scrolls() {
        let mut p = panel(3, "model-00");
        p.key(Key::Char('G'));
        assert_eq!(
            p.cursor.top(),
            0,
            "three rows in a nine-row window: no scroll"
        );
        assert_eq!(p.window().len(), 3);
    }

    /// Active styling remains distinct from the moving keyboard cursor.
    #[test]
    fn exactly_one_row_is_marked_active() {
        let p = panel(52, "model-07");
        let marked: Vec<_> = p
            .window()
            .into_iter()
            .filter(|r| {
                p.row_style(r).0.fg == Some(crate::theme::color(crate::theme::Role::ActiveModel))
            })
            .collect();
        assert_eq!(marked.len(), 1);
        assert_eq!(marked[0].value, "model-07");
    }
}

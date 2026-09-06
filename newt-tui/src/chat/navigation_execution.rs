//! Execute parsed navigator commands against the session's indexes and ledger.
//!
//! Parsing stays in `crate::navigator_cmds`; index warmup and terminal output
//! remain in `chat`.

pub(super) fn handle_nav_command(
    cmd: crate::navigator_cmds::NavCommand,
    workspace: &str,
    nav_session: &mut newt_core::NavigatorSession,
    where_is: Option<&newt_core::WhereIsIndex>,
    index_status: &newt_core::IndexStatus,
) -> String {
    use crate::navigator_cmds::{NavCommand, RetrievalView};
    use newt_core::{
        compare_ledgers, compare_semantic_lexical, export_ledger_json, export_ledger_markdown,
        find_callees, find_callers, find_hierarchy, find_implementations, find_references,
        find_tests, format_ledger_diff, format_ledger_human, format_ledger_model, goto_definition,
        hash_context, impact_analysis, inspect_type, project_map_nav, text_search,
        GotoDefinitionArgs,
    };
    let id = index_status.index_id();
    let record =
        |nav_session: &mut newt_core::NavigatorSession, query: &str, nav: newt_core::NavResult| {
            nav_session.turn_counter = nav_session.turn_counter.saturating_add(1);
            let ctx = hash_context(nav.render().as_bytes());
            nav_session
                .ledger
                .record_nav(nav_session.turn_counter, query, &nav, &ctx);
            let rendered = nav.render();
            nav_session.last_nav = Some(nav);
            rendered
        };
    match cmd {
        NavCommand::Help(msg) => msg.to_string(),
        NavCommand::Def(sym) => {
            let Some(idx) = where_is else {
                return "where_is index not ready".into();
            };
            let nav = goto_definition(
                idx,
                GotoDefinitionArgs {
                    symbol: &sym,
                    kind: None,
                    index_id: &id,
                    files: Some(nav_session.files.as_slice()),
                },
            );
            record(nav_session, &sym, nav)
        }
        NavCommand::Text(q) => {
            let nav = text_search(&q, std::path::Path::new(workspace), &id);
            nav_session.last_lexical = Some(nav.clone());
            record(nav_session, &q, nav)
        }
        NavCommand::Uses(sym) => {
            let Some(idx) = nav_session.usage.as_ref() else {
                return "usage index not ready".into();
            };
            let nav = find_references(idx, &sym);
            record(nav_session, &sym, nav)
        }
        NavCommand::Tests(sym) => {
            let Some(idx) = nav_session.usage.as_ref() else {
                return "usage index not ready".into();
            };
            let nav = find_tests(idx, &sym);
            record(nav_session, &sym, nav)
        }
        NavCommand::Map { expand } => {
            if nav_session.project.is_none() {
                return "no project model detected for this workspace".into();
            }
            let seed = newt_core::project_map::load_seed(std::path::Path::new(workspace));
            let (out, nav) = {
                let model = nav_session.project.as_ref().expect("checked above");
                let mut out = newt_core::project_map::render_project_map(model, &seed)
                    .unwrap_or_else(|| "(empty project map)\n".into());
                if let Some(unit) = expand.as_ref() {
                    if let Some(u) = model
                        .units
                        .iter()
                        .find(|u| u.name == *unit || u.dir == *unit)
                    {
                        out.push_str(&format!(
                            "\nexpanded `{unit}`:\n  dir: {}\n  roots: {:?}\n  deps: {:?}\n  langs: {:?}\n",
                            u.dir, u.source_roots, u.deps, u.languages
                        ));
                    } else {
                        out.push_str(&format!("\n(no unit named `{unit}`)\n"));
                    }
                }
                let nav = project_map_nav(model, expand.as_deref(), &id);
                (out, nav)
            };
            if let Some(unit) = expand.clone() {
                nav_session.map_expand = Some(unit);
            }
            let _ = record(nav_session, expand.as_deref().unwrap_or("map"), nav);
            out
        }
        NavCommand::Callers(sym) => {
            let Some(idx) = nav_session.graph.as_ref() else {
                return "graph index not ready".into();
            };
            record(nav_session, &sym, find_callers(idx, &sym))
        }
        NavCommand::Callees(sym) => {
            let Some(idx) = nav_session.graph.as_ref() else {
                return "graph index not ready".into();
            };
            record(nav_session, &sym, find_callees(idx, &sym))
        }
        NavCommand::Implementations(sym) => {
            let Some(idx) = nav_session.graph.as_ref() else {
                return "graph index not ready".into();
            };
            record(nav_session, &sym, find_implementations(idx, &sym))
        }
        NavCommand::Hierarchy(sym) => {
            let Some(idx) = nav_session.graph.as_ref() else {
                return "graph index not ready".into();
            };
            record(nav_session, &sym, find_hierarchy(idx, &sym))
        }
        NavCommand::Type(sym) => {
            let nav = inspect_type(&sym, &nav_session.files, where_is, &id);
            record(nav_session, &sym, nav)
        }
        NavCommand::Impact(unit) => {
            let Some(model) = nav_session.project.as_ref() else {
                return "no project model — cannot compute impact".into();
            };
            let report = impact_analysis(
                &unit,
                model,
                &nav_session.files,
                std::path::Path::new(workspace),
            );
            let nav = report.to_nav(&id);
            let text = report.render();
            let _ = record(nav_session, &unit, nav);
            text
        }
        NavCommand::Retrieval { turn, view } => {
            let t = match turn {
                Some(n) => nav_session.ledger.get_turn(n),
                None => nav_session.ledger.turns.last(),
            };
            match t {
                None => "no retrieval ledger entries yet".into(),
                Some(tr) => match view {
                    RetrievalView::Human => format_ledger_human(tr),
                    RetrievalView::Model => format_ledger_model(tr),
                    RetrievalView::Diff => {
                        let prior = match turn {
                            Some(n) => nav_session.ledger.prior_turn(n),
                            None => {
                                let len = nav_session.ledger.turns.len();
                                if len >= 2 {
                                    Some(&nav_session.ledger.turns[len - 2])
                                } else {
                                    None
                                }
                            }
                        };
                        match prior {
                            Some(a) => format_ledger_diff(a, tr),
                            None => format_ledger_human(tr),
                        }
                    }
                },
            }
        }
        NavCommand::CompareSemanticLexical => compare_semantic_lexical(
            nav_session.last_semantic.as_ref(),
            nav_session.last_lexical.as_ref(),
        ),
        NavCommand::CompareTurns(a, b) => compare_ledgers(&nav_session.ledger, a, b),
        NavCommand::CompareIndex => format!(
            "session-index previous={:?} current={:?}\n",
            nav_session.ledger.previous_index_id, nav_session.ledger.current_index_id
        ),
        NavCommand::ExportJson => export_ledger_json(&nav_session.ledger),
        NavCommand::ExportMarkdown => export_ledger_markdown(&nav_session.ledger),
    }
}

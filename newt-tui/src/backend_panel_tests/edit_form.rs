use super::*;

#[test]
fn edit_form_prefills_and_saves_through_injected_persist() {
    let mut s = panel();
    s.cycle(1); // → gpu-runner
    s.begin_edit();
    let Mode::Form(form) = &s.mode else {
        panic!("edit should open the form");
    };
    assert_eq!(form.editing.as_deref(), Some("gpu-runner"));
    assert_eq!(form.url, "http://gpu-runner:11434");
    assert_eq!(form.model, "qwen3:30b");
    // ↓↓↓ to the model field, clear it, type a new one.
    s.form_nav(1); // kind
    s.form_nav(1); // url
    s.form_nav(1); // model
    for _ in 0.."qwen3:30b".len() {
        s.form_backspace();
    }
    type_text(&mut s, "llama3.1:8b");
    let mut seen: Vec<BackendEdit> = Vec::new();
    let mut persist = |edit: &BackendEdit| {
        seen.push(edit.clone());
        BackendSaveResult::Saved {
            note: "saved backend 'gpu-runner' → /tmp/gpu-runner.toml".to_string(),
        }
    };
    assert!(s.submit_form(&mut persist));
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].name, "gpu-runner");
    assert_eq!(seen[0].model.as_deref(), Some("llama3.1:8b"));
    assert!(seen[0].replace, "editing replaces the existing drop-in");
    // The chooser folded the save back in and recorded the change.
    assert_eq!(s.mode, Mode::Choose);
    assert_eq!(
        s.options[1].model.as_deref(),
        Some("llama3.1:8b"),
        "chooser reflects the edit"
    );
    assert_eq!(
        s.changes,
        vec!["saved backend 'gpu-runner' → /tmp/gpu-runner.toml"]
    );
}

#[test]
fn add_form_validates_before_any_io_runs() {
    let mut called = 0usize;
    let mut persist = |_: &BackendEdit| {
        called += 1;
        BackendSaveResult::Saved {
            note: String::new(),
        }
    };
    let cases: &[(&str, &str, &str)] = &[
        // (name, url, expected status fragment)
        ("", "http://x:1", "needs a name"),
        ("bad name", "http://x:1", "letters, digits"),
        ("dgx1", "http://x:1", "already exists"),
        ("fresh", "", "needs a url"),
        ("fresh", "host:11434", "http:// or https://"),
    ];
    for (name, url, want) in cases {
        let mut s = panel();
        s.begin_add();
        type_text(&mut s, name);
        s.form_nav(1); // kind
        s.form_nav(1); // url
        type_text(&mut s, url);
        assert!(!s.submit_form(&mut persist), "case {name:?} {url:?}");
        assert!(
            s.status.as_deref().unwrap_or("").contains(want),
            "case {name:?} {url:?} → {:?}",
            s.status
        );
        assert!(matches!(s.mode, Mode::Form(_)), "stays open to fix it");
    }
    // A whitespace-y api-key env is refused too.
    let mut s = panel();
    s.begin_add();
    type_text(&mut s, "fresh");
    s.form_nav(1);
    s.form_nav(1);
    type_text(&mut s, "http://x:1");
    s.form_nav(1); // model
    s.form_nav(1); // key env
    type_text(&mut s, "NOT AVAR");
    assert!(!s.submit_form(&mut persist));
    assert!(s.status.as_deref().unwrap().contains("bare variable name"));
    assert_eq!(called, 0, "validation failures never reach the disk");
}

#[test]
fn failed_persist_keeps_the_form_open_and_mutates_nothing() {
    // review-3 §1: a failed write keeps the panel open with a visible
    // status; options, changes, and the runtime stay untouched.
    let mut s = panel();
    let before = s.options.clone();
    s.begin_add();
    type_text(&mut s, "newbie");
    s.form_nav(1); // kind
    s.form_nav(1); // url
    type_text(&mut s, "http://newbie:8000");
    let mut persist = |_: &BackendEdit| BackendSaveResult::Failed("disk full".to_string());
    assert!(!s.submit_form(&mut persist));
    assert!(matches!(s.mode, Mode::Form(_)), "stays open for a retry");
    assert!(s.status.as_deref().unwrap().contains("disk full"));
    assert_eq!(s.options, before, "no phantom chooser entry");
    assert!(s.changes.is_empty(), "nothing recorded as changed");
    assert_eq!(close_outcome(false, &s), PanelClose::cancelled());
}

#[test]
fn add_saves_a_new_entry_before_the_kind_fallbacks() {
    let mut s = panel();
    s.begin_add();
    type_text(&mut s, "fresh");
    s.form_nav(1);
    s.form_nav(1);
    type_text(&mut s, "https://fresh.example");
    let mut persist = ok_persist();
    assert!(s.submit_form(&mut persist));
    assert_eq!(s.mode, Mode::Choose);
    assert_eq!(s.options[3].name, "fresh", "inserted after the named set");
    assert!(s.options[3].editable(), "a fresh drop-in is editable");
    assert!(matches!(
        s.options[4].selection,
        BackendSelection::Kind("ollama")
    ));
    assert_eq!(s.changes.len(), 1);
    // The active marker did not drift.
    assert!(s.pick_label().contains("dgx1") && s.pick_label().contains("(active)"));
}

#[test]
fn name_is_fixed_while_editing_but_free_while_adding() {
    let mut s = panel();
    s.begin_edit(); // dgx1
    type_text(&mut s, "x");
    let Mode::Form(form) = &s.mode else {
        panic!("form")
    };
    assert_eq!(form.name, "dgx1", "edit cannot rename");
    assert!(s.status.as_deref().unwrap().contains("fixed"));
    let mut a = panel();
    a.begin_add();
    type_text(&mut a, "brand-new");
    let Mode::Form(form) = &a.mode else {
        panic!("form")
    };
    assert_eq!(form.name, "brand-new");
}

/// §6: an edit that changed nothing performs NO I/O — the panel-open
/// prefill is never re-stamped over whatever the file says now.
#[test]
fn an_untouched_edit_writes_nothing() {
    let mut s = panel();
    let mut called = 0usize;
    let mut persist = |_: &BackendEdit| {
        called += 1;
        BackendSaveResult::Saved {
            note: String::new(),
        }
    };
    s.begin_edit();
    assert!(!s.submit_form(&mut persist));
    assert_eq!(called, 0, "no write for a no-op edit");
    assert_eq!(s.mode, Mode::Choose);
    assert!(s.status.as_deref().unwrap().contains("no changes"));
    assert!(s.changes.is_empty());
}

/// §6: a URL the operator never typed is neither validated nor written —
/// so an edit that only changes the model cannot fail on, or re-stamp, the
/// endpoint. Typing a bad one IS still refused.
#[test]
fn an_untouched_url_is_not_revalidated_but_a_typed_one_is() {
    let mut s = panel();
    s.cycle(1); // → gpu-runner
    s.begin_edit();
    s.form_nav(1);
    s.form_nav(1);
    s.form_nav(1); // model
    type_text(&mut s, "-instruct");
    let mut seen: Vec<BackendEdit> = Vec::new();
    {
        let mut persist = |edit: &BackendEdit| {
            seen.push(edit.clone());
            BackendSaveResult::Saved {
                note: "saved".to_string(),
            }
        };
        assert!(s.submit_form(&mut persist), "{:?}", s.status);
    }
    assert!(!seen[0].dirty.endpoint, "the untouched url is not written");
    let mut never = |_: &BackendEdit| BackendSaveResult::Saved {
        note: String::new(),
    };
    s.begin_edit();
    s.form_nav(1);
    s.form_nav(1); // url
    type_text(&mut s, " and rubbish");
    assert!(!s.submit_form(&mut never));
    assert!(s.status.as_deref().unwrap().contains("invalid url"));
}

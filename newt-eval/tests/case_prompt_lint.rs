//! #33: the bundled `case.toml` prompts must not re-specify the emission
//! shape (e.g. "respond with a unified diff only"). The system prompt owns
//! the emission contract; case prompts describe the task. This gates CI via
//! `cargo test --workspace`, so the #31 mistake cannot be reintroduced.

#[test]
fn bundled_case_prompts_do_not_respecify_emission_shape() {
    let dir = newt_eval::default_cases_dir();
    if !dir.exists() {
        // Soft-pass in scaffolds without a cases dir.
        return;
    }
    if let Err(errors) = newt_eval::lint_case_prompts(&dir) {
        let joined = errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n  ");
        panic!(
            "case-prompt lint found {} violation(s):\n  {joined}\n\n\
             Fix: remove the emission-shape directive — the system prompt owns it. \
             See newt-eval/cases/CASE_AUTHORING.md.",
            errors.len()
        );
    }
}

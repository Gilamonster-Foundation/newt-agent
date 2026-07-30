//! `newt config` — print the resolved configuration as a documented starter
//! (e.g. `newt config > ~/.newt/config.toml`).

use newt_core::Config;
use std::path::Path;

pub fn run(config_path: Option<&Path>) -> anyhow::Result<()> {
    let mut config = match config_path {
        Some(p) => Config::load(p)?,
        None => Config::resolve()?,
    };

    // Surface the prompt set to its built-in default (visible + editable) when
    // the user hasn't set one, so the dump is a usable starter config.
    let tui = config.tui.get_or_insert_with(Default::default);
    if tui.prompt.is_none() {
        tui.prompt = Some(newt_tui::DEFAULT_RICH_PROMPT.to_string());
    }

    // Surface the context-manager block (with its tunables at their built-in
    // defaults) so `newt config` documents input_ceiling_pct / low_budget_pct
    // as an editable starter, rather than hiding them behind an unset Option.
    config.context.get_or_insert_with(Default::default);

    println!("# Resolved Newt configuration  (Config::resolve() search order; secrets redacted)");
    println!("#");
    println!("# [tui] edit_mode = \"emacs\" | \"vi\"     (shipped default: emacs)");
    println!("# [tui] footer    = \"auto\" | \"on\" | \"off\"   (auto = rich prompt on a TTY)");
    println!("# [tui] markdown  = \"auto\" | \"on\" | \"off\"   (auto = render RichTUI Markdown when styled)");
    println!("# [tui] spill_lines = 3   (collapsed live + completed tool rows; 0 = unbounded completion only)");
    println!("#");
    println!("# [context] input_ceiling_pct = 80   (% of num_ctx usable as INPUT before trim;");
    println!("#           raise for large-window models like Opus, e.g. 90; clamped 1..=99)");
    println!("# [context] low_budget_pct   = 15   (% of ceiling below which the low-budget nudge");
    println!("#           fires; raise to be warned earlier, 0 disables; clamped 0..=100)");
    println!("#");
    println!(
        "# [[model_tuning]] model = \"...\", context_window = 200000   (total token window the"
    );
    println!(
        "#           backend accepts; seeds the input budget for OpenAI/NVIDIA endpoints that"
    );
    println!(
        "#           can't self-report a window via /api/show — an empirical probe still wins)"
    );
    println!("#");
    println!("# [tui] prompt — customize with these tokens (or run `/prompt` in a session).");
    println!("#   Prefer the $NAME macros here: TOML eats backslashes, so the \\x forms need");
    println!("#   a 'literal string' (single quotes) or doubled \\\\ (e.g. \"\\\\t\").");
    for (name, slash, desc) in newt_tui::PROMPT_TOKENS {
        if slash.is_empty() {
            println!("#   {name:<11}       {desc}");
        } else {
            println!("#   {name:<11}  {slash:<3}  {desc}");
        }
    }
    println!();

    let toml_str = config.to_redacted_toml()?;
    println!("{toml_str}");
    Ok(())
}

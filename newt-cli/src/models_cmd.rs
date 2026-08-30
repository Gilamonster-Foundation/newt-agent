//! `newt models` — manage the local palette of mini models for the on-host
//! embedded summarizer (#661 group C).
//!
//! `pull` fetches a model's GGUF **and** its `tokenizer.json` to
//! `~/.newt/models/<alias>/` (candle needs both — the quant GGUF repos do not
//! ship a standalone tokenizer, so it comes from `tokenizer_repo`). `list` shows
//! the palette and what is fully installed, `path` prints the resolved GGUF path.
//! Besides the interactive first-run auto-provision, `pull` is the explicit fetch
//! that lets the embedded CPU summarizer be the default without a GPU.

use clap::Subcommand;
use newt_inference::palette::{self, MiniModel};
use std::path::Path;

#[derive(Subcommand, Debug)]
pub enum ModelsCmd {
    /// List the palette (smallest-first) and which models are installed.
    List,
    /// Download a palette model's GGUF to ~/.newt/models/<alias>/ (default: the
    /// summarizer default, qwen2.5-0.5b). The one explicit fetch.
    Pull {
        /// Palette alias (e.g. `qwen2.5-0.5b`). Omit for the summarizer default.
        alias: Option<String>,
    },
    /// Print the resolved local GGUF path for a model (default: the summarizer
    /// default), whether or not it is present.
    Path {
        /// Palette alias. Omit for the summarizer default.
        alias: Option<String>,
    },
    /// Download the default embedding model (bge-small) to
    /// ~/.newt/models/bge-small-en-v1.5/ — the explicit fetch that lights up
    /// on-host semantic retrieval (#1279). Like `pull`, an operator action: no
    /// silent download (#639). Once fetched, semantic retrieval uses it
    /// automatically (no config).
    PullEmbed,
}

pub async fn run(cmd: ModelsCmd) -> anyhow::Result<()> {
    match cmd {
        ModelsCmd::List => list(),
        ModelsCmd::Pull { alias } => pull(alias.as_deref()).await,
        ModelsCmd::Path { alias } => print_path(alias.as_deref()),
        ModelsCmd::PullEmbed => pull_embed().await,
    }
}

/// Fetch the default embedding model's files (config + tokenizer + safetensors)
/// into `~/.newt/models/bge-small-en-v1.5/` (#1279). Reuses [`fetch_if_absent`]
/// so a half-provisioned dir completes in place; each file is skipped when
/// already present. The dir + files are exactly what `CandleEmbedder::new`
/// validates and what the config-resolution path auto-detects.
async fn pull_embed() -> anyhow::Result<()> {
    let dir = palette::embed_model_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve ~/.newt/models (no home dir)"))?;
    std::fs::create_dir_all(&dir)?;
    write_models_readme(&dir);
    println!(
        "Provisioning {} -> {}",
        palette::EMBED_HF_REPO,
        dir.display()
    );
    for file in palette::EMBED_REQUIRED_FILES {
        let url = format!(
            "https://huggingface.co/{}/resolve/main/{file}",
            palette::EMBED_HF_REPO
        );
        fetch_if_absent(&url, &dir.join(file), file).await?;
    }
    println!(
        "OK {} fully provisioned — semantic retrieval will use it automatically",
        palette::EMBED_HF_REPO
    );
    Ok(())
}

fn resolve(alias: Option<&str>) -> anyhow::Result<&'static MiniModel> {
    match alias {
        None => Ok(palette::default_model()),
        Some(a) => palette::find(a).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown model alias '{a}'. Available: {}",
                palette::palette()
                    .iter()
                    .map(|m| m.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }),
    }
}

/// One row of `newt models`, as data.
struct ModelRow {
    alias: String,
    ram_gb: f32,
    arch: String,
    installed: &'static str,
    note: String,
}

/// Render the `newt models` listing.
///
/// Extracted from `list` so the exact bytes are testable without a model
/// palette on disk (#1916). Byte-identical to the `println!`s it replaces —
/// the migration onto `markup::table` is the NEXT commit, so the golden below
/// pins what shipped before either change.
fn models_table(rows: &[ModelRow]) -> String {
    use newt_core::markup::table::{render_table, Align, Column};
    let columns = [
        Column::new("alias"),
        Column::new("RAM").align(Align::Right),
        Column::new("arch"),
        Column::new("installed"),
        Column::new("note"),
    ];
    let data: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            vec![
                r.alias.clone(),
                format!("{:.1}G", r.ram_gb),
                r.arch.clone(),
                r.installed.to_string(),
                r.note.clone(),
            ]
        })
        .collect();
    render_table(&columns, &data)
}

fn list() -> anyhow::Result<()> {
    let rows: Vec<ModelRow> = palette::palette()
        .iter()
        .map(|m| ModelRow {
            alias: m.name.to_string(),
            ram_gb: m.approx_ram_gb,
            arch: format!("{:?}", m.arch),
            installed: if palette::resolve_local(m.name).is_some() {
                "yes"
            } else {
                "no"
            },
            note: m.note.to_string(),
        })
        .collect();
    print!("{}", models_table(&rows));
    println!(
        "\nSummarizer default: {}   (fetch with: newt models pull [alias])",
        palette::default_model().name
    );
    let embed_installed = if palette::embed_model_dir_if_present().is_some() {
        "yes"
    } else {
        "no"
    };
    println!(
        "Embedding model (semantic retrieval): {}   installed: {}   (fetch with: newt models pull-embed)",
        palette::EMBED_HF_REPO,
        embed_installed
    );
    Ok(())
}

fn print_path(alias: Option<&str>) -> anyhow::Result<()> {
    let m = resolve(alias)?;
    let path = palette::local_gguf_path(m)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve ~/.newt/models (no home dir)"))?;
    println!("{}", path.display());
    if !path.is_file() {
        eprintln!("(not present — run `newt models pull {}`)", m.name);
    }
    Ok(())
}

async fn pull(alias: Option<&str>) -> anyhow::Result<()> {
    provision(alias).await?;
    Ok(())
}

/// Ensure a palette model is fully provisioned locally (GGUF + tokenizer), and
/// return its palette entry so callers can wire it into config.
pub async fn provision(alias: Option<&str>) -> anyhow::Result<&'static MiniModel> {
    let m = resolve(alias)?;
    let gguf = palette::local_gguf_path(m)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve ~/.newt/models (no home dir)"))?;
    let tok = palette::local_tokenizer_path(m)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve ~/.newt/models (no home dir)"))?;
    if let Some(parent) = gguf.parent() {
        std::fs::create_dir_all(parent)?;
        write_models_readme(parent);
    }
    // The embedded engine needs BOTH the weights and a standalone tokenizer.json.
    // Weights come from the quant GGUF repo; the tokenizer from `tokenizer_repo`
    // (the GGUF repo does not ship one — that was the init failure). Each fetch is
    // skipped when the file is already on disk, so a half-provisioned dir (e.g.
    // GGUF-only from an older newt) is completed in place.
    let gguf_url = format!(
        "https://huggingface.co/{}/resolve/main/{}",
        m.hf_repo, m.gguf_file
    );
    let tok_url = format!(
        "https://huggingface.co/{}/resolve/main/tokenizer.json",
        m.tokenizer_repo
    );
    println!(
        "Provisioning {} -> {}",
        m.name,
        gguf.parent().unwrap_or(&gguf).display()
    );
    fetch_if_absent(
        &gguf_url,
        &gguf,
        &format!("weights (~{:.1} GB)", m.approx_ram_gb),
    )
    .await?;
    fetch_if_absent(&tok_url, &tok, "tokenizer.json").await?;
    println!("OK {} fully provisioned", m.name);
    Ok(m)
}

/// Download `url` to `dest` unless it is already present. One-line status; used
/// by `newt models pull` for both the weights and the tokenizer.
async fn fetch_if_absent(url: &str, dest: &Path, label: &str) -> anyhow::Result<()> {
    if dest.is_file() {
        println!("  {label}: present");
        return Ok(());
    }
    println!("  {label}: fetching from {url}");
    download_to(url, dest).await?;
    Ok(())
}

/// Stream a URL to `dest` via a `.part` file, then atomically rename — so a
/// killed download never leaves a truncated GGUF that `resolve_local()` would
/// treat as installed.
async fn download_to(url: &str, dest: &Path) -> anyhow::Result<()> {
    use std::io::Write;
    let mut resp = reqwest::Client::new()
        .get(url)
        .send()
        .await?
        .error_for_status()?;
    let total = resp.content_length();
    let part = dest.with_extension("part");
    let mut file = std::fs::File::create(&part)?;
    let mut got: u64 = 0;
    let mut last_pct = 0u64;
    while let Some(chunk) = resp.chunk().await? {
        file.write_all(&chunk)?;
        got += chunk.len() as u64;
        if let Some(t) = total.filter(|&t| t > 0) {
            let pct = got * 100 / t;
            if pct >= last_pct + 5 {
                last_pct = pct;
                eprint!(
                    "\r  {pct}%  ({} / {} MB)   ",
                    got / 1_048_576,
                    t / 1_048_576
                );
            }
        }
    }
    file.flush()?;
    eprintln!();
    std::fs::rename(&part, dest)?;
    Ok(())
}

/// Build a background first-run provisioning task for the interactive splash
/// (#985, #661 group C). Returns a [`newt_tui::SetupHandle`] the splash covers
/// with a spinner while the model downloads on a std::thread; `None` when there's
/// nothing to do.
///
/// `None` unless ALL hold: built with the `embedded` feature, stdout is a TTY
/// (never provision in a pipe / headless worker / CI), the opt-out env
/// `NEWT_NO_MODEL_PULL` is unset, and the default model isn't fully present.
#[cfg(feature = "embedded")]
pub fn spawn_setup() -> Option<newt_tui::SetupHandle> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() || std::env::var_os("NEWT_NO_MODEL_PULL").is_some() {
        return None;
    }
    let m = palette::default_model();
    if palette::resolve_local(m.name).is_some() {
        return None; // already provisioned (GGUF + tokenizer)
    }
    let (gguf, tok) = (
        palette::local_gguf_path(m)?,
        palette::local_tokenizer_path(m)?,
    );
    let gguf_url = format!(
        "https://huggingface.co/{}/resolve/main/{}",
        m.hf_repo, m.gguf_file
    );
    let tok_url = format!(
        "https://huggingface.co/{}/resolve/main/tokenizer.json",
        m.tokenizer_repo
    );
    let (tx, rx) = std::sync::mpsc::channel();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cancel_thread = std::sync::Arc::clone(&cancel);
    // A std::thread with its OWN current-thread runtime, so the download is
    // independent of the main runtime that run_code blocks in — no nesting.
    std::thread::spawn(move || {
        run_setup_thread(&gguf_url, &gguf, &tok_url, &tok, &tx, &cancel_thread);
    });
    Some(newt_tui::SetupHandle {
        what: format!("on-host summarizer ({})", m.name),
        rx,
        cancel,
    })
}

/// Lean build: no embedded engine, so there is nothing to provision.
#[cfg(not(feature = "embedded"))]
pub fn spawn_setup() -> Option<newt_tui::SetupHandle> {
    None
}

/// The background provisioning body (runs on its own std::thread + runtime):
/// fetch the GGUF then `tokenizer.json`, emitting [`newt_tui::SetupEvent`]s for
/// the splash spinner. Each file is skipped if already present, so a GGUF-only
/// dir is completed in place. Honours `cancel` (triple-Esc from the splash).
#[cfg(feature = "embedded")]
fn run_setup_thread(
    gguf_url: &str,
    gguf: &Path,
    tok_url: &str,
    tok: &Path,
    tx: &std::sync::mpsc::Sender<newt_tui::SetupEvent>,
    cancel: &std::sync::atomic::AtomicBool,
) {
    use newt_tui::SetupEvent;
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = tx.send(SetupEvent::Failed(e.to_string()));
            return;
        }
    };
    rt.block_on(async {
        if let Some(parent) = gguf.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                let _ = tx.send(SetupEvent::Failed(e.to_string()));
                return;
            }
            write_models_readme(parent);
        }
        for (label, url, dest) in [("weights", gguf_url, gguf), ("tokenizer", tok_url, tok)] {
            let _ = tx.send(SetupEvent::Step(label.into()));
            if dest.is_file() {
                continue;
            }
            if let Err(e) = download_stream(url, dest, tx, cancel).await {
                let _ = tx.send(SetupEvent::Failed(e.to_string()));
                return;
            }
        }
        let _ = tx.send(SetupEvent::Done);
    });
}

/// Stream `url` to `dest` (via a `.part` file), emitting one progress event per
/// MB and aborting promptly if `cancel` is set. Atomic rename on success; the
/// partial file is removed on cancel.
#[cfg(feature = "embedded")]
async fn download_stream(
    url: &str,
    dest: &Path,
    tx: &std::sync::mpsc::Sender<newt_tui::SetupEvent>,
    cancel: &std::sync::atomic::AtomicBool,
) -> anyhow::Result<()> {
    use newt_tui::SetupEvent;
    use std::io::Write;
    use std::sync::atomic::Ordering;
    let mut resp = reqwest::Client::new()
        .get(url)
        .send()
        .await?
        .error_for_status()?;
    let total = resp.content_length();
    let part = dest.with_extension("part");
    let mut file = std::fs::File::create(&part)?;
    let mut got: u64 = 0;
    let mut last_mb: u64 = 0;
    let _ = tx.send(SetupEvent::Progress { done: 0, total });
    while let Some(chunk) = resp.chunk().await? {
        if cancel.load(Ordering::SeqCst) {
            drop(file);
            let _ = std::fs::remove_file(&part);
            anyhow::bail!("cancelled");
        }
        file.write_all(&chunk)?;
        got += chunk.len() as u64;
        // Throttle: one event per MB (the UI shows whole MB anyway).
        if got / 1_048_576 > last_mb {
            last_mb = got / 1_048_576;
            let _ = tx.send(SetupEvent::Progress { done: got, total });
        }
    }
    file.flush()?;
    std::fs::rename(&part, dest)?;
    Ok(())
}

/// Drop a README into `~/.newt/models/` explaining what these files are and why
/// they're on the CPU. Written once — never clobbers a user-edited one — by both
/// `newt models pull` and the first-run auto-provision.
fn write_models_readme(dir: &Path) {
    let readme = dir.join("README.md");
    if readme.exists() {
        return;
    }
    let _ = std::fs::write(&readme, MODELS_README);
}

const MODELS_README: &str = "\
# newt on-host summarizer models

These GGUF files are the on-host, CPU inference engine newt uses to summarize /
compact its OWN context mid-session, so context management never competes with
your GPU (the primary model) under load.

- Managed by `newt models` (`list` / `pull` / `path`).
- Default summarizer model: qwen2.5-0.5b (Q4_K_M, ~350 MB).
- Fetched from Hugging Face on the first interactive `newt code` run, or with
  `newt models pull`. Nothing else auto-downloads.
- Layout: <alias>/<file>.gguf
- Safe to delete: newt re-pulls on the next interactive run, or falls back to the
  session model (with a warning) until you re-pull.
- Skip the auto-pull with NEWT_NO_MODEL_PULL=1.

Why the CPU? Context compaction fires exactly when context is large — i.e. when
the GPU is busiest. Running the summarizer on the session GPU model there
overloads it and can stall the turn (#979). A small CPU model decouples them.

See docs/decisions/embedded_inference.md and issues #639 / #661 / #979.
";

#[cfg(test)]
mod d3c {
    use super::{models_table, ModelRow};

    fn rows() -> Vec<ModelRow> {
        vec![
            ModelRow {
                alias: "qwen2.5-coder".into(),
                ram_gb: 4.7,
                arch: "Qwen2".into(),
                installed: "yes",
                note: "default summarizer".into(),
            },
            ModelRow {
                alias: "llama3.1".into(),
                ram_gb: 12.0,
                arch: "Llama".into(),
                installed: "no",
                note: String::new(),
            },
        ]
    }

    /// **The byte golden for `newt models` as it ships today** (#1916).
    ///
    /// Captured from the shipping renderer, which is the correct method for a
    /// characterization golden: it pins what an operator sees NOW, so the
    /// migration onto `markup::table` has to declare its diff instead of
    /// sliding one past.
    #[test]
    fn the_models_listing_is_byte_exact() {
        assert_eq!(
            models_table(&rows()),
            concat!(
                "| alias         |   RAM | arch  | installed | note               |\n",
                "| ------------- | ----: | ----- | --------- | ------------------ |\n",
                "| qwen2.5-coder |  4.7G | Qwen2 | yes       | default summarizer |\n",
                "| llama3.1      | 12.0G | Llama | no        |                    |\n",
            )
        );
    }

    /// **AMENDED by the migration.** The fixed-width renderer padded the LAST
    /// column, so a model with no note left trailing spaces on its line. GFM
    /// closes every row with a pipe, so that whole class is gone — asserted as
    /// the new property rather than deleted, because "no line ends in
    /// whitespace" is worth keeping once it is true.
    #[test]
    fn no_row_ends_in_trailing_whitespace() {
        let out = models_table(&rows());
        for line in out.lines() {
            assert!(
                line.ends_with('|'),
                "every GFM row closes with a pipe: {line:?}"
            );
            assert_eq!(line.trim_end(), line, "no trailing whitespace: {line:?}");
        }
    }
}

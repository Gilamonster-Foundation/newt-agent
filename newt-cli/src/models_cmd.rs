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
}

pub async fn run(cmd: ModelsCmd) -> anyhow::Result<()> {
    match cmd {
        ModelsCmd::List => list(),
        ModelsCmd::Pull { alias } => pull(alias.as_deref()).await,
        ModelsCmd::Path { alias } => print_path(alias.as_deref()),
    }
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

fn list() -> anyhow::Result<()> {
    println!(
        "{:<16} {:>6}  {:<8} {:<9} note",
        "alias", "RAM", "arch", "installed"
    );
    for m in palette::palette() {
        let installed = if palette::resolve_local(m.name).is_some() {
            "yes"
        } else {
            "no"
        };
        println!(
            "{:<16} {:>5.1}G  {:<8} {:<9} {}",
            m.name,
            m.approx_ram_gb,
            format!("{:?}", m.arch),
            installed,
            m.note
        );
    }
    println!(
        "\nSummarizer default: {}   (fetch with: newt models pull [alias])",
        palette::default_model().name
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
    Ok(())
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

/// First-run provisioning of the on-host summarizer model (#661 group C).
///
/// Called at the start of an interactive `newt code` session. When the embedded
/// summarizer is the compiled default but its GGUF isn't on disk yet, this
/// prints a one-time `first pull` notice, fetches the default palette model to
/// `~/.newt/models/`, and drops a README. Best-effort: a failed pull (offline /
/// firewalled) leaves the model absent, so summarizer resolution falls back to
/// the warn-and-degrade (session-model) path — nothing here is fatal.
///
/// Silently no-ops unless ALL hold: built with the `embedded` feature, stdout is
/// a TTY (never auto-pull in a pipe / headless worker / CI), the opt-out env
/// `NEWT_NO_MODEL_PULL` is unset, and the model is absent.
pub async fn ensure_summarizer_model() {
    use std::io::IsTerminal;
    // Interactive only — a headless worker / piped / CI run must never pull
    // ~350 MB behind the operator's back. NEWT_NO_MODEL_PULL is the opt-out.
    let may_pull =
        std::io::stdout().is_terminal() && std::env::var_os("NEWT_NO_MODEL_PULL").is_none();
    #[cfg(feature = "embedded")]
    if may_pull {
        provision_default_model().await;
    }
    #[cfg(not(feature = "embedded"))]
    let _ = may_pull; // lean build: no embedded engine to provision for
}

/// The feature-on body of [`ensure_summarizer_model`]. Only compiled when the
/// embedded engine exists — a lean (`--no-default-features`) build has nothing to
/// run the model, so it never auto-pulls.
#[cfg(feature = "embedded")]
async fn provision_default_model() {
    let m = palette::default_model();
    // Already provisioned (every run after the first) → nothing to do.
    if palette::resolve_local(m.name).is_some() {
        return;
    }
    let (Some(gguf), Some(tok)) = (
        palette::local_gguf_path(m),
        palette::local_tokenizer_path(m),
    ) else {
        return; // no home dir — resolution will warn/degrade later
    };
    eprintln!(
        "first pull — setting up the on-host summarizer.\n\
         newt offloads context compaction from your GPU onto a small CPU model, so\n\
         summarizing never competes with the primary model under load. Fetching\n\
         {} to ~/.newt/models now (one time). Set NEWT_NO_MODEL_PULL=1 to skip.",
        m.name
    );
    if let Some(parent) = gguf.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!(
                "  first pull skipped (cannot create {}): {e}",
                parent.display()
            );
            return;
        }
        write_models_readme(parent);
    }
    let gguf_url = format!(
        "https://huggingface.co/{}/resolve/main/{}",
        m.hf_repo, m.gguf_file
    );
    let tok_url = format!(
        "https://huggingface.co/{}/resolve/main/tokenizer.json",
        m.tokenizer_repo
    );
    // The embedded engine needs BOTH weights and tokenizer; a failure on either
    // degrades to the session model (with a warning) rather than a mid-compaction
    // init error — which is what happened when only the GGUF was fetched.
    let ok = provision_one(&gguf_url, &gguf).await && provision_one(&tok_url, &tok).await;
    if ok {
        eprintln!(
            "  ready — context compaction now runs on the CPU ({}).",
            m.name
        );
    } else {
        eprintln!(
            "  first pull incomplete; the summarizer will use the session model\n  \
             (with a warning) until `newt models pull` succeeds."
        );
    }
}

/// One best-effort provisioning fetch for the auto-provision path (skip if
/// present). Errors are reported to stderr, not fatal — the caller degrades.
#[cfg(feature = "embedded")]
async fn provision_one(url: &str, dest: &Path) -> bool {
    if dest.is_file() {
        return true;
    }
    if let Err(e) = download_to(url, dest).await {
        eprintln!("  pull failed for {}: {e}", dest.display());
        return false;
    }
    true
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

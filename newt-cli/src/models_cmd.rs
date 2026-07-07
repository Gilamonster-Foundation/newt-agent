//! `newt models` — manage the local palette of mini models for the on-host
//! embedded summarizer (#661 group C).
//!
//! `pull` fetches a GGUF to `~/.newt/models/<alias>/`, `list` shows the palette
//! and what is installed, `path` prints the resolved local path. `pull` is the ONE
//! explicit fetch (nothing is auto-downloaded anywhere else), which is what lets
//! the embedded CPU summarizer be the default without a second backend or GPU.

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
    let dest = palette::local_gguf_path(m)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve ~/.newt/models (no home dir)"))?;
    if dest.is_file() {
        println!("{} already present at {}", m.name, dest.display());
        return Ok(());
    }
    // Standard HF GGUF layout: <repo>/resolve/main/<file>.
    let url = format!(
        "https://huggingface.co/{}/resolve/main/{}",
        m.hf_repo, m.gguf_file
    );
    println!(
        "Pulling {} (~{:.1} GB)\n  from {url}\n  to   {}",
        m.name,
        m.approx_ram_gb,
        dest.display()
    );
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    download_to(&url, &dest).await?;
    println!("OK installed {} -> {}", m.name, dest.display());
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

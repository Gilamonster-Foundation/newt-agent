// Copyright (c) 2025-2026 newt contributors. Licensed under Apache-2.0.

//! # newt-tuner binary
//!
//! CLI tool for managing model tuning configurations in ~/.newt/model_tuning/.

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "newt-tuner", version, about = "Model tuning configuration manager")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize the model_tuning directory with bundled defaults
    Init,
    /// Show the path to the model_tuning directory
    Path,
    /// List all available model configurations
    List,
    /// Get config for a specific model
    Get {
        /// Model name (e.g., "ornith", "llama3.1")
        model: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Init => {
            newt_tuner::init_model_tuning()?;
            println!("Initialized ~/.newt/model_tuning/ with bundled defaults");
        }
        Commands::Path => {
            let path = newt_tuner::model_tuning_dir()?;
            println!("{}", path.display());
        }
        Commands::List => {
            let dir = newt_tuner::model_tuning_dir()?;
            if !dir.exists() {
                println!("No model_tuning directory found. Run `newt-tuner init` first.");
                return Ok(());
            }
            
            let mut configs: Vec<_> = std::fs::read_dir(&dir)?
                .filter_map(|entry| entry.ok())
                .filter(|e| e.path().extension().map_or(false, |ext| ext == "toml"))
                .collect();
            
            configs.sort_by_key(|e| e.file_name());
            
            if configs.is_empty() {
                println!("No model configurations found.");
            } else {
                println!("Available models:");
                for entry in &configs {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let name = name.trim_end_matches(".toml").trim_end_matches(".default.toml");
                    println!("  - {}", name);
                }
            }
        }
        Commands::Get { model } => {
            match newt_tuner::load_model_config(&model) {
                Ok(config) => {
                    println!("{}", toml::to_string_pretty(&config)?);
                }
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        }
    }

    Ok(())
}

//! Interactive vLLM model switcher for dgx1.
//!
//! Manages a registry of vLLM models defined in `~/.newt/models-vllm.toml`,
//! provides an interactive menu to select and launch models, and orchestrates
//! Docker container lifecycle over SSH.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VllmModel {
    pub name: String,
    pub model_id: String,
    pub description: String,
    pub quantization: String,
    pub memory_gb: u32,
    pub max_model_len: u32,
    pub context_tokens: u32,
    #[serde(default)]
    pub special_args: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ModelsConfig {
    models: Vec<VllmModel>,
}

/// Manages vLLM model registry and switching.
pub struct VllmModelManager {
    config_path: String,
    pub models: Vec<VllmModel>,
    dgx_host: String,
}

impl VllmModelManager {
    /// Load models from `~/.newt/models-vllm.toml`.
    pub fn load(dgx_host: Option<String>) -> Result<Self> {
        let home = std::env::var("HOME")?;
        let config_path = format!("{}/.newt/models-vllm.toml", home);

        if !Path::new(&config_path).exists() {
            return Err(anyhow!(
                "vLLM models config not found at: {}\n\
                Create it with: cp models-vllm.toml ~/.newt/models-vllm.toml",
                config_path
            ));
        }

        let content = std::fs::read_to_string(&config_path)?;
        let config: ModelsConfig = toml::from_str(&content)?;

        if config.models.is_empty() {
            return Err(anyhow!("No models defined in {}", config_path));
        }

        Ok(VllmModelManager {
            config_path,
            models: config.models,
            dgx_host: dgx_host.unwrap_or_else(|| "hartsock@dgx1.home.lab".to_string()),
        })
    }

    /// Display all models in a formatted table.
    pub fn list(&self) {
        println!("\n📦 Available vLLM Models\n");
        println!(
            "{:<3} {:<30} {:<10} {:<20} {:<30}",
            "ID", "Model", "Memory", "Quantization", "Description"
        );
        println!("{:-<93}", "");

        for (idx, model) in self.models.iter().enumerate() {
            println!(
                "{:<3} {:<30} {:<10} {:<20} {:<30}",
                idx + 1,
                truncate(&model.name, 28),
                format!("{}GB", model.memory_gb),
                model.quantization,
                truncate(&model.description, 28)
            );
        }
        println!();
    }

    /// Show interactive menu to select a model.
    pub fn select_interactive(&self) -> Result<VllmModel> {
        if self.models.is_empty() {
            return Err(anyhow!("No models available to select"));
        }

        println!("\n🎯 Select vLLM Model:\n");

        for (idx, model) in self.models.iter().enumerate() {
            println!(
                "  {}. {} ({} GB, {})",
                idx + 1, model.name, model.memory_gb, model.quantization
            );
            println!("     {}", model.description);
        }

        println!("\n? Choose model number (1-{}): ", self.models.len());

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;

        let choice: usize = input
            .trim()
            .parse()
            .map_err(|_| anyhow!("Invalid selection"))?;

        if choice < 1 || choice > self.models.len() {
            return Err(anyhow!("Selection out of range"));
        }

        Ok(self.models[choice - 1].clone())
    }

    /// Launch a model on dgx1 via SSH.
    pub async fn launch(&self, model: &VllmModel) -> Result<()> {
        println!(
            "\n🚀 Launching {} on {}...\n",
            model.name, self.dgx_host
        );

        // Stop existing container
        println!("⏹  Stopping existing vllm-server...");
        let _ = Command::new("ssh")
            .args(&[
                &self.dgx_host,
                "sudo docker stop vllm-server 2>/dev/null || true; \
                 sudo docker rm vllm-server 2>/dev/null || true",
            ])
            .output();

        // Build and execute launch script
        let script = self.build_launch_script(model);
        let output = Command::new("ssh")
            .arg(&self.dgx_host)
            .arg(&script)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!(
                "Failed to launch model on {}: {}",
                self.dgx_host,
                stderr
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        if !stdout.is_empty() {
            println!("✅ Container started: {}", stdout.trim());
        }

        println!("⏳ Waiting for model to load...");
        self.wait_for_ready(120).await?;

        println!("\n🎉 {} is ready!", model.name);
        println!("   Endpoint: http://{}:8000/v1", self.dgx_host);
        println!("   Model ID: {}\n", model.model_id);

        Ok(())
    }

    /// Generate complete launch script for SSH execution.
    fn build_launch_script(&self, model: &VllmModel) -> String {
        let mut args = vec![
            "vllm serve".to_string(),
            model.model_id.clone(),
            "--host 0.0.0.0".to_string(),
            "--port 8000".to_string(),
            "--trust-remote-code".to_string(),
            format!("--max-model-len {}", model.max_model_len),
        ];

        args.extend(model.special_args.clone());

        let vllm_cmd = args.join(" ");

        format!(
            r#"sudo docker run -d \
  --gpus all \
  --name vllm-server \
  --restart unless-stopped \
  --shm-size 32g \
  -p 8000:8000 \
  -v ~/.cache/huggingface:/root/.cache/huggingface \
  --ipc=host \
  vllm/vllm-openai:latest \
  /bin/sh -c "{}" "#,
            vllm_cmd
        )
    }

    /// Poll API endpoint until model is ready.
    async fn wait_for_ready(&self, timeout_secs: u32) -> Result<()> {
        let check_cmd = "until curl -s http://localhost:8000/v1/models | grep -q 'data' 2>/dev/null; do sleep 2; done";

        let output = Command::new("timeout")
            .args(&[
                &timeout_secs.to_string(),
                "ssh",
                &self.dgx_host,
                check_cmd,
            ])
            .output()?;

        if !output.status.success() {
            return Err(anyhow!(
                "Model did not become ready within {} seconds.\n\
                Check logs with: ssh {} sudo docker logs vllm-server",
                timeout_secs, self.dgx_host
            ));
        }

        Ok(())
    }

    /// Show current vLLM server status.
    pub fn status(&self) -> Result<()> {
        println!("\n📊 vLLM Server Status on {}\n", self.dgx_host);

        let output = Command::new("ssh")
            .args(&[
                &self.dgx_host,
                "sudo docker ps --filter name=vllm-server --format 'table {{.Names}}\\t{{.Status}}' || echo 'No vllm-server'",
            ])
            .output()?;

        println!("Container:");
        println!("{}", String::from_utf8_lossy(&output.stdout));

        let api_output = Command::new("curl")
            .args(&["-s", &format!("http://{}:8000/v1/models", self.dgx_host)])
            .output();

        if let Ok(output) = api_output {
            if output.status.success() {
                if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
                    if let Some(data) = json.get("data") {
                        if let Some(first) = data.get(0) {
                            if let Some(model_id) = first.get("id") {
                                println!("\n✅ API responding");
                                println!("Current model: {}\n", model_id);
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }

        println!("API not responding\n");
        Ok(())
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() > n {
        format!("{}...", &s[..n.saturating_sub(3)])
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("this is a long string", 10), "this is...");
    }

    #[test]
    fn test_model_deserialization() {
        let toml_str = r#"
[[models]]
name = "Test Model"
model_id = "test/model"
description = "A test"
quantization = "fp8"
memory_gb = 40
max_model_len = 4096
context_tokens = 4096
special_args = ["--flag", "value"]
        "#;

        let config: ModelsConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.models.len(), 1);
        assert_eq!(config.models[0].name, "Test Model");
        assert_eq!(config.models[0].special_args.len(), 2);
    }

    #[test]
    fn test_launch_script_generation() {
        let manager = VllmModelManager {
            config_path: "test".to_string(),
            models: vec![],
            dgx_host: "test@host".to_string(),
        };

        let model = VllmModel {
            name: "Test".to_string(),
            model_id: "test/model".to_string(),
            description: "Test".to_string(),
            quantization: "fp8".to_string(),
            memory_gb: 40,
            max_model_len: 4096,
            context_tokens: 4096,
            special_args: vec!["--flag".to_string(), "value".to_string()],
            tags: vec![],
        };

        let script = manager.build_launch_script(&model);
        assert!(script.contains("vllm serve test/model"));
        assert!(script.contains("--max-model-len 4096"));
        assert!(script.contains("--flag value"));
        assert!(script.contains("docker run"));
    }
}

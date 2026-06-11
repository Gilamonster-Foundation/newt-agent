use async_trait::async_trait;
use clap::Parser;
use newt_provider_openai::OpenAiClient;
use plugins_protocol::{
    CompleteRequest, CompleteResponse, InitializeRequest, InitializeResponse, ListModelsResponse,
    PluginHandler, PluginServer, PROTOCOL_VERSION,
};

#[derive(Parser, Debug)]
#[command(
    name = "newt-provider-openai",
    version,
    about = "Opt-in OpenAI provider plugin for Newt-Agent"
)]
struct Cli {}

struct OpenAiProvider;

#[async_trait]
impl PluginHandler for OpenAiProvider {
    async fn initialize(
        &self,
        req: InitializeRequest,
    ) -> plugins_protocol::Result<InitializeResponse> {
        if req.protocol_version != PROTOCOL_VERSION {
            return Err(plugins_protocol::Error::Protocol(format!(
                "unsupported provider protocol version {}",
                req.protocol_version
            )));
        }
        Ok(InitializeResponse {
            plugin_name: "newt-provider-openai".to_string(),
            plugin_version: env!("CARGO_PKG_VERSION").to_string(),
            supported_models: Vec::new(),
        })
    }

    async fn list_models(&self) -> plugins_protocol::Result<ListModelsResponse> {
        OpenAiClient::from_env()
            .list_models()
            .await
            .map_err(|e| plugins_protocol::Error::Protocol(e.to_string()))
    }

    async fn complete(&self, req: CompleteRequest) -> plugins_protocol::Result<CompleteResponse> {
        OpenAiClient::from_env()
            .complete(req)
            .await
            .map_err(|e| plugins_protocol::Error::Protocol(e.to_string()))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _cli = Cli::parse();
    PluginServer::new(OpenAiProvider).run_stdio().await?;
    Ok(())
}

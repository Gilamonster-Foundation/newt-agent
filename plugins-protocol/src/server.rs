use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use crate::Result;
use crate::{
    CompleteRequest, CompleteResponse, InitializeRequest, InitializeResponse, ListModelsResponse,
    RpcErrorObject,
};

#[async_trait]
pub trait PluginHandler: Send + Sync {
    async fn initialize(&self, req: InitializeRequest) -> Result<InitializeResponse>;
    async fn list_models(&self) -> Result<ListModelsResponse>;
    async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse>;

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

pub struct PluginServer<H> {
    handler: H,
}

impl<H> PluginServer<H>
where
    H: PluginHandler,
{
    pub fn new(handler: H) -> Self {
        Self { handler }
    }

    pub async fn run_stdio(self) -> Result<()> {
        let stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();
        self.run(stdin, &mut stdout).await
    }

    pub async fn run<R, W>(&self, reader: R, writer: &mut W) -> Result<()>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut lines = BufReader::new(reader).lines();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }

            let request: std::result::Result<RpcRequest, serde_json::Error> =
                serde_json::from_str(&line);
            let request = match request {
                Ok(request) => request,
                Err(e) => {
                    write_response(
                        writer,
                        &RpcResponse::error(None, -32700, format!("parse error: {e}")),
                    )
                    .await?;
                    continue;
                }
            };

            let id = request.id;
            let mut should_shutdown = false;
            let response = match request.method.as_str() {
                "initialize" => match serde_json::from_value::<InitializeRequest>(request.params) {
                    Ok(req) => match self.handler.initialize(req).await {
                        Ok(result) => RpcResponse::ok(id, serde_json::to_value(result)?),
                        Err(e) => RpcResponse::error(id, -32000, e.to_string()),
                    },
                    Err(e) => RpcResponse::error(id, -32602, e.to_string()),
                },
                "list_models" => match self.handler.list_models().await {
                    Ok(result) => RpcResponse::ok(id, serde_json::to_value(result)?),
                    Err(e) => RpcResponse::error(id, -32000, e.to_string()),
                },
                "complete" => match serde_json::from_value::<CompleteRequest>(request.params) {
                    Ok(req) => match self.handler.complete(req).await {
                        Ok(result) => RpcResponse::ok(id, serde_json::to_value(result)?),
                        Err(e) => RpcResponse::error(id, -32000, e.to_string()),
                    },
                    Err(e) => RpcResponse::error(id, -32602, e.to_string()),
                },
                "stream" => RpcResponse::error(id, -32601, "stream is not supported".to_string()),
                "shutdown" => {
                    should_shutdown = true;
                    match self.handler.shutdown().await {
                        Ok(()) => RpcResponse::ok(id, serde_json::json!({})),
                        Err(e) => RpcResponse::error(id, -32000, e.to_string()),
                    }
                }
                other => RpcResponse::error(id, -32601, format!("unknown method: {other}")),
            };
            write_response(writer, &response).await?;
            if should_shutdown {
                break;
            }
        }
        Ok(())
    }
}

async fn write_response<W>(writer: &mut W, response: &RpcResponse) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut bytes = serde_json::to_vec(response)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

#[derive(Debug, serde::Deserialize)]
struct RpcRequest {
    id: Option<u64>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, serde::Serialize)]
struct RpcResponse {
    jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcErrorObject>,
}

impl RpcResponse {
    fn ok(id: Option<u64>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    fn error(id: Option<u64>, code: i64, message: String) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcErrorObject { code, message }),
        }
    }
}

//! Streaming reply types for incremental inference responses.

use std::pin::Pin;

use futures_core::Stream;
use serde::{Deserialize, Serialize};

use crate::backend::ChatReply;

/// A single chunk from a streaming inference response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatChunk {
    /// The incremental text content of this chunk.
    pub delta: String,
    /// The model that produced this chunk.
    pub model_id: String,
    /// Whether this is the final chunk in the stream.
    pub is_final: bool,
}

/// A stream of chat chunks from a backend.
pub type ChatStream = Pin<Box<dyn Stream<Item = anyhow::Result<ChatChunk>> + Send>>;

/// Collect a stream of [`ChatChunk`]s into a single [`ChatReply`] by
/// concatenating all deltas.
pub async fn collect_stream(stream: ChatStream) -> anyhow::Result<ChatReply> {
    use tokio_stream::StreamExt;

    let mut content = String::new();
    let mut model_id = String::new();

    tokio::pin!(stream);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        content.push_str(&chunk.delta);
        if !chunk.model_id.is_empty() {
            model_id.clone_from(&chunk.model_id);
        }
    }

    Ok(ChatReply {
        content,
        model_id,
        usage: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_serde_roundtrip() {
        let chunk = ChatChunk {
            delta: "hello".to_string(),
            model_id: "test-model".to_string(),
            is_final: false,
        };
        let json = serde_json::to_string(&chunk).unwrap();
        let back: ChatChunk = serde_json::from_str(&json).unwrap();
        assert_eq!(back.delta, "hello");
        assert_eq!(back.model_id, "test-model");
        assert!(!back.is_final);
    }

    #[tokio::test]
    async fn collect_empty_stream() {
        let stream: ChatStream = Box::pin(tokio_stream::empty());
        let reply = collect_stream(stream).await.unwrap();
        assert_eq!(reply.content, "");
        assert_eq!(reply.model_id, "");
    }

    #[tokio::test]
    async fn collect_single_chunk() {
        let chunks = vec![Ok(ChatChunk {
            delta: "hello world".to_string(),
            model_id: "m1".to_string(),
            is_final: true,
        })];
        let stream: ChatStream = Box::pin(tokio_stream::iter(chunks));
        let reply = collect_stream(stream).await.unwrap();
        assert_eq!(reply.content, "hello world");
        assert_eq!(reply.model_id, "m1");
    }

    #[tokio::test]
    async fn collect_multiple_chunks_concatenates() {
        let chunks = vec![
            Ok(ChatChunk {
                delta: "hello ".to_string(),
                model_id: "m1".to_string(),
                is_final: false,
            }),
            Ok(ChatChunk {
                delta: "world".to_string(),
                model_id: "m1".to_string(),
                is_final: true,
            }),
        ];
        let stream: ChatStream = Box::pin(tokio_stream::iter(chunks));
        let reply = collect_stream(stream).await.unwrap();
        assert_eq!(reply.content, "hello world");
    }

    #[tokio::test]
    async fn collect_stream_propagates_error() {
        let chunks: Vec<anyhow::Result<ChatChunk>> = vec![
            Ok(ChatChunk {
                delta: "start".to_string(),
                model_id: "m1".to_string(),
                is_final: false,
            }),
            Err(anyhow::anyhow!("connection lost")),
        ];
        let stream: ChatStream = Box::pin(tokio_stream::iter(chunks));
        let result = collect_stream(stream).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("connection lost"));
    }

    #[tokio::test]
    async fn collect_stream_uses_last_nonempty_model_id() {
        let chunks = vec![
            Ok(ChatChunk {
                delta: "a".to_string(),
                model_id: "m1".to_string(),
                is_final: false,
            }),
            Ok(ChatChunk {
                delta: "b".to_string(),
                model_id: "".to_string(),
                is_final: false,
            }),
            Ok(ChatChunk {
                delta: "c".to_string(),
                model_id: "m2".to_string(),
                is_final: true,
            }),
        ];
        let stream: ChatStream = Box::pin(tokio_stream::iter(chunks));
        let reply = collect_stream(stream).await.unwrap();
        assert_eq!(reply.content, "abc");
        assert_eq!(reply.model_id, "m2");
    }
}

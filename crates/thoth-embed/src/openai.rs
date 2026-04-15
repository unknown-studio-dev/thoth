//! OpenAI embedder — https://platform.openai.com/docs/api-reference/embeddings
//!
//! Supports any `text-embedding-*` model. We default to
//! `text-embedding-3-small` (1536-dim) which is a good balance of cost and
//! quality for code-adjacent text.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thoth_core::{Embedder, Error, Result};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// Handle to the OpenAI embeddings endpoint.
#[derive(Debug, Clone)]
pub struct OpenAiEmbedder {
    client: reqwest::Client,
    api_key: String,
    model: String,
    dim: usize,
    base_url: String,
}

impl OpenAiEmbedder {
    /// OpenAI's documented per-request input cap.
    pub const MAX_BATCH: usize = 2048;

    /// Construct from key / model / dimensionality.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>, dim: usize) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            model: model.into(),
            dim,
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }

    /// Sugar: `text-embedding-3-small` (1536-dim).
    pub fn text_embedding_3_small(api_key: impl Into<String>) -> Self {
        Self::new(api_key, "text-embedding-3-small", 1536)
    }

    /// Sugar: `text-embedding-3-large` (3072-dim).
    pub fn text_embedding_3_large(api_key: impl Into<String>) -> Self {
        Self::new(api_key, "text-embedding-3-large", 3072)
    }

    /// Read `OPENAI_API_KEY` and build a `text-embedding-3-small` client.
    pub fn from_env() -> Result<Self> {
        let key = std::env::var("OPENAI_API_KEY")
            .map_err(|_| Error::Config("OPENAI_API_KEY not set".to_string()))?;
        if key.trim().is_empty() {
            return Err(Error::Config("OPENAI_API_KEY is empty".to_string()));
        }
        Ok(Self::text_embedding_3_small(key))
    }

    /// Override the base URL.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }
}

#[async_trait]
impl Embedder for OpenAiEmbedder {
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(texts.len());
        for batch in texts.chunks(Self::MAX_BATCH) {
            let req = EmbedRequest {
                input: batch.iter().map(|s| s.to_string()).collect(),
                model: &self.model,
            };
            let url = format!("{}/embeddings", self.base_url);
            let resp = self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&req)
                .send()
                .await
                .map_err(provider)?;
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(Error::Provider(format!("openai {status}: {body}")));
            }
            let body: EmbedResponse = resp.json().await.map_err(provider)?;
            let mut items = body.data;
            items.sort_by_key(|d| d.index);
            for item in items {
                out.push(item.embedding);
            }
        }
        if out.len() != texts.len() {
            return Err(Error::Provider(format!(
                "openai: expected {} embeddings, got {}",
                texts.len(),
                out.len()
            )));
        }
        Ok(out)
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn model_id(&self) -> &str {
        &self.model
    }
}

// ---- wire types ------------------------------------------------------------

#[derive(Serialize)]
struct EmbedRequest<'a> {
    input: Vec<String>,
    model: &'a str,
}

#[derive(Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedItem>,
}

#[derive(Deserialize)]
struct EmbedItem {
    index: usize,
    embedding: Vec<f32>,
}

fn provider(e: impl std::fmt::Display) -> Error {
    Error::Provider(e.to_string())
}

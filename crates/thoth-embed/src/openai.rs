//! OpenAI embedder — https://platform.openai.com/docs/api-reference/embeddings
//!
//! Supports any `text-embedding-*` model. We default to
//! `text-embedding-3-small` (1536-dim) which is a good balance of cost and
//! quality for code-adjacent text.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thoth_core::{Embedder, Error, Result};

use crate::http::HttpEmbedderBase;

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const PROVIDER: &str = "openai";

/// Handle to the OpenAI embeddings endpoint.
#[derive(Debug, Clone)]
pub struct OpenAiEmbedder {
    base: HttpEmbedderBase,
}

impl OpenAiEmbedder {
    /// OpenAI's documented per-request input cap.
    pub const MAX_BATCH: usize = 2048;

    /// Construct from key / model / dimensionality.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>, dim: usize) -> Self {
        Self {
            base: HttpEmbedderBase::new(api_key, model, dim, DEFAULT_BASE_URL),
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
        Ok(Self::text_embedding_3_small(
            HttpEmbedderBase::key_from_env("OPENAI_API_KEY")?,
        ))
    }

    /// Override the base URL.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base.base_url = url.into();
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
                model: &self.base.model,
            };
            let body: EmbedResponse = self.base.post_json("/embeddings", &req, PROVIDER).await?;
            let mut items = body.data;
            items.sort_by_key(|d| d.index);
            for item in items {
                out.push(item.embedding);
            }
        }
        if out.len() != texts.len() {
            return Err(Error::Provider(format!(
                "{PROVIDER}: expected {} embeddings, got {}",
                texts.len(),
                out.len()
            )));
        }
        Ok(out)
    }

    fn dim(&self) -> usize {
        self.base.dim
    }

    fn model_id(&self) -> &str {
        &self.base.model
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

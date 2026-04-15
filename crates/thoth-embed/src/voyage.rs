//! Voyage AI embedder — https://docs.voyageai.com/reference/embeddings-api
//!
//! The Voyage endpoint takes up to 128 inputs per call and returns one float
//! vector per input. We expose a batched [`Embedder`] impl that chunks large
//! input arrays into batches of [`Self::MAX_BATCH`] and concatenates the
//! results back in order.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thoth_core::{Embedder, Error, Result};

const DEFAULT_BASE_URL: &str = "https://api.voyageai.com/v1";

/// Handle to the Voyage embeddings API.
#[derive(Debug, Clone)]
pub struct VoyageEmbedder {
    client: reqwest::Client,
    api_key: String,
    model: String,
    dim: usize,
    base_url: String,
}

impl VoyageEmbedder {
    /// Voyage's documented maximum inputs per request.
    pub const MAX_BATCH: usize = 128;

    /// Construct from an API key and a model identifier. Dimensionality is
    /// fixed per model — for `voyage-code-3` it's 1024; pass the right value
    /// for your model of choice.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>, dim: usize) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            model: model.into(),
            dim,
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }

    /// Sugar: the `voyage-code-3` model (1024-dim, optimised for code).
    pub fn voyage_code_3(api_key: impl Into<String>) -> Self {
        Self::new(api_key, "voyage-code-3", 1024)
    }

    /// Read `VOYAGE_API_KEY` from the environment and build a `voyage-code-3`
    /// client. Returns `Err(Error::Config)` if the var is unset or empty.
    pub fn from_env() -> Result<Self> {
        let key = std::env::var("VOYAGE_API_KEY")
            .map_err(|_| Error::Config("VOYAGE_API_KEY not set".to_string()))?;
        if key.trim().is_empty() {
            return Err(Error::Config("VOYAGE_API_KEY is empty".to_string()));
        }
        Ok(Self::voyage_code_3(key))
    }

    /// Override the base URL (for tests or self-hosted proxies).
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }
}

#[async_trait]
impl Embedder for VoyageEmbedder {
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(texts.len());
        for batch in texts.chunks(Self::MAX_BATCH) {
            let req = EmbedRequest {
                input: batch.iter().map(|s| s.to_string()).collect(),
                model: &self.model,
                input_type: "document",
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
                return Err(Error::Provider(format!("voyage {status}: {body}")));
            }
            let body: EmbedResponse = resp.json().await.map_err(provider)?;
            // The API returns items unordered by index; restore deterministic order.
            let mut items = body.data;
            items.sort_by_key(|d| d.index);
            for item in items {
                out.push(item.embedding);
            }
        }
        if out.len() != texts.len() {
            return Err(Error::Provider(format!(
                "voyage: expected {} embeddings, got {}",
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
    input_type: &'a str,
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

//! Cohere embedder — https://docs.cohere.com/reference/embed
//!
//! Uses Cohere's v2 `/embed` endpoint. `embed-english-v3.0` is 1024-dim and
//! works well for mixed code + natural language.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thoth_core::{Embedder, Error, Result};

const DEFAULT_BASE_URL: &str = "https://api.cohere.com/v2";

/// Handle to the Cohere embeddings endpoint.
#[derive(Debug, Clone)]
pub struct CohereEmbedder {
    client: reqwest::Client,
    api_key: String,
    model: String,
    dim: usize,
    base_url: String,
}

impl CohereEmbedder {
    /// Cohere's documented per-request input cap.
    pub const MAX_BATCH: usize = 96;

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

    /// Sugar: `embed-english-v3.0` (1024-dim).
    pub fn embed_english_v3(api_key: impl Into<String>) -> Self {
        Self::new(api_key, "embed-english-v3.0", 1024)
    }

    /// Read `COHERE_API_KEY` and build an `embed-english-v3.0` client.
    pub fn from_env() -> Result<Self> {
        let key = std::env::var("COHERE_API_KEY")
            .map_err(|_| Error::Config("COHERE_API_KEY not set".to_string()))?;
        if key.trim().is_empty() {
            return Err(Error::Config("COHERE_API_KEY is empty".to_string()));
        }
        Ok(Self::embed_english_v3(key))
    }

    /// Override the base URL.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }
}

#[async_trait]
impl Embedder for CohereEmbedder {
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(texts.len());
        for batch in texts.chunks(Self::MAX_BATCH) {
            let req = EmbedRequest {
                model: &self.model,
                texts: batch.iter().map(|s| s.to_string()).collect(),
                input_type: "search_document",
                embedding_types: vec!["float"],
            };
            let url = format!("{}/embed", self.base_url);
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
                return Err(Error::Provider(format!("cohere {status}: {body}")));
            }
            let body: EmbedResponse = resp.json().await.map_err(provider)?;
            // Cohere preserves input order in the `float` array.
            for v in body.embeddings.float {
                out.push(v);
            }
        }
        if out.len() != texts.len() {
            return Err(Error::Provider(format!(
                "cohere: expected {} embeddings, got {}",
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
    model: &'a str,
    texts: Vec<String>,
    input_type: &'a str,
    embedding_types: Vec<&'a str>,
}

#[derive(Deserialize)]
struct EmbedResponse {
    embeddings: EmbedBuckets,
}

#[derive(Deserialize)]
struct EmbedBuckets {
    #[serde(default)]
    float: Vec<Vec<f32>>,
}

fn provider(e: impl std::fmt::Display) -> Error {
    Error::Provider(e.to_string())
}

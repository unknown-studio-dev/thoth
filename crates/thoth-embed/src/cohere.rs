//! Cohere embedder — https://docs.cohere.com/reference/embed
//!
//! Uses Cohere's v2 `/embed` endpoint. `embed-english-v3.0` is 1024-dim and
//! works well for mixed code + natural language.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thoth_core::{Embedder, Error, Result};

use crate::http::HttpEmbedderBase;

const DEFAULT_BASE_URL: &str = "https://api.cohere.com/v2";
const PROVIDER: &str = "cohere";

/// Handle to the Cohere embeddings endpoint.
#[derive(Debug, Clone)]
pub struct CohereEmbedder {
    base: HttpEmbedderBase,
}

impl CohereEmbedder {
    /// Cohere's documented per-request input cap.
    pub const MAX_BATCH: usize = 96;

    /// Construct from key / model / dimensionality.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>, dim: usize) -> Self {
        Self {
            base: HttpEmbedderBase::new(api_key, model, dim, DEFAULT_BASE_URL),
        }
    }

    /// Sugar: `embed-english-v3.0` (1024-dim).
    pub fn embed_english_v3(api_key: impl Into<String>) -> Self {
        Self::new(api_key, "embed-english-v3.0", 1024)
    }

    /// Read `COHERE_API_KEY` and build an `embed-english-v3.0` client.
    pub fn from_env() -> Result<Self> {
        Ok(Self::embed_english_v3(HttpEmbedderBase::key_from_env(
            "COHERE_API_KEY",
        )?))
    }

    /// Override the base URL.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base.base_url = url.into();
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
                model: &self.base.model,
                texts: batch.iter().map(|s| s.to_string()).collect(),
                input_type: "search_document",
                embedding_types: vec!["float"],
            };
            let body: EmbedResponse = self.base.post_json("/embed", &req, PROVIDER).await?;
            // Cohere preserves input order in the `float` array.
            for v in body.embeddings.float {
                out.push(v);
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

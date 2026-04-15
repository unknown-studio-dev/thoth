//! Pluggable provider traits: embedding and LLM synthesis.
//!
//! Thoth never depends on a specific provider SDK. Callers plug in an
//! implementation of [`Embedder`] and/or [`Synthesizer`] when they want
//! Mode::Full behaviour.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::event::Outcome;
use crate::memory::Lesson;
use crate::query::Chunk;

/// Semantic embedding provider.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Embed a batch of texts. The returned vector has length `texts.len()`,
    /// each inner vec has length [`Embedder::dim`].
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;

    /// The output dimensionality of this embedder.
    fn dim(&self) -> usize;

    /// Stable identifier of the underlying model (e.g. `"voyage-code-3"`).
    fn model_id(&self) -> &str;
}

/// LLM synthesis provider — used for answer synthesis, query rewriting,
/// and self-critique.
#[async_trait]
pub trait Synthesizer: Send + Sync {
    /// Synthesize a natural-language answer from retrieved context.
    async fn synthesize(&self, prompt: &Prompt) -> Result<Synthesis>;

    /// Reflect on an outcome and optionally propose a `Lesson` to persist.
    async fn critique(&self, outcome: &Outcome) -> Result<Option<Lesson>>;

    /// Stable identifier of the underlying model.
    fn model_id(&self) -> &str;
}

/// A synthesis prompt carrying retrieved context plus the user's question.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prompt {
    /// The user's question.
    pub question: String,
    /// Retrieved chunks (already reranked).
    pub chunks: Vec<Chunk>,
    /// Any lessons to inject at the top of the prompt.
    pub lessons: Vec<Lesson>,
    /// Hard token budget for the synthesized answer.
    pub max_tokens: Option<u32>,
}

/// Result of a synthesis call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Synthesis {
    /// Natural-language answer.
    pub answer: String,
    /// Chunks the model chose to cite (by id).
    pub citations: Vec<String>,
    /// Rough token usage, if the provider reports it.
    pub tokens_used: Option<u32>,
}

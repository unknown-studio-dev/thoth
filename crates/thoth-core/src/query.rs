//! Query and retrieval types.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// What the caller wants Thoth to find.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Query {
    /// The natural-language or keyword text of the query.
    pub text: String,
    /// Optional scope filters.
    #[serde(default)]
    pub scope: QueryScope,
    /// How many chunks to return at most.
    #[serde(default = "default_top_k")]
    pub top_k: usize,
}

fn default_top_k() -> usize {
    12
}

/// Filters constraining the retrieval.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueryScope {
    /// Restrict to these path prefixes.
    #[serde(default)]
    pub paths: Vec<PathBuf>,
    /// Restrict to these languages (tree-sitter names).
    #[serde(default)]
    pub languages: Vec<String>,
    /// Restrict to these symbol names.
    #[serde(default)]
    pub symbols: Vec<String>,
}

impl Query {
    /// Shorthand for building a text-only query with default settings.
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            text: s.into(),
            scope: QueryScope::default(),
            top_k: default_top_k(),
        }
    }
}

/// The result of a recall call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Retrieval {
    /// Ranked chunks.
    pub chunks: Vec<Chunk>,
    /// Optional LLM-synthesized answer (Mode::Full only).
    pub synthesized: Option<String>,
    /// The correlation id to record outcomes against later.
    pub correlation_id: uuid::Uuid,
}

/// A single retrieved piece of context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    /// Stable id (content hash).
    pub id: String,
    /// Path the chunk originated from.
    pub path: PathBuf,
    /// 1-based starting line.
    pub line: u32,
    /// Line span `(start, end)`.
    pub span: (u32, u32),
    /// Enclosing symbol (e.g. function/class name), if known.
    pub symbol: Option<String>,
    /// Short human-readable preview (trimmed to ~1–3 lines).
    pub preview: String,
    /// Full chunk body.
    pub body: String,
    /// Retrieval score (higher is better).
    pub score: f32,
    /// How the chunk was found.
    pub source: RetrievalSource,
}

/// Provenance of a retrieved chunk.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalSource {
    /// Exact symbol lookup.
    Symbol,
    /// Graph traversal (callers, callees, imports, ...).
    Graph,
    /// Full-text / BM25 match via tantivy.
    FullText,
    /// Vector similarity via LanceDB (Mode::Full only).
    Vector,
    /// Text from a markdown memory file (MEMORY.md / LESSONS.md).
    Markdown,
}

/// A citation pointing at a chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Citation {
    /// Chunk id.
    pub chunk_id: String,
    /// Path.
    pub path: PathBuf,
    /// Starting line.
    pub line: u32,
}

//! # thoth-retrieve
//!
//! Retrieval orchestrator. Given a [`Query`] and a [`Mode`], it fans out to
//! the relevant stores, fuses the results with Reciprocal Rank Fusion, and
//! returns a [`Retrieval`].
//!
//! Pipeline (see `DESIGN.md` §4):
//!
//! ```text
//! Query → { symbol | graph | BM25 | markdown | vector (Mode::Full) }
//!       → RRF fuse
//!       → (Mode::Full) Synthesizer::synthesize
//!       → Retrieval
//! ```
//!
//! This crate also hosts the [`Indexer`], which walks a source tree and
//! populates every backend behind a [`StoreRoot`]. The retriever assumes an
//! indexer has already run.

#![deny(rust_2018_idioms)]
#![warn(missing_docs)]

pub mod config;
pub mod enrich;
pub mod indexer;
pub mod retriever;

pub use config::{IndexConfig, OutputConfig};
pub use enrich::{enrich_chunks, extract_docstring};
pub use indexer::{IndexProgress, IndexStats, Indexer, chunk_id, read_span};
pub use retriever::Retriever;

use std::sync::Arc;

use thoth_core::{Embedder, Mode, Query, Result, Retrieval, Synthesizer};
use thoth_store::{StoreRoot, VectorStore};

/// Convenience wrapper: opens the right extra backends for the requested
/// [`Mode`] and runs a single recall.
///
/// In Mode::Zero the synthesizer and vector stages are skipped. In Mode::Full
/// the vector store is opened at `<root>/vectors.db` (per DESIGN §7) and
/// the caller-supplied embedder / synthesizer are plugged into the
/// retriever.
pub async fn recall(store: StoreRoot, q: Query, mode: Mode) -> Result<Retrieval> {
    match mode {
        Mode::Zero => Retriever::new(store).recall(&q).await,
        Mode::Full {
            embedder,
            synthesizer,
        } => {
            let vectors_path = StoreRoot::vectors_path(&store.path);
            let vectors = VectorStore::open(&vectors_path).await?;
            let embedder: Option<Arc<dyn Embedder>> = embedder.map(Arc::from);
            let synth: Option<Arc<dyn Synthesizer>> = synthesizer.map(Arc::from);
            Retriever::with_full(store, Some(vectors), embedder, synth)
                .recall_full(&q)
                .await
        }
    }
}

//! Operating mode for Thoth.
//!
//! Thoth runs in one of two modes (see `DESIGN.md` §6).

use crate::provider::{Embedder, Synthesizer};

/// Operating mode.
///
/// `Mode::Zero` is fully offline and requires no API key.
/// `Mode::Full` accepts an optional `Embedder` (for semantic search)
/// and an optional `Synthesizer` (for LLM-curated memory and answer synthesis).
/// Either, both, or neither may be supplied in `Mode::Full`.
pub enum Mode {
    /// Offline-only. Symbol lookup + graph traversal + BM25 + markdown grep.
    Zero,

    /// Plug-in mode. Supply an embedder and/or a synthesizer.
    Full {
        /// Semantic embedding provider. If `None`, falls back to Mode::Zero
        /// retrieval but still runs synthesizer-driven flows.
        embedder: Option<Box<dyn Embedder>>,
        /// LLM synthesis provider. If `None`, retrieval returns raw chunks.
        synthesizer: Option<Box<dyn Synthesizer>>,
    },
}

impl Mode {
    /// Returns true if this mode can run fully offline.
    pub fn is_offline(&self) -> bool {
        matches!(self, Mode::Zero)
    }
}

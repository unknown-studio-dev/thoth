//! Hybrid retrieval orchestrator.
//!
//! In **Mode::Zero** four independent recallers run over the local indexes:
//!
//! 1. **Symbol lookup** over [`KvStore`] (exact + prefix on FQN tokens).
//! 2. **BM25** via [`FtsIndex`].
//! 3. **Graph fan-out** from whichever symbols the first two steps hit.
//! 4. **Markdown grep** over `MEMORY.md` (fact bullets surface as chunks).
//!
//! In **Mode::Full** an additional [`VectorStore`] stage runs — the query text
//! is embedded with the configured [`Embedder`] and the top-k nearest
//! neighbours by cosine similarity are folded into the fusion alongside the
//! other sources. If a [`Synthesizer`] is configured, the fused chunks are
//! then handed to it to produce a natural-language answer.
//!
//! Results are fused with [Reciprocal Rank Fusion][rrf] and the top-K by
//! fused score becomes the [`Retrieval`].
//!
//! [rrf]: https://plg.uwaterloo.ca/~gvcormac/cormacksigir09-rrf.pdf

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use thoth_core::{
    Chunk, Embedder, Prompt, Query, Result, Retrieval, RetrievalSource, Synthesizer,
};
use thoth_graph::Graph;
use thoth_store::{FtsHit, KvStore, MarkdownStore, StoreRoot, SymbolRow, VectorHit, VectorStore};
use uuid::Uuid;

use crate::indexer::{chunk_id, read_span};

/// Reciprocal-Rank-Fusion constant. 60 is the Cormack/Clarke default.
const RRF_K: f32 = 60.0;

/// Top-level retrieval orchestrator.
///
/// Built on top of an opened [`StoreRoot`]. Cheap to clone.
#[derive(Clone)]
pub struct Retriever {
    store: StoreRoot,
    graph: Graph,
    vectors: Option<VectorStore>,
    embedder: Option<Arc<dyn Embedder>>,
    synthesizer: Option<Arc<dyn Synthesizer>>,
}

impl Retriever {
    /// Create a Mode::Zero retriever — no vector stage, no synthesis.
    pub fn new(store: StoreRoot) -> Self {
        let graph = Graph::new(store.kv.clone());
        Self {
            store,
            graph,
            vectors: None,
            embedder: None,
            synthesizer: None,
        }
    }

    /// Create a Mode::Full retriever with any of the optional providers
    /// attached. Passing `None` for all three is equivalent to [`Retriever::new`].
    pub fn with_full(
        store: StoreRoot,
        vectors: Option<VectorStore>,
        embedder: Option<Arc<dyn Embedder>>,
        synthesizer: Option<Arc<dyn Synthesizer>>,
    ) -> Self {
        let graph = Graph::new(store.kv.clone());
        Self {
            store,
            graph,
            vectors,
            embedder,
            synthesizer,
        }
    }

    /// Mode::Zero recall. Runs each local source, then fuses with RRF.
    pub async fn recall(&self, q: &Query) -> Result<Retrieval> {
        self.recall_inner(q, /* with_vector */ false, /* with_synth */ false)
            .await
    }

    /// Mode::Full recall. Adds the vector stage (if an embedder + vector
    /// store are configured) and runs the synthesizer (if one is configured)
    /// against the top-K fused chunks.
    pub async fn recall_full(&self, q: &Query) -> Result<Retrieval> {
        self.recall_inner(q, /* with_vector */ true, /* with_synth */ true)
            .await
    }

    async fn recall_inner(
        &self,
        q: &Query,
        with_vector: bool,
        with_synth: bool,
    ) -> Result<Retrieval> {
        let k = q.top_k.max(1);
        let mut fused: HashMap<String, FusedRow> = HashMap::new();

        // 1. symbol lookup
        let sym_hits = self.symbol_stage(&q.text).await?;
        for (rank, cand) in sym_hits.iter().enumerate() {
            fuse(&mut fused, cand, rank);
        }

        // 2. BM25
        let fts_hits = self.bm25_stage(&q.text, k * 3).await?;
        for (rank, cand) in fts_hits.iter().enumerate() {
            fuse(&mut fused, cand, rank);
        }

        // 3. graph fan-out (depth 1) from the first handful of symbol seeds
        let seeds: Vec<String> = sym_hits
            .iter()
            .chain(fts_hits.iter())
            .filter_map(|c| c.symbol.clone())
            .take(8)
            .collect();
        let graph_hits = self.graph_stage(&seeds).await?;
        for (rank, cand) in graph_hits.iter().enumerate() {
            fuse(&mut fused, cand, rank);
        }

        // 4. markdown grep
        let md_hits = self.markdown_stage(&q.text).await?;
        for (rank, cand) in md_hits.iter().enumerate() {
            fuse(&mut fused, cand, rank);
        }

        // 5. (Mode::Full) vector similarity — only if both an embedder and a
        //    vector store are configured. Silent no-op otherwise so that
        //    Mode::Full without a key still degrades to Mode::Zero recall.
        if with_vector
            && let Some(hits) = self.vector_stage(&q.text, k * 3).await?
        {
            for (rank, cand) in hits.iter().enumerate() {
                fuse(&mut fused, cand, rank);
            }
        }

        // Sort by fused score, take top-k.
        let mut ranked: Vec<FusedRow> = fused.into_values().collect();
        ranked.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut chunks = Vec::with_capacity(k);
        for row in ranked.into_iter().take(k) {
            chunks.push(self.materialize(row).await?);
        }

        // 6. (Mode::Full) synthesis.
        let synthesized = if with_synth {
            self.synthesize(&q.text, &chunks).await?
        } else {
            None
        };

        Ok(Retrieval {
            chunks,
            synthesized,
            correlation_id: Uuid::new_v4(),
        })
    }

    // ---- per-source stages -------------------------------------------------

    async fn symbol_stage(&self, text: &str) -> Result<Vec<Candidate>> {
        let kv: &KvStore = &self.store.kv;
        let mut out = Vec::new();
        for tok in tokens(text) {
            let rows: Vec<SymbolRow> = kv.symbols_with_prefix(tok.clone()).await?;
            for r in rows {
                out.push(Candidate::from_symbol(r));
            }
        }
        dedupe(&mut out);
        Ok(out)
    }

    async fn bm25_stage(&self, text: &str, k: usize) -> Result<Vec<Candidate>> {
        let hits: Vec<FtsHit> = self.store.fts.search(text, k).await?;
        Ok(hits.into_iter().map(Candidate::from_fts).collect())
    }

    async fn graph_stage(&self, seeds: &[String]) -> Result<Vec<Candidate>> {
        let mut out = Vec::new();
        for seed in seeds {
            let ns = self.graph.neighbors(seed, 1).await?;
            for n in ns {
                out.push(Candidate {
                    id: chunk_id(&n.path, n.line, n.line),
                    path: n.path,
                    start_line: n.line,
                    end_line: n.line,
                    symbol: Some(n.fqn),
                    source: RetrievalSource::Graph,
                    preview: None,
                });
            }
        }
        dedupe(&mut out);
        Ok(out)
    }

    async fn markdown_stage(&self, text: &str) -> Result<Vec<Candidate>> {
        let md: &MarkdownStore = &self.store.markdown;
        // Cheap heuristic: split the query into meaningful tokens and union
        // the matches. Short tokens would match everything, so we filter.
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for tok in tokens(text) {
            let facts = md.grep_facts(&tok).await?;
            for f in facts {
                let preview = first_nonempty_line(&f.text);
                let id = format!("memory.md:{}", blake3::hash(f.text.as_bytes()).to_hex());
                if !seen.insert(id.clone()) {
                    continue;
                }
                out.push(Candidate {
                    id,
                    path: PathBuf::from("MEMORY.md"),
                    start_line: 0,
                    end_line: 0,
                    symbol: None,
                    source: RetrievalSource::Markdown,
                    preview: Some(preview),
                });
            }
        }
        Ok(out)
    }

    /// Returns `Ok(None)` when the vector stage is disabled (no embedder or
    /// no vector store configured). Returns `Ok(Some(vec))` — possibly empty
    /// — when the stage ran.
    async fn vector_stage(&self, text: &str, k: usize) -> Result<Option<Vec<Candidate>>> {
        let (Some(embedder), Some(vectors)) = (self.embedder.as_ref(), self.vectors.as_ref())
        else {
            return Ok(None);
        };
        let embeddings = embedder.embed_batch(&[text]).await?;
        let Some(q_vec) = embeddings.into_iter().next() else {
            return Ok(Some(Vec::new()));
        };
        let hits: Vec<VectorHit> = vectors.search(embedder.model_id(), &q_vec, k).await?;
        let mut out = Vec::with_capacity(hits.len());
        for h in hits {
            // Every vector we wrote uses a chunk_id shaped like
            // `"<path>:<start>-<end>"`. If a row fails to parse we skip it —
            // the vector DB might hold rows written by an older schema.
            if let Some((path, start, end)) = parse_chunk_id(&h.id) {
                out.push(Candidate {
                    id: h.id,
                    path,
                    start_line: start,
                    end_line: end,
                    symbol: None,
                    source: RetrievalSource::Vector,
                    preview: None,
                });
            }
        }
        Ok(Some(out))
    }

    // ---- synthesis ---------------------------------------------------------

    async fn synthesize(&self, question: &str, chunks: &[Chunk]) -> Result<Option<String>> {
        let Some(synth) = self.synthesizer.as_ref() else {
            return Ok(None);
        };
        if chunks.is_empty() {
            return Ok(None);
        }
        // Pull lessons so Claude can weave them in. Failing to read lessons
        // is not fatal — treat as empty.
        let lessons = self.store.markdown.read_lessons().await.unwrap_or_default();

        let prompt = Prompt {
            question: question.to_string(),
            chunks: chunks.to_vec(),
            lessons,
            max_tokens: None,
        };
        let syn = synth.synthesize(&prompt).await?;
        Ok(Some(syn.answer))
    }

    // ---- chunk materialisation --------------------------------------------

    async fn materialize(&self, row: FusedRow) -> Result<Chunk> {
        let (body, preview) = match row.cand.source {
            RetrievalSource::Markdown => {
                let p = row.cand.preview.clone().unwrap_or_default();
                (p.clone(), p)
            }
            _ => {
                let body = read_span(
                    &row.cand.path,
                    row.cand.start_line.max(1),
                    row.cand.end_line.max(row.cand.start_line.max(1)),
                )
                .await
                .unwrap_or_default();
                let prev = short_preview(&body);
                (body, prev)
            }
        };

        Ok(Chunk {
            id: row.cand.id,
            path: row.cand.path,
            line: row.cand.start_line,
            span: (row.cand.start_line, row.cand.end_line),
            symbol: row.cand.symbol,
            preview,
            body,
            score: row.score,
            source: row.cand.source,
        })
    }
}

// ---- fusion helpers --------------------------------------------------------

#[derive(Clone)]
struct Candidate {
    id: String,
    path: PathBuf,
    start_line: u32,
    end_line: u32,
    symbol: Option<String>,
    source: RetrievalSource,
    preview: Option<String>,
}

impl Candidate {
    fn from_symbol(r: SymbolRow) -> Self {
        Self {
            id: chunk_id(&r.path, r.start_line, r.end_line),
            path: r.path,
            start_line: r.start_line,
            end_line: r.end_line,
            symbol: Some(r.fqn),
            source: RetrievalSource::Symbol,
            preview: None,
        }
    }

    fn from_fts(h: FtsHit) -> Self {
        Self {
            id: h.id,
            path: PathBuf::from(h.path),
            start_line: h.start_line,
            end_line: h.end_line,
            symbol: h.symbol,
            source: RetrievalSource::FullText,
            preview: None,
        }
    }
}

struct FusedRow {
    cand: Candidate,
    score: f32,
}

fn fuse(map: &mut HashMap<String, FusedRow>, c: &Candidate, rank: usize) {
    let delta = 1.0 / (RRF_K + rank as f32 + 1.0);
    map.entry(c.id.clone())
        .and_modify(|row| row.score += delta)
        .or_insert_with(|| FusedRow {
            cand: c.clone(),
            score: delta,
        });
}

fn dedupe(v: &mut Vec<Candidate>) {
    let mut seen = std::collections::HashSet::new();
    v.retain(|c| seen.insert(c.id.clone()));
}

fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '_' && c != ':')
        .filter(|t| t.len() >= 3)
        .map(str::to_ascii_lowercase)
        .collect()
}

fn first_nonempty_line(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

fn short_preview(body: &str) -> String {
    body.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(3)
        .collect::<Vec<_>>()
        .join(" ⏎ ")
}

/// Parse a `chunk_id` shaped like `"<path>:<start>-<end>"` back into its
/// components. Returns `None` if the id doesn't match the expected shape —
/// e.g. markdown memory ids which use a different scheme.
fn parse_chunk_id(id: &str) -> Option<(PathBuf, u32, u32)> {
    let colon = id.rfind(':')?;
    let (path_part, span_part) = id.split_at(colon);
    let span_part = &span_part[1..]; // drop the ':'
    let (s, e) = span_part.split_once('-')?;
    let start: u32 = s.parse().ok()?;
    let end: u32 = e.parse().ok()?;
    Some((PathBuf::from(path_part), start, end))
}

#[cfg(test)]
mod tests {
    use super::parse_chunk_id;
    use std::path::PathBuf;

    #[test]
    fn parse_plain_chunk_id() {
        let id = "src/lib.rs:10-42";
        let (p, s, e) = parse_chunk_id(id).unwrap();
        assert_eq!(p, PathBuf::from("src/lib.rs"));
        assert_eq!(s, 10);
        assert_eq!(e, 42);
    }

    #[test]
    fn rejects_markdown_memory_id() {
        // memory.md:<64 hex> — span part won't parse as `<u32>-<u32>`.
        let id = "memory.md:deadbeef";
        assert!(parse_chunk_id(id).is_none());
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_chunk_id("no-colon-here").is_none());
        assert!(parse_chunk_id("path:not-numbers").is_none());
    }
}

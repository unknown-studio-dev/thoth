//! Full-project indexer.
//!
//! Walks a source tree, parses every recognised file with `thoth-parse`, and
//! writes the results into every backend of a [`StoreRoot`]:
//!
//! - `fts`      — one BM25 document per [`SourceChunk`].
//! - `kv`       — symbol rows keyed by FQN (for exact lookup).
//! - `graph`    — nodes per symbol, edges for calls + imports.
//!
//! Per-file work (parse + FTS/KV/graph writes) fans out across a bounded
//! pool of concurrent tasks; the underlying stores are already behind their
//! own mutexes, so writes serialize there naturally. Embedding is deferred
//! until the whole tree is walked so we can ship chunks to the provider in
//! large batches instead of one HTTP round-trip per file.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::stream::{self, StreamExt};
use parking_lot::Mutex;
use thoth_core::{Embedder, Result};
use thoth_graph::{Edge, EdgeKind, Graph, Node};
use thoth_parse::{
    LanguageRegistry, SourceChunk, Symbol, SymbolKind, SymbolTable,
    walk::{WalkOptions, walk_sources},
};
use thoth_store::{ChunkDoc, StoreRoot, SymbolRow, VectorStore};
use tracing::debug;

/// How many chunks to embed in one `embed_batch` call. Each provider adapter
/// already chunks this down to its own HTTP cap (Voyage 128, OpenAI 2048,
/// Cohere 96), so 256 is a comfortable upper bound that keeps the progress
/// bar moving without blowing out memory.
const EMBED_BATCH_SIZE: usize = 256;

/// Stats returned from one full [`Indexer::index_path`] run.
#[derive(Debug, Default, Clone, Copy)]
pub struct IndexStats {
    /// Files touched.
    pub files: usize,
    /// Chunks written to the BM25 index.
    pub chunks: usize,
    /// Symbols written to the KV + graph.
    pub symbols: usize,
    /// Call edges inserted.
    pub calls: usize,
    /// Import edges inserted.
    pub imports: usize,
    /// Chunks embedded into the vector store. `0` unless an [`Embedder`]
    /// + [`VectorStore`] are configured.
    pub embedded: usize,
}

/// Progress event fired during [`Indexer::index_path`].
///
/// The indexer walks a tree in four stages and emits one event per stage
/// transition (and per unit of progress within each stage):
///
/// | Stage      | `done` / `total` counted in | Emitted              |
/// |------------|-----------------------------|----------------------|
/// | `"walk"`   | files                       | once, at start       |
/// | `"file"`   | files                       | after each file      |
/// | `"embed"`  | chunks                      | once at 0, then per batch |
/// | `"commit"` | files                       | once, before flushing FTS |
///
/// `path` is populated for `"file"` events only.
#[derive(Debug, Clone, Copy)]
pub struct IndexProgress<'a> {
    /// Current pipeline stage (see table above).
    pub stage: &'static str,
    /// Units processed so far.
    pub done: usize,
    /// Total units in this stage.
    pub total: usize,
    /// File path for the `"file"` stage.
    pub path: Option<&'a Path>,
}

/// Dynamic progress callback. Stored inside [`Indexer`] when
/// [`Indexer::with_progress`] is called.
type ProgressFn = Arc<dyn for<'a> Fn(IndexProgress<'a>) + Send + Sync>;

/// Project indexer.
#[derive(Clone)]
pub struct Indexer {
    store: StoreRoot,
    graph: Graph,
    registry: LanguageRegistry,
    /// Optional embedder — if set (together with `vectors`) the indexer
    /// populates the vector store as it walks the tree.
    embedder: Option<Arc<dyn Embedder>>,
    /// Optional vector store — set together with `embedder` for Mode::Full.
    vectors: Option<VectorStore>,
    /// Optional per-file progress callback.
    on_progress: Option<ProgressFn>,
    /// Max concurrent per-file pipelines during [`Indexer::index_path`].
    concurrency: usize,
}

impl Indexer {
    /// Build a new indexer over the given store + language registry.
    pub fn new(store: StoreRoot, registry: LanguageRegistry) -> Self {
        let graph = Graph::new(store.kv.clone());
        Self {
            store,
            graph,
            registry,
            embedder: None,
            vectors: None,
            on_progress: None,
            concurrency: default_concurrency(),
        }
    }

    /// Override the per-file concurrency cap used by [`Indexer::index_path`].
    /// Passing `0` falls back to the default (≈ CPU count, capped at 16).
    pub fn with_concurrency(mut self, n: usize) -> Self {
        self.concurrency = if n == 0 { default_concurrency() } else { n };
        self
    }

    /// Attach an [`Embedder`] + [`VectorStore`] so the indexer also
    /// populates the semantic search backend. Returns `self` for chaining.
    pub fn with_embedding(mut self, embedder: Arc<dyn Embedder>, vectors: VectorStore) -> Self {
        self.embedder = Some(embedder);
        self.vectors = Some(vectors);
        self
    }

    /// Register a progress callback fired once per file during
    /// [`Indexer::index_path`] (plus one `stage = "walk"` at the start and
    /// one `stage = "commit"` at the end).
    pub fn with_progress<F>(mut self, cb: F) -> Self
    where
        F: for<'a> Fn(IndexProgress<'a>) + Send + Sync + 'static,
    {
        self.on_progress = Some(Arc::new(cb));
        self
    }

    fn emit(&self, ev: IndexProgress<'_>) {
        if let Some(cb) = &self.on_progress {
            cb(ev);
        }
    }

    /// Index every eligible file under `root`.
    ///
    /// Pipeline:
    /// 1. Walk the source tree (synchronous; fast).
    /// 2. Fan out per-file parse + FTS/KV/graph writes over a bounded pool
    ///    of concurrent tasks. Chunks are buffered for later embedding.
    /// 3. If Mode::Full is configured, batch-embed every collected chunk in
    ///    rounds of [`EMBED_BATCH_SIZE`] — one `embed_batch` call per round
    ///    instead of one per file.
    /// 4. Commit the BM25 writer so fresh docs become searchable.
    pub async fn index_path(&self, root: impl AsRef<Path>) -> Result<IndexStats> {
        let root = root.as_ref().to_path_buf();
        let opts = WalkOptions::default();
        let files = walk_sources(&root, &self.registry, &opts);
        let total = files.len();
        debug!(count = total, ?root, concurrency = self.concurrency, "indexing");
        self.emit(IndexProgress {
            stage: "walk",
            done: 0,
            total,
            path: None,
        });

        // Phase A: fan-out parse + writes.
        let stats = Arc::new(Mutex::new(IndexStats::default()));
        let pending: Arc<Mutex<Vec<SourceChunk>>> = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(AtomicUsize::new(0));
        let want_embed = self.embedder.is_some() && self.vectors.is_some();

        stream::iter(files)
            .for_each_concurrent(self.concurrency, |path| {
                let this = self.clone();
                let stats = stats.clone();
                let pending = pending.clone();
                let done = done.clone();
                async move {
                    match this.index_file_no_embed(&path).await {
                        Ok((s, chunks)) => {
                            {
                                let mut st = stats.lock();
                                st.files += 1;
                                st.chunks += s.chunks;
                                st.symbols += s.symbols;
                                st.calls += s.calls;
                                st.imports += s.imports;
                            }
                            if want_embed {
                                pending.lock().extend(chunks);
                            }
                        }
                        Err(e) => {
                            debug!(?path, error = %e, "skip: index error");
                        }
                    }
                    let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                    this.emit(IndexProgress {
                        stage: "file",
                        done: d,
                        total,
                        path: Some(&path),
                    });
                }
            })
            .await;

        // Phase B: batch embedding (Mode::Full only).
        if want_embed {
            let chunks: Vec<SourceChunk> = std::mem::take(&mut *pending.lock());
            let total_embed = chunks.len();
            self.emit(IndexProgress {
                stage: "embed",
                done: 0,
                total: total_embed,
                path: None,
            });
            let mut embedded = 0usize;
            for (i, batch) in chunks.chunks(EMBED_BATCH_SIZE).enumerate() {
                match self.embed_chunks(batch).await {
                    Ok(Some(n)) => embedded += n,
                    Ok(None) => {}
                    Err(e) => {
                        // Don't abort the whole run — log, skip this batch.
                        debug!(error = %e, batch = i, "skip: embed error");
                    }
                }
                let done_count = ((i + 1) * EMBED_BATCH_SIZE).min(total_embed);
                self.emit(IndexProgress {
                    stage: "embed",
                    done: done_count,
                    total: total_embed,
                    path: None,
                });
            }
            stats.lock().embedded += embedded;
        }

        // Phase C: commit FTS.
        self.emit(IndexProgress {
            stage: "commit",
            done: total,
            total,
            path: None,
        });
        self.store.fts.commit().await?;

        let final_stats = *stats.lock();
        debug!(?final_stats, "index complete");
        Ok(final_stats)
    }

    /// Index a single file. Public so callers (e.g. the watcher) can
    /// re-index on change. Embeds the file's chunks inline if a provider is
    /// configured.
    pub async fn index_file(&self, path: &Path) -> Result<IndexStats> {
        let (mut s, chunks) = self.index_file_no_embed(path).await?;
        if let Some(n) = self.embed_chunks(&chunks).await? {
            s.embedded += n;
        }
        Ok(s)
    }

    /// Internal: parse + write chunks/symbols/edges for one file, returning
    /// the parsed chunks so a caller (e.g. [`Indexer::index_path`]) can defer
    /// embedding and batch it across files.
    async fn index_file_no_embed(&self, path: &Path) -> Result<(IndexStats, Vec<SourceChunk>)> {
        let mut s = IndexStats::default();
        let (chunks, table) = thoth_parse::parse_file(&self.registry, path).await?;

        for c in &chunks {
            self.write_chunk(c).await?;
            s.chunks += 1;
        }
        for sym in &table.symbols {
            self.write_symbol(sym).await?;
            s.symbols += 1;
        }
        self.write_call_edges(&table, path).await?;
        s.calls += table.calls.len();
        self.write_import_edges(&table, path).await?;
        s.imports += table.imports.len();

        Ok((s, chunks))
    }

    /// Embed every chunk body and upsert into the vector store. Returns
    /// `Some(n)` with the number of rows written, or `None` if the vector
    /// stage isn't configured.
    async fn embed_chunks(&self, chunks: &[SourceChunk]) -> Result<Option<usize>> {
        let (Some(embedder), Some(vectors)) = (self.embedder.as_ref(), self.vectors.as_ref())
        else {
            return Ok(None);
        };
        if chunks.is_empty() {
            return Ok(Some(0));
        }
        // embed_batch takes &[&str]; build a parallel Vec<&str>.
        let texts: Vec<&str> = chunks.iter().map(|c| c.body.as_str()).collect();
        let vecs = embedder.embed_batch(&texts).await?;
        let items: Vec<(String, Vec<f32>)> = chunks
            .iter()
            .zip(vecs.into_iter())
            .map(|(c, v)| (chunk_id(&c.path, c.start_line, c.end_line), v))
            .collect();
        vectors.upsert_batch(&items, embedder.model_id()).await?;
        Ok(Some(items.len()))
    }

    // -------------------------------------------------------------------

    async fn write_chunk(&self, c: &SourceChunk) -> Result<()> {
        let id = chunk_id(&c.path, c.start_line, c.end_line);
        self.store
            .fts
            .index_chunk(ChunkDoc {
                id,
                path: c.path.to_string_lossy().into_owned(),
                symbol: c.symbol.clone(),
                body: c.body.clone(),
                start_line: c.start_line,
                end_line: c.end_line,
                language: c.language.to_string(),
            })
            .await
    }

    async fn write_symbol(&self, sym: &Symbol) -> Result<()> {
        let kind_tag = symbol_kind_tag(sym.kind).to_string();
        // KV symbol row for exact lookup.
        self.store
            .kv
            .put_symbol(SymbolRow {
                fqn: sym.fqn.clone(),
                path: sym.path.clone(),
                start_line: sym.span.0,
                end_line: sym.span.1,
                kind: kind_tag.clone(),
            })
            .await?;
        // Graph node.
        self.graph
            .upsert_node(Node {
                fqn: sym.fqn.clone(),
                kind: kind_tag,
                path: sym.path.clone(),
                line: sym.span.0,
            })
            .await
    }

    async fn write_call_edges(&self, table: &SymbolTable, _path: &Path) -> Result<()> {
        for (caller, callee) in &table.calls {
            self.graph
                .upsert_edge(Edge {
                    from: caller.clone(),
                    to: callee.clone(),
                    kind: EdgeKind::Calls,
                })
                .await?;
        }
        Ok(())
    }

    async fn write_import_edges(&self, table: &SymbolTable, path: &Path) -> Result<()> {
        let module = module_fqn(path);
        for imp in &table.imports {
            let target = imp.trim().to_string();
            if target.is_empty() {
                continue;
            }
            self.graph
                .upsert_edge(Edge {
                    from: module.clone(),
                    to: target,
                    kind: EdgeKind::Imports,
                })
                .await?;
        }
        Ok(())
    }
}

// ---- helpers ---------------------------------------------------------------

/// Stable chunk id — the tantivy delete key on re-index.
pub fn chunk_id(path: &Path, start: u32, end: u32) -> String {
    format!("{}:{}-{}", path.display(), start, end)
}

fn symbol_kind_tag(k: SymbolKind) -> &'static str {
    match k {
        SymbolKind::Function => "function",
        SymbolKind::Type => "type",
        SymbolKind::Trait => "trait",
        SymbolKind::Module => "module",
        SymbolKind::Binding => "binding",
    }
}

fn module_fqn(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Pick a sensible default fan-out for [`Indexer::index_path`]. Uses the
/// logical CPU count, capped at 16 so we don't stampede the provider's
/// rate limits or the underlying store mutexes on very large machines.
fn default_concurrency() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().min(16))
        .unwrap_or(4)
}

/// Read the lines `[start_line..=end_line]` (1-based, inclusive) from a file,
/// returning the body text. Used when retrieval needs to surface the code
/// that FTS/graph only referenced by coordinates.
pub async fn read_span(path: &Path, start_line: u32, end_line: u32) -> Result<String> {
    let text = tokio::fs::read_to_string(path).await?;
    let start = start_line.saturating_sub(1) as usize;
    let end = end_line as usize;
    let mut out = String::new();
    for (i, line) in text.lines().enumerate() {
        if i >= start && i < end {
            out.push_str(line);
            out.push('\n');
        }
        if i >= end {
            break;
        }
    }
    Ok(out)
}

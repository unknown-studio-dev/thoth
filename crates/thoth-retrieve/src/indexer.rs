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
    /// Walker options: ignore patterns, max file size, hidden-dir toggle,
    /// symlink handling. Typically sourced from `config.toml`'s
    /// `[index]` table via [`Indexer::with_config`].
    walk_opts: WalkOptions,
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
            walk_opts: WalkOptions::default(),
        }
    }

    /// Attach extra ignore patterns (gitignore syntax) that will be applied
    /// during [`Indexer::index_path`] on top of `.gitignore`, `.ignore`, and
    /// `.thothignore`. Malformed patterns are logged and skipped.
    ///
    /// Typical source: `config.toml`'s `[index] ignore = [...]`.
    pub fn with_ignore_patterns<I, S>(mut self, patterns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.walk_opts.extra_ignore_patterns = patterns.into_iter().map(Into::into).collect();
        self
    }

    /// Replace the [`WalkOptions`] wholesale. Useful for callers that want
    /// to tweak `max_file_size` / `include_hidden` / `follow_symlinks`
    /// programmatically without round-tripping through `config.toml`.
    pub fn with_walk_options(mut self, opts: WalkOptions) -> Self {
        self.walk_opts = opts;
        self
    }

    /// Apply a user-facing [`IndexConfig`] (typically loaded from
    /// `config.toml`) to this indexer. Sets the ignore list, max file size,
    /// hidden-dir toggle, and symlink handling in one call.
    ///
    /// This is the "one-stop wire" for apps: load once, pass here, done.
    pub fn with_config(mut self, cfg: &crate::IndexConfig) -> Self {
        self.walk_opts = WalkOptions {
            max_file_size: cfg.max_file_size,
            follow_symlinks: cfg.follow_symlinks,
            include_hidden: cfg.include_hidden,
            extra_ignore_patterns: cfg.ignore.clone(),
        };
        self
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
        let files = walk_sources(&root, &self.registry, &self.walk_opts);
        let total = files.len();
        debug!(
            count = total,
            ?root,
            concurrency = self.concurrency,
            "indexing"
        );
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
    ///
    /// Any pre-existing index state for `path` (FTS chunks, KV symbol rows,
    /// graph nodes/edges, and — in Mode::Full — vectors) is purged before
    /// the new parse is written, so line shifts, renames, and deleted
    /// symbols don't leave stale rows behind. The caller is still
    /// responsible for calling [`Indexer::commit`] before the next query.
    pub async fn index_file(&self, path: &Path) -> Result<IndexStats> {
        let (mut s, chunks) = self.index_file_no_embed(path).await?;
        if let Some(n) = self.embed_chunks(&chunks).await? {
            s.embedded += n;
        }
        Ok(s)
    }

    /// Remove every indexed artefact that references `path` — FTS chunks,
    /// KV symbol rows, graph nodes/edges, and (if Mode::Full) vectors.
    ///
    /// Used by both [`Indexer::index_file`] (purge-before-write) and the
    /// watcher's `FileDeleted` branch (purge-only, no reparse). Commit is
    /// the caller's responsibility so batched watch events can coalesce
    /// into a single flush.
    pub async fn purge_path(&self, path: &Path) -> Result<()> {
        let path_str = path.to_string_lossy().into_owned();

        // 1. FTS — delete every doc whose `path` field matches.
        self.store.fts.delete_path(&path_str).await?;

        // 2. KV symbols — collect the FQNs we're dropping so we can also
        //    prune graph nodes and any edges that touch them.
        let symbol_fqns = self.store.kv.delete_symbols_by_path(path).await?;

        // 3. Graph nodes + edges.
        let (_node_count, _edge_count) = self.graph.purge_path(path).await?;
        if !symbol_fqns.is_empty() {
            // `delete_nodes_by_path` will usually be a superset, but some
            // symbol rows live without matching graph nodes (e.g. when the
            // parser produced a symbol but not a node — rare, belt-and-
            // braces). Drop any edges keyed on those FQNs explicitly.
            let _ = self.store.kv.delete_edges_touching(&symbol_fqns).await?;
        }

        // 4. Vectors — Mode::Full only. Safe to skip otherwise.
        if let Some(vectors) = &self.vectors {
            let _ = vectors.delete_by_path(&path_str).await?;
        }

        // 5. Drop the content-hash sentinel so the next writer sees a miss
        //    and rebuilds from scratch. Without this, deleting + recreating
        //    a file would short-circuit on the old hash.
        self.store.kv.delete_meta(hash_meta_key(path)).await?;

        Ok(())
    }

    /// Flush the BM25 writer so previously indexed chunks become
    /// searchable. Safe to call repeatedly.
    pub async fn commit(&self) -> Result<()> {
        self.store.fts.commit().await
    }

    /// Internal: parse + write chunks/symbols/edges for one file, returning
    /// the parsed chunks so a caller (e.g. [`Indexer::index_path`]) can defer
    /// embedding and batch it across files.
    ///
    /// The file's previous index state is purged before the new parse is
    /// written so stale chunks (e.g. from a function that moved lines or
    /// was deleted) can never linger.
    ///
    /// # Content-hash gating
    ///
    /// Before doing any work, we blake3 the file bytes and compare against
    /// the hash we stored under `hash:<path>` the last time this file was
    /// indexed. If they match, the on-disk state is authoritative and we
    /// short-circuit — no purge, no reparse, no writes. This is DESIGN §9's
    /// "content-hash gated" writer clause. On a hash miss (new file, real
    /// edit, or first-ever index) we fall through to the full pipeline and
    /// record the new hash at the end.
    async fn index_file_no_embed(&self, path: &Path) -> Result<(IndexStats, Vec<SourceChunk>)> {
        let bytes = tokio::fs::read(path).await?;
        let new_hash = blake3::hash(&bytes);
        let hash_key = hash_meta_key(path);

        let new_hash_bytes: &[u8] = new_hash.as_bytes();
        if let Some(prev) = self.store.kv.get_meta(hash_key.clone()).await?
            && prev.as_slice() == new_hash_bytes
        {
            debug!(?path, "skip: content hash unchanged");
            return Ok((IndexStats::default(), Vec::new()));
        }

        self.purge_path(path).await?;
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
        // Build the file-local resolution map. Two ingredients, in
        // priority order:
        //   1. Import aliases (`use foo::Bar as Baz`) — the local name
        //      the source will use maps to the fully qualified target.
        //   2. Symbols declared in this file — for every call to a
        //      same-file function (`foo()` inside module `m`) we want
        //      the edge to land on `m::foo` instead of the bare leaf,
        //      so BFS over `Calls` actually connects.
        // #2 fills in gaps #1 misses. When both produce a binding we
        // prefer the alias (explicit > implicit).
        let mut resolution: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for sym in &table.symbols {
            if let Some(leaf) = sym.fqn.rsplit("::").next() {
                resolution
                    .entry(leaf.to_string())
                    .or_insert_with(|| sym.fqn.clone());
            }
        }
        for (local, target) in &table.aliases {
            resolution.insert(local.clone(), target.clone());
        }
        self.write_call_edges(&table, &resolution).await?;
        s.calls += table.calls.len();
        let alias_only: std::collections::HashMap<String, String> =
            table.aliases.iter().cloned().collect();
        self.write_import_edges(&table, path, &alias_only).await?;
        s.imports += table.imports.len();
        self.write_extends_edges(&table, &resolution).await?;

        // Record the new content hash *after* all the writes succeeded — if
        // we crash mid-write, next run will see a hash miss and retry.
        self.store.kv.put_meta(hash_key, new_hash_bytes).await?;

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

    async fn write_call_edges(
        &self,
        table: &SymbolTable,
        aliases: &std::collections::HashMap<String, String>,
    ) -> Result<()> {
        for (caller, callee) in &table.calls {
            // Callee names come out of the parser as a bare leaf
            // identifier (`foo()` → `"foo"`; `x.bar()` → `"bar"`). If
            // the file's import-alias map has a binding for that name
            // we route the edge to the resolved FQN so cross-module
            // call chains connect through the graph.
            let resolved = aliases.get(callee).cloned().unwrap_or_else(|| callee.clone());
            self.graph
                .upsert_edge(Edge {
                    from: caller.clone(),
                    to: resolved,
                    kind: EdgeKind::Calls,
                })
                .await?;
        }
        Ok(())
    }

    async fn write_import_edges(
        &self,
        table: &SymbolTable,
        path: &Path,
        aliases: &std::collections::HashMap<String, String>,
    ) -> Result<()> {
        let module = module_fqn(path);

        // Prefer parsed aliases — each one is a real (local_name,
        // resolved_target) pair, so writing an Imports edge to the
        // resolved side gives a clean module→symbol graph without
        // dragging raw statement text like "use std::sync::Arc;" into
        // the graph.
        if !aliases.is_empty() {
            let mut seen = std::collections::HashSet::new();
            for target in aliases.values() {
                if seen.insert(target.clone()) {
                    self.graph
                        .upsert_edge(Edge {
                            from: module.clone(),
                            to: target.clone(),
                            kind: EdgeKind::Imports,
                        })
                        .await?;
                }
            }
            return Ok(());
        }

        // Fallback: no alias info (unsupported language, malformed
        // source). Store the raw statement text so the graph at least
        // remembers *something* about the file's dependencies.
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

    async fn write_extends_edges(
        &self,
        table: &SymbolTable,
        aliases: &std::collections::HashMap<String, String>,
    ) -> Result<()> {
        for (child, parent) in &table.extends {
            // Parent name is whatever the source wrote: a bare name, a
            // local alias, or a qualified path. Run it through the
            // alias map first (so `class X extends Foo` after
            // `import { Foo } from 'lib'` routes to `lib::Foo`); fall
            // back to the raw text otherwise. Unknown parents still
            // show up in the graph — downstream readers can tell they
            // are unresolved because `Graph::get(&to)` returns None.
            let resolved = aliases.get(parent).cloned().unwrap_or_else(|| parent.clone());
            if resolved.is_empty() {
                continue;
            }
            self.graph
                .upsert_edge(Edge {
                    from: child.clone(),
                    to: resolved,
                    kind: EdgeKind::Extends,
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

/// Bump whenever the indexer's output schema changes (new edge kinds,
/// new alias-resolution rules, renamed FQN scheme, ...). The version
/// baked into the hash meta key invalidates every previously-stored
/// hash sentinel in one go, so the next indexer run re-parses every
/// file even when its bytes haven't changed.
const PARSER_SCHEMA_VERSION: u32 = 2;

/// Meta key under which we store the blake3 hash of the last-indexed bytes
/// of `path`. Kept private to the indexer — callers shouldn't need to read
/// it. The `hashVER:` prefix carries both the schema version (so a parser
/// upgrade invalidates every sentinel at once) and leaves room for future
/// per-path sentinels (e.g. `mtime:`) without colliding.
fn hash_meta_key(path: &Path) -> String {
    format!("hash{PARSER_SCHEMA_VERSION}:{}", path.display())
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

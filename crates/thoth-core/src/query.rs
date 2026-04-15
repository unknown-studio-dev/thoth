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
    /// Graph-backed context (callers, callees, imports, siblings,
    /// docstring). Populated by the retriever's enrichment pass when
    /// [`Query::enrich`] is set. `None` on legacy responses and in
    /// Mode::Zero when enrichment is disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<ChunkContext>,
}

/// Graph-backed context around a chunk. This is what makes Thoth's
/// recall output qualitatively different from a plain RAG hit: instead
/// of just the matched code we also surface who calls it, what it calls,
/// and its neighbours in the module.
///
/// All fields are best-effort — they reflect whatever the parser +
/// graph currently know about. Empty vectors mean "we looked and
/// found nothing", `None` on `doc` means "no leading comment block
/// detected".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChunkContext {
    /// Symbols that reference / call this chunk's symbol (incoming edges).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub callers: Vec<SymbolRef>,
    /// Symbols that this chunk's symbol references / calls (outgoing edges).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub callees: Vec<SymbolRef>,
    /// Imports declared at the top of the same file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<String>,
    /// Other symbols defined in the same file (for a quick sense of the
    /// surrounding module layout). Excludes the chunk's own symbol.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub siblings: Vec<SymbolRef>,
    /// Leading documentation comment extracted from the chunk body,
    /// stripped of comment markers. E.g. for Rust we collect the `///`
    /// lines that precede the item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
}

/// A lightweight reference to another symbol in the graph.
///
/// This is deliberately flat and cheap to serialize — it exists so
/// enrichment lookups can surface "callers: foo::bar at path:123"
/// without forcing consumers to understand the full graph schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolRef {
    /// Fully-qualified symbol name (e.g. `crate::module::fn_name`).
    pub fqn: String,
    /// File the symbol is defined in, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// 1-based line the symbol starts on, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
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
    /// Hit from the episodic log (past queries/answers/outcomes).
    Episodic,
}

impl Retrieval {
    /// Render this retrieval as the human-readable text surface that both
    /// the CLI (`thoth query`) and MCP (`thoth_recall` tool text) display.
    ///
    /// The format is designed so an agent reading the output can answer
    /// three questions per chunk without opening the file:
    ///
    /// * **Where does this live?** — symbol, path, line span, score.
    /// * **What does it do?** — leading docstring, then the full body.
    /// * **What's around it?** — callers, callees, imports, siblings.
    ///
    /// Empty sections are omitted rather than shown as `(none)` so the
    /// common case (a rich hit) stays compact.
    pub fn render(&self) -> String {
        if self.chunks.is_empty() {
            return "(no matches — did you run thoth_index?)".to_string();
        }
        let mut out = String::new();
        for (i, c) in self.chunks.iter().enumerate() {
            c.render_into(&mut out, i);
        }
        if let Some(answer) = &self.synthesized {
            out.push_str("\n─── synthesized ───\n");
            out.push_str(answer);
            out.push('\n');
        }
        out
    }
}

impl Chunk {
    /// Append a human-readable rendering of this chunk to `out`.
    ///
    /// Shared by [`Retrieval::render`] and the CLI; extracted so each
    /// consumer gets consistent formatting.
    pub fn render_into(&self, out: &mut String, index: usize) {
        let sym = self.symbol.as_deref().unwrap_or("-");
        out.push_str(&format!(
            "\n[{index:>2}] score={:.4} src={:?}  {}  {}:{}-{}\n",
            self.score,
            self.source,
            sym,
            self.path.display(),
            self.span.0,
            self.span.1
        ));

        if let Some(ctx) = &self.context
            && let Some(doc) = &ctx.doc
        {
            // Indent the doc block so it's visually distinct from the
            // body. Trim once so we don't double-indent multi-line docs.
            for line in doc.lines() {
                out.push_str("  │ ");
                out.push_str(line);
                out.push('\n');
            }
        }

        // Body with real newlines. We indent two spaces so the chunk
        // header/sections stand out in scrollback.
        if !self.body.is_empty() {
            for line in self.body.lines() {
                out.push_str("    ");
                out.push_str(line);
                out.push('\n');
            }
        } else if !self.preview.is_empty() {
            // Fallback: markdown/episodic hits don't have a body — show
            // the preview instead.
            out.push_str("     ");
            out.push_str(&self.preview);
            out.push('\n');
        }

        if let Some(ctx) = &self.context {
            render_symbol_row(out, "↑ callers", &ctx.callers);
            render_symbol_row(out, "↓ calls  ", &ctx.callees);
            render_symbol_row(out, "↔ siblings", &ctx.siblings);
            render_string_row(out, "📦 imports", &ctx.imports);
        }
    }
}

fn render_symbol_row(out: &mut String, label: &str, refs: &[SymbolRef]) {
    if refs.is_empty() {
        return;
    }
    let items: Vec<String> = refs
        .iter()
        .map(|r| match (&r.path, r.line) {
            (Some(p), Some(l)) => format!("{} @ {}:{}", r.fqn, p.display(), l),
            (Some(p), None) => format!("{} @ {}", r.fqn, p.display()),
            _ => r.fqn.clone(),
        })
        .collect();
    out.push_str(&format!("  {label}: {}\n", items.join(", ")));
}

fn render_string_row(out: &mut String, label: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    out.push_str(&format!("  {label}: {}\n", items.join(", ")));
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

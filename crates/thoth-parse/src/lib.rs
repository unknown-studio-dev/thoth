//! # thoth-parse
//!
//! tree-sitter wrapper, AST-aware chunking, file discovery, and change
//! watching.
//!
//! This crate is the perception layer for Thoth. Its outputs feed every
//! other pipeline:
//!
//! - [`parse_file`] produces the [`SourceChunk`]s and [`SymbolTable`] that
//!   `thoth-store` persists.
//! - [`walk::walk_sources`] enumerates indexable files in a project, honouring
//!   `.gitignore` and friends.
//! - [`watch::Watcher`] streams [`thoth_core::Event`] whenever files change.
//!
//! See `DESIGN.md` §4 and §9.

#![deny(rust_2018_idioms)]
#![warn(missing_docs)]

pub mod language;
pub mod walk;
pub mod watch;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thoth_core::{Error, Result};
use tree_sitter::{Node, Parser};

pub use language::{Language, LanguageRegistry};

/// A chunk of source code aligned to an AST node boundary.
///
/// Chunks are the unit of indexing: each one is hashed, embedded (Mode::Full),
/// and rerankable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceChunk {
    /// Absolute path of the source file.
    pub path: PathBuf,
    /// Canonical language identifier (e.g. `"rust"`, `"python"`).
    pub language: &'static str,
    /// 1-based starting line.
    pub start_line: u32,
    /// 1-based ending line (inclusive).
    pub end_line: u32,
    /// Fully qualified symbol name if this chunk is a top-level definition.
    pub symbol: Option<String>,
    /// Broad kind of the enclosing symbol.
    pub kind: Option<SymbolKind>,
    /// Source text of the chunk.
    pub body: String,
    /// blake3 hash of [`Self::body`] (for change detection).
    pub content_hash: [u8; 32],
}

/// A symbol discovered in an AST.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    /// Fully qualified name (module path + simple name, best effort).
    pub fqn: String,
    /// Broad kind.
    pub kind: SymbolKind,
    /// Source file.
    pub path: PathBuf,
    /// Line span (1-based, inclusive).
    pub span: (u32, u32),
}

/// Broad cross-language symbol kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    /// Function, method, or free subroutine.
    Function,
    /// Struct, class, record, enum.
    Type,
    /// Trait, interface, protocol.
    Trait,
    /// Module, namespace, package.
    Module,
    /// Named binding (const, static, let-at-module-level).
    Binding,
}

/// Symbols + coarse edges extracted from a single file's AST.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SymbolTable {
    /// Declared symbols.
    pub symbols: Vec<Symbol>,
    /// `(caller_fqn, callee_name)` edges. Callee resolution happens later
    /// in `thoth-graph` once imports are known.
    pub calls: Vec<(String, String)>,
    /// Raw import specifiers (`use foo::bar`, `from x import y`, ...).
    pub imports: Vec<String>,
}

/// Parse a single file and produce chunks + a symbol table.
///
/// If the file's language is not registered (or its grammar feature is
/// disabled at build time), returns an empty result — never errors.
pub async fn parse_file(
    registry: &LanguageRegistry,
    path: impl AsRef<Path>,
) -> Result<(Vec<SourceChunk>, SymbolTable)> {
    let path = path.as_ref().to_path_buf();
    let bytes = tokio::fs::read(&path).await?;

    // Parsing is CPU work; offload to a blocking worker.
    let registry = registry.clone();
    tokio::task::spawn_blocking(move || parse_bytes(&registry, &path, &bytes))
        .await
        .map_err(|e| Error::Parse(format!("join error: {e}")))?
}

fn parse_bytes(
    registry: &LanguageRegistry,
    path: &Path,
    bytes: &[u8],
) -> Result<(Vec<SourceChunk>, SymbolTable)> {
    let Some(lang) = registry.detect(path) else {
        tracing::debug!(?path, "no grammar registered; skipping");
        return Ok((Vec::new(), SymbolTable::default()));
    };

    let mut parser = Parser::new();
    parser
        .set_language(&lang.tree_sitter())
        .map_err(|e| Error::Parse(format!("set_language: {e}")))?;

    let tree = parser
        .parse(bytes, None)
        .ok_or_else(|| Error::Parse("tree-sitter returned no tree".to_string()))?;

    let root = tree.root_node();
    let mut chunks = Vec::new();
    let mut table = SymbolTable::default();

    let mut stack: Vec<(String, SymbolKind)> = Vec::new();
    walk_ast(
        root,
        bytes,
        path,
        lang,
        /* module_path */ &module_path_from(path),
        &mut chunks,
        &mut table,
        &mut stack,
    );

    // If no top-level items were emitted, at least emit a whole-file chunk so
    // BM25 / vectors have something to hang on to.
    if chunks.is_empty() {
        let body = String::from_utf8_lossy(bytes).into_owned();
        let end_line = body.lines().count().max(1) as u32;
        let hash = blake3::hash(body.as_bytes());
        chunks.push(SourceChunk {
            path: path.to_path_buf(),
            language: lang.name(),
            start_line: 1,
            end_line,
            symbol: None,
            kind: None,
            body,
            content_hash: *hash.as_bytes(),
        });
    }

    Ok((chunks, table))
}

fn module_path_from(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// AST walker.
///
/// Single depth-first pass that:
/// - Emits a chunk + symbol for each top-level or container-nested
///   definition (`SymbolKind::Function`, `Type`, `Trait`, `Module`,
///   `Binding`). Definitions nested *inside a function body* are not
///   chunked — that would blow up the index — but the walker still
///   descends into them so call edges get recorded.
/// - Records every `use` / `import` statement.
/// - Records `(caller_fqn, callee_simple_name)` for every call site,
///   attributing it to the nearest enclosing symbol on `stack`. If a call
///   happens at module scope with no enclosing symbol (e.g. a Python
///   top-level statement) it is skipped.
#[allow(clippy::too_many_arguments)]
fn walk_ast(
    node: Node<'_>,
    source: &[u8],
    path: &Path,
    lang: Language,
    module: &str,
    chunks: &mut Vec<SourceChunk>,
    table: &mut SymbolTable,
    stack: &mut Vec<(String, SymbolKind)>,
) {
    let kind_str = node.kind();

    // ---- calls -------------------------------------------------------------
    if lang.is_call_node(kind_str)
        && let (Some((caller, _)), Some(callee)) = (stack.last(), lang.extract_callee(node, source))
        && !callee.is_empty()
    {
        table.calls.push((caller.clone(), callee));
        // Still descend — calls can nest inside call arguments.
    }

    // ---- definitions -------------------------------------------------------
    if let Some(sym_kind) = lang.symbol_kind_for(kind_str) {
        let name = lang
            .extract_name(node, source)
            .unwrap_or_else(|| "<anon>".to_string());
        let fqn = format!("{module}::{name}");
        let start_row = node.start_position().row as u32 + 1;
        let end_row = node.end_position().row as u32 + 1;

        // Emit chunk + symbol only if we're not nested inside a function
        // body. We still descend into fn bodies so call edges get picked up.
        let inside_fn = stack.iter().any(|(_, k)| *k == SymbolKind::Function);

        if !inside_fn {
            let body_str = String::from_utf8_lossy(&source[node.byte_range()]).into_owned();
            let hash = blake3::hash(body_str.as_bytes());
            chunks.push(SourceChunk {
                path: path.to_path_buf(),
                language: lang.name(),
                start_line: start_row,
                end_line: end_row,
                symbol: Some(fqn.clone()),
                kind: Some(sym_kind),
                body: body_str,
                content_hash: *hash.as_bytes(),
            });
            table.symbols.push(Symbol {
                fqn: fqn.clone(),
                kind: sym_kind,
                path: path.to_path_buf(),
                span: (start_row, end_row),
            });
        }

        stack.push((fqn, sym_kind));
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            walk_ast(child, source, path, lang, module, chunks, table, stack);
        }
        stack.pop();
        return;
    }

    // ---- imports -----------------------------------------------------------
    if lang.is_import_node(kind_str) {
        if let Ok(text) = node.utf8_text(source) {
            table.imports.push(text.to_string());
        }
        return;
    }

    // ---- default: descend --------------------------------------------------
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_ast(child, source, path, lang, module, chunks, table, stack);
    }
}

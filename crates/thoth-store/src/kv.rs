//! Key-value / graph store backed by [`redb`].
//!
//! Four logical tables live in a single `kv.redb` file:
//!
//! - `nodes`  — graph nodes, keyed by fully-qualified name.
//! - `edges`  — graph edges, keyed by `"<src>|<kind>|<dst>"`.
//! - `symbols`— symbol → `(path, line_start, line_end)` lookups.
//! - `meta`   — free-form metadata (config, cursor positions, ...).
//!
//! Values are opaque byte strings; callers are expected to pick their own
//! serialization (we use JSON for human-friendly debugging in early
//! milestones). redb is synchronous, so every public method wraps the actual
//! I/O in [`tokio::task::spawn_blocking`].

use std::path::{Path, PathBuf};
use std::sync::Arc;

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use thoth_core::{Error, Result};

// ---- table definitions ------------------------------------------------------

const NODES: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("nodes");
const EDGES: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("edges");
const SYMBOLS: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("symbols");
const META: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("meta");

// ---- public payload types ---------------------------------------------------

/// Symbol location record written by the indexer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolRow {
    /// Fully qualified name (e.g. `my_crate::module::fn_name`).
    pub fqn: String,
    /// Absolute file path.
    pub path: PathBuf,
    /// 1-based start line.
    pub start_line: u32,
    /// 1-based end line (inclusive).
    pub end_line: u32,
    /// Coarse kind (`"function"`, `"type"`, `"trait"`, ...).
    pub kind: String,
}

/// Graph node (stored in the `nodes` table).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeRow {
    /// Node id — typically an FQN.
    pub id: String,
    /// Node kind tag.
    pub kind: String,
    /// Optional JSON payload with extra per-node data.
    pub payload: serde_json::Value,
}

/// Graph edge (stored in the `edges` table).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeRow {
    /// Source node id.
    pub src: String,
    /// Destination node id.
    pub dst: String,
    /// Edge kind (e.g. `"calls"`, `"imports"`, `"defines"`).
    pub kind: String,
    /// Optional JSON payload.
    pub payload: serde_json::Value,
}

// ---- handle ----------------------------------------------------------------

/// Handle to the redb-backed KV + graph store.
///
/// Cheap to clone; the underlying [`Database`] is shared behind an [`Arc`].
#[derive(Clone)]
pub struct KvStore {
    db: Arc<Database>,
    #[allow(dead_code)]
    path: PathBuf,
}

impl KvStore {
    /// Open (or create) the store at `path` (a file, not a directory).
    ///
    /// On first open the required tables are created so every subsequent
    /// read is guaranteed to find them.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let db_path = path.clone();
        let db = tokio::task::spawn_blocking(move || -> Result<Database> {
            let db = Database::create(&db_path).map_err(store)?;
            // Ensure all tables exist.
            let wtxn = db.begin_write().map_err(store)?;
            {
                let _ = wtxn.open_table(NODES).map_err(store)?;
                let _ = wtxn.open_table(EDGES).map_err(store)?;
                let _ = wtxn.open_table(SYMBOLS).map_err(store)?;
                let _ = wtxn.open_table(META).map_err(store)?;
            }
            wtxn.commit().map_err(store)?;
            Ok(db)
        })
        .await
        .map_err(|e| Error::Store(format!("join: {e}")))??;

        Ok(Self {
            db: Arc::new(db),
            path,
        })
    }

    // --- meta -----------------------------------------------------------

    /// Store a meta value (config, cursor, ...).
    pub async fn put_meta(&self, key: impl Into<String>, value: &[u8]) -> Result<()> {
        let db = self.db.clone();
        let key = key.into();
        let value = value.to_vec();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let wtxn = db.begin_write().map_err(store)?;
            {
                let mut t = wtxn.open_table(META).map_err(store)?;
                t.insert(key.as_str(), value.as_slice()).map_err(store)?;
            }
            wtxn.commit().map_err(store)?;
            Ok(())
        })
        .await
        .map_err(|e| Error::Store(format!("join: {e}")))?
    }

    /// Fetch a previously stored meta value.
    pub async fn get_meta(&self, key: impl Into<String>) -> Result<Option<Vec<u8>>> {
        let db = self.db.clone();
        let key = key.into();
        tokio::task::spawn_blocking(move || -> Result<Option<Vec<u8>>> {
            let rtxn = db.begin_read().map_err(store)?;
            let t = rtxn.open_table(META).map_err(store)?;
            Ok(t.get(key.as_str())
                .map_err(store)?
                .map(|g| g.value().to_vec()))
        })
        .await
        .map_err(|e| Error::Store(format!("join: {e}")))?
    }

    // --- symbols --------------------------------------------------------

    /// Insert or replace a symbol row. Key is the FQN.
    pub async fn put_symbol(&self, row: SymbolRow) -> Result<()> {
        let db = self.db.clone();
        let bytes = serde_json::to_vec(&row)?;
        tokio::task::spawn_blocking(move || -> Result<()> {
            let wtxn = db.begin_write().map_err(store)?;
            {
                let mut t = wtxn.open_table(SYMBOLS).map_err(store)?;
                t.insert(row.fqn.as_str(), bytes.as_slice())
                    .map_err(store)?;
            }
            wtxn.commit().map_err(store)?;
            Ok(())
        })
        .await
        .map_err(|e| Error::Store(format!("join: {e}")))?
    }

    /// Fetch a symbol row by FQN.
    pub async fn get_symbol(&self, fqn: impl Into<String>) -> Result<Option<SymbolRow>> {
        let db = self.db.clone();
        let fqn = fqn.into();
        tokio::task::spawn_blocking(move || -> Result<Option<SymbolRow>> {
            let rtxn = db.begin_read().map_err(store)?;
            let t = rtxn.open_table(SYMBOLS).map_err(store)?;
            let Some(g) = t.get(fqn.as_str()).map_err(store)? else {
                return Ok(None);
            };
            let row: SymbolRow = serde_json::from_slice(g.value())?;
            Ok(Some(row))
        })
        .await
        .map_err(|e| Error::Store(format!("join: {e}")))?
    }

    /// List every symbol whose FQN starts with `prefix`.
    pub async fn symbols_with_prefix(&self, prefix: impl Into<String>) -> Result<Vec<SymbolRow>> {
        let db = self.db.clone();
        let prefix = prefix.into();
        tokio::task::spawn_blocking(move || -> Result<Vec<SymbolRow>> {
            let rtxn = db.begin_read().map_err(store)?;
            let t = rtxn.open_table(SYMBOLS).map_err(store)?;
            let mut out = Vec::new();
            // redb does not expose true prefix scans on &str keys directly;
            // walk the full range and filter. At M2's scale this is fine.
            for entry in t.iter().map_err(store)? {
                let (k, v) = entry.map_err(store)?;
                if k.value().starts_with(prefix.as_str()) {
                    out.push(serde_json::from_slice(v.value())?);
                }
            }
            Ok(out)
        })
        .await
        .map_err(|e| Error::Store(format!("join: {e}")))?
    }

    // --- graph ----------------------------------------------------------

    /// Upsert a graph node.
    pub async fn put_node(&self, row: NodeRow) -> Result<()> {
        let db = self.db.clone();
        let bytes = serde_json::to_vec(&row)?;
        tokio::task::spawn_blocking(move || -> Result<()> {
            let wtxn = db.begin_write().map_err(store)?;
            {
                let mut t = wtxn.open_table(NODES).map_err(store)?;
                t.insert(row.id.as_str(), bytes.as_slice()).map_err(store)?;
            }
            wtxn.commit().map_err(store)?;
            Ok(())
        })
        .await
        .map_err(|e| Error::Store(format!("join: {e}")))?
    }

    /// Upsert a graph edge. Edge key is `"<src>|<kind>|<dst>"`.
    pub async fn put_edge(&self, row: EdgeRow) -> Result<()> {
        let db = self.db.clone();
        let key = format!("{}|{}|{}", row.src, row.kind, row.dst);
        let bytes = serde_json::to_vec(&row)?;
        tokio::task::spawn_blocking(move || -> Result<()> {
            let wtxn = db.begin_write().map_err(store)?;
            {
                let mut t = wtxn.open_table(EDGES).map_err(store)?;
                t.insert(key.as_str(), bytes.as_slice()).map_err(store)?;
            }
            wtxn.commit().map_err(store)?;
            Ok(())
        })
        .await
        .map_err(|e| Error::Store(format!("join: {e}")))?
    }

    /// List every outgoing edge from `src`.
    pub async fn edges_from(&self, src: impl Into<String>) -> Result<Vec<EdgeRow>> {
        let db = self.db.clone();
        let src = src.into();
        tokio::task::spawn_blocking(move || -> Result<Vec<EdgeRow>> {
            let rtxn = db.begin_read().map_err(store)?;
            let t = rtxn.open_table(EDGES).map_err(store)?;
            let mut out = Vec::new();
            let needle = format!("{src}|");
            for entry in t.iter().map_err(store)? {
                let (k, v) = entry.map_err(store)?;
                if k.value().starts_with(needle.as_str()) {
                    out.push(serde_json::from_slice(v.value())?);
                }
            }
            Ok(out)
        })
        .await
        .map_err(|e| Error::Store(format!("join: {e}")))?
    }

    /// List every incoming edge to `dst`. Walks the entire `edges` table;
    /// fine at M3 scale but worth revisiting once edge counts grow.
    pub async fn edges_to(&self, dst: impl Into<String>) -> Result<Vec<EdgeRow>> {
        let db = self.db.clone();
        let dst = dst.into();
        tokio::task::spawn_blocking(move || -> Result<Vec<EdgeRow>> {
            let rtxn = db.begin_read().map_err(store)?;
            let t = rtxn.open_table(EDGES).map_err(store)?;
            let mut out = Vec::new();
            let needle = format!("|{dst}");
            for entry in t.iter().map_err(store)? {
                let (k, v) = entry.map_err(store)?;
                if k.value().ends_with(needle.as_str()) {
                    out.push(serde_json::from_slice(v.value())?);
                }
            }
            Ok(out)
        })
        .await
        .map_err(|e| Error::Store(format!("join: {e}")))?
    }

    /// Get a node by id.
    pub async fn get_node(&self, id: impl Into<String>) -> Result<Option<NodeRow>> {
        let db = self.db.clone();
        let id = id.into();
        tokio::task::spawn_blocking(move || -> Result<Option<NodeRow>> {
            let rtxn = db.begin_read().map_err(store)?;
            let t = rtxn.open_table(NODES).map_err(store)?;
            let Some(g) = t.get(id.as_str()).map_err(store)? else {
                return Ok(None);
            };
            Ok(Some(serde_json::from_slice(g.value())?))
        })
        .await
        .map_err(|e| Error::Store(format!("join: {e}")))?
    }
}

// ---- helpers ---------------------------------------------------------------

fn store<E: std::fmt::Display>(e: E) -> Error {
    Error::Store(e.to_string())
}

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

    /// Remove a meta value. No-op if the key was never set. Returns
    /// whether a row was actually removed.
    pub async fn delete_meta(&self, key: impl Into<String>) -> Result<bool> {
        let db = self.db.clone();
        let key = key.into();
        tokio::task::spawn_blocking(move || -> Result<bool> {
            let wtxn = db.begin_write().map_err(store)?;
            let removed = {
                let mut t = wtxn.open_table(META).map_err(store)?;
                t.remove(key.as_str()).map_err(store)?.is_some()
            };
            wtxn.commit().map_err(store)?;
            Ok(removed)
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

    /// Insert many symbol rows in a single redb transaction.
    pub async fn put_symbols_batch(&self, rows: Vec<SymbolRow>) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let wtxn = db.begin_write().map_err(store)?;
            {
                let mut t = wtxn.open_table(SYMBOLS).map_err(store)?;
                for row in &rows {
                    let bytes = serde_json::to_vec(row)?;
                    t.insert(row.fqn.as_str(), bytes.as_slice())
                        .map_err(store)?;
                }
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
            // Use a range scan starting at `prefix` and stop as soon as the
            // key no longer shares the prefix — O(k) instead of O(N).
            for entry in t.range(prefix.as_str()..).map_err(store)? {
                let (k, v) = entry.map_err(store)?;
                if !k.value().starts_with(prefix.as_str()) {
                    break;
                }
                out.push(serde_json::from_slice(v.value())?);
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

    /// Insert many graph nodes in a single redb transaction.
    pub async fn put_nodes_batch(&self, rows: Vec<NodeRow>) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let wtxn = db.begin_write().map_err(store)?;
            {
                let mut t = wtxn.open_table(NODES).map_err(store)?;
                for row in &rows {
                    let bytes = serde_json::to_vec(row)?;
                    t.insert(row.id.as_str(), bytes.as_slice()).map_err(store)?;
                }
            }
            wtxn.commit().map_err(store)?;
            Ok(())
        })
        .await
        .map_err(|e| Error::Store(format!("join: {e}")))?
    }

    /// Insert many graph edges in a single redb transaction.
    pub async fn put_edges_batch(&self, rows: Vec<EdgeRow>) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let wtxn = db.begin_write().map_err(store)?;
            {
                let mut t = wtxn.open_table(EDGES).map_err(store)?;
                for row in &rows {
                    let key = format!("{}|{}|{}", row.src, row.kind, row.dst);
                    let bytes = serde_json::to_vec(row)?;
                    t.insert(key.as_str(), bytes.as_slice()).map_err(store)?;
                }
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

    /// Delete every symbol row whose `path` matches `path`, returning the
    /// FQNs that were removed so the caller can also prune the graph nodes
    /// and edges that referenced them.
    ///
    /// There is no secondary index on `path` — this walks the whole symbols
    /// table. Fine at our scale; if it ever isn't we can add a `path → fqn`
    /// side table.
    pub async fn delete_symbols_by_path(&self, path: impl AsRef<Path>) -> Result<Vec<String>> {
        let db = self.db.clone();
        let path = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
            let rtxn = db.begin_read().map_err(store)?;
            let t = rtxn.open_table(SYMBOLS).map_err(store)?;
            let mut keys: Vec<String> = Vec::new();
            for entry in t.iter().map_err(store)? {
                let (k, v) = entry.map_err(store)?;
                let row: SymbolRow = match serde_json::from_slice(v.value()) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                if row.path == path {
                    keys.push(k.value().to_string());
                }
            }
            drop(t);
            drop(rtxn);

            if keys.is_empty() {
                return Ok(keys);
            }
            let wtxn = db.begin_write().map_err(store)?;
            {
                let mut t = wtxn.open_table(SYMBOLS).map_err(store)?;
                for k in &keys {
                    t.remove(k.as_str()).map_err(store)?;
                }
            }
            wtxn.commit().map_err(store)?;
            Ok(keys)
        })
        .await
        .map_err(|e| Error::Store(format!("join: {e}")))?
    }

    /// Every symbol row whose declared path equals `path`. Same O(table-scan)
    /// cost as [`Self::delete_symbols_by_path`]; fine while the number of
    /// indexed symbols stays in the low millions. Returned order matches the
    /// underlying key sort (FQN), which is stable across calls.
    pub async fn symbols_for_path(&self, path: impl AsRef<Path>) -> Result<Vec<SymbolRow>> {
        let db = self.db.clone();
        let path = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || -> Result<Vec<SymbolRow>> {
            let rtxn = db.begin_read().map_err(store)?;
            let t = rtxn.open_table(SYMBOLS).map_err(store)?;
            let mut out = Vec::new();
            for entry in t.iter().map_err(store)? {
                let (_k, v) = entry.map_err(store)?;
                let row: SymbolRow = match serde_json::from_slice(v.value()) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                if row.path == path {
                    out.push(row);
                }
            }
            Ok(out)
        })
        .await
        .map_err(|e| Error::Store(format!("join: {e}")))?
    }

    /// Delete every graph node whose JSON payload `path` field matches
    /// `path`. Returns the list of node ids removed (so the caller can clean
    /// up the edges that touch them).
    pub async fn delete_nodes_by_path(&self, path: impl AsRef<Path>) -> Result<Vec<String>> {
        let db = self.db.clone();
        let path_str = path.as_ref().to_string_lossy().into_owned();
        tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
            let rtxn = db.begin_read().map_err(store)?;
            let t = rtxn.open_table(NODES).map_err(store)?;
            let mut keys: Vec<String> = Vec::new();
            for entry in t.iter().map_err(store)? {
                let (k, v) = entry.map_err(store)?;
                let row: NodeRow = match serde_json::from_slice(v.value()) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                let p = row.payload.get("path").and_then(|x| x.as_str());
                if p == Some(path_str.as_str()) {
                    keys.push(k.value().to_string());
                }
            }
            drop(t);
            drop(rtxn);

            if keys.is_empty() {
                return Ok(keys);
            }
            let wtxn = db.begin_write().map_err(store)?;
            {
                let mut t = wtxn.open_table(NODES).map_err(store)?;
                for k in &keys {
                    t.remove(k.as_str()).map_err(store)?;
                }
            }
            wtxn.commit().map_err(store)?;
            Ok(keys)
        })
        .await
        .map_err(|e| Error::Store(format!("join: {e}")))?
    }

    /// Delete every edge whose `src` or `dst` appears in `ids`.
    pub async fn delete_edges_touching(&self, ids: &[String]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let db = self.db.clone();
        let ids: std::collections::HashSet<String> = ids.iter().cloned().collect();
        tokio::task::spawn_blocking(move || -> Result<usize> {
            let rtxn = db.begin_read().map_err(store)?;
            let t = rtxn.open_table(EDGES).map_err(store)?;
            let mut to_drop: Vec<String> = Vec::new();
            for entry in t.iter().map_err(store)? {
                let (k, v) = entry.map_err(store)?;
                let row: EdgeRow = match serde_json::from_slice(v.value()) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                if ids.contains(&row.src) || ids.contains(&row.dst) {
                    to_drop.push(k.value().to_string());
                }
            }
            drop(t);
            drop(rtxn);

            if to_drop.is_empty() {
                return Ok(0);
            }
            let n = to_drop.len();
            let wtxn = db.begin_write().map_err(store)?;
            {
                let mut t = wtxn.open_table(EDGES).map_err(store)?;
                for k in &to_drop {
                    t.remove(k.as_str()).map_err(store)?;
                }
            }
            wtxn.commit().map_err(store)?;
            Ok(n)
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

    /// Return every graph node whose payload `path` field matches `path`.
    ///
    /// There is no secondary index on `path` — this walks the whole nodes
    /// table. Fine at our scale (called at most `top_k` times per query) and
    /// symmetric with [`Self::delete_nodes_by_path`].
    pub async fn nodes_for_path(&self, path: impl AsRef<Path>) -> Result<Vec<NodeRow>> {
        let db = self.db.clone();
        let path_str = path.as_ref().to_string_lossy().into_owned();
        tokio::task::spawn_blocking(move || -> Result<Vec<NodeRow>> {
            let rtxn = db.begin_read().map_err(store)?;
            let t = rtxn.open_table(NODES).map_err(store)?;
            let mut out = Vec::new();
            for entry in t.iter().map_err(store)? {
                let (_k, v) = entry.map_err(store)?;
                let row: NodeRow = match serde_json::from_slice(v.value()) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                let p = row.payload.get("path").and_then(|x| x.as_str());
                if p == Some(path_str.as_str()) {
                    out.push(row);
                }
            }
            Ok(out)
        })
        .await
        .map_err(|e| Error::Store(format!("join: {e}")))?
    }
}

// ---- helpers ---------------------------------------------------------------

fn store<E: std::fmt::Display>(e: E) -> Error {
    Error::Store(e.to_string())
}

//! Vector store — SQLite-backed flat cosine index.
//!
//! Trades peak recall latency for zero-dep simplicity: every vector is stored
//! as a little-endian `f32` blob alongside its id and model tag, and search
//! does a linear scan computing cosine similarity in pure Rust. Plenty fast
//! for the <100k-chunk codebases Thoth is aimed at; we can swap in LanceDB
//! later under the same interface.
//!
//! Schema:
//!
//! ```sql
//! CREATE TABLE vectors(
//!     id    TEXT PRIMARY KEY,
//!     model TEXT NOT NULL,
//!     dim   INTEGER NOT NULL,
//!     vec   BLOB NOT NULL     -- little-endian f32 array, len = dim * 4
//! );
//! CREATE INDEX idx_vectors_model ON vectors(model);
//! ```
//!
//! All vectors are **L2-normalised on write**, which reduces cosine
//! similarity to a dot product at query time.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use rusqlite::{Connection, params};
use thoth_core::{Error, Result};

/// A single vector hit.
#[derive(Debug, Clone)]
pub struct VectorHit {
    /// Chunk id this vector points at.
    pub id: String,
    /// Cosine similarity in `[-1.0, 1.0]` (higher is better).
    pub score: f32,
}

/// Handle to the vector index. Cheap to clone.
#[derive(Clone)]
pub struct VectorStore {
    conn: Arc<Mutex<Connection>>,
    #[allow(dead_code)]
    path: PathBuf,
}

impl VectorStore {
    /// Open (or create) a vector index at `path` (a `.sqlite` file).
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let p2 = path.clone();

        let conn = tokio::task::spawn_blocking(move || -> Result<Connection> {
            let c = Connection::open(&p2).map_err(store)?;
            c.pragma_update(None, "journal_mode", "WAL")
                .map_err(store)?;
            c.pragma_update(None, "synchronous", "NORMAL")
                .map_err(store)?;
            c.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS vectors (
                    id    TEXT PRIMARY KEY,
                    model TEXT NOT NULL,
                    dim   INTEGER NOT NULL,
                    vec   BLOB NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_vectors_model ON vectors(model);
                "#,
            )
            .map_err(store)?;
            Ok(c)
        })
        .await
        .map_err(|e| Error::Store(format!("join: {e}")))??;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            path,
        })
    }

    /// Upsert a single `(id, vector)` pair under `model`. The vector is
    /// L2-normalised before being stored.
    pub async fn upsert(&self, id: &str, model: &str, vector: &[f32]) -> Result<()> {
        let id = id.to_string();
        let model = model.to_string();
        let dim = vector.len() as i64;
        let normalised = normalise(vector);
        let blob = f32s_to_bytes(&normalised);

        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let c = conn.lock();
            c.execute(
                r#"INSERT INTO vectors(id, model, dim, vec)
                   VALUES (?1, ?2, ?3, ?4)
                   ON CONFLICT(id) DO UPDATE SET
                       model = excluded.model,
                       dim   = excluded.dim,
                       vec   = excluded.vec"#,
                params![id, model, dim, blob],
            )
            .map_err(store)?;
            Ok(())
        })
        .await
        .map_err(|e| Error::Store(format!("join: {e}")))?
    }

    /// Upsert a batch in one transaction.
    pub async fn upsert_batch(&self, items: &[(String, Vec<f32>)], model: &str) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let model = model.to_string();
        let prepared: Vec<(String, i64, Vec<u8>)> = items
            .iter()
            .map(|(id, v)| {
                let dim = v.len() as i64;
                let blob = f32s_to_bytes(&normalise(v));
                (id.clone(), dim, blob)
            })
            .collect();

        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut c = conn.lock();
            let tx = c.transaction().map_err(store)?;
            {
                let mut stmt = tx
                    .prepare(
                        r#"INSERT INTO vectors(id, model, dim, vec)
                           VALUES (?1, ?2, ?3, ?4)
                           ON CONFLICT(id) DO UPDATE SET
                               model = excluded.model,
                               dim   = excluded.dim,
                               vec   = excluded.vec"#,
                    )
                    .map_err(store)?;
                for (id, dim, blob) in &prepared {
                    stmt.execute(params![id, model, dim, blob]).map_err(store)?;
                }
            }
            tx.commit().map_err(store)?;
            Ok(())
        })
        .await
        .map_err(|e| Error::Store(format!("join: {e}")))?
    }

    /// Delete every vector whose id begins with `"{path}:"`.
    ///
    /// The indexer uses `chunk_id = "{path}:{start}-{end}"` so every chunk
    /// that belongs to a given file shares the same prefix. Returns the
    /// number of rows deleted.
    pub async fn delete_by_path(&self, path: &str) -> Result<u64> {
        let prefix = format!("{path}:");
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<u64> {
            let c = conn.lock();
            // `LIKE` with an explicit ESCAPE so ids containing `%` or `_`
            // (rare in practice, but possible) don't widen the match.
            let esc = prefix
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            let pattern = format!("{esc}%");
            let n = c
                .execute(
                    "DELETE FROM vectors WHERE id LIKE ?1 ESCAPE '\\'",
                    params![pattern],
                )
                .map_err(store)?;
            Ok(n as u64)
        })
        .await
        .map_err(|e| Error::Store(format!("join: {e}")))?
    }

    /// Delete a single vector by id.
    pub async fn delete(&self, id: &str) -> Result<()> {
        let id = id.to_string();
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let c = conn.lock();
            c.execute("DELETE FROM vectors WHERE id = ?1", params![id])
                .map_err(store)?;
            Ok(())
        })
        .await
        .map_err(|e| Error::Store(format!("join: {e}")))?
    }

    /// Top-k nearest neighbours by cosine similarity.
    ///
    /// Only rows stored under `model` are considered — this guards against
    /// silently mixing embeddings from different models. Vectors with a
    /// dimension that doesn't match the query are skipped.
    pub async fn search(&self, model: &str, query: &[f32], k: usize) -> Result<Vec<VectorHit>> {
        if query.is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        let q = normalise(query);
        let q_dim = q.len();
        let model = model.to_string();

        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<VectorHit>> {
            let c = conn.lock();
            let mut stmt = c
                .prepare("SELECT id, dim, vec FROM vectors WHERE model = ?1")
                .map_err(store)?;
            let mut rows = stmt.query(params![model]).map_err(store)?;

            // Min-heap of fixed size k.
            let mut heap: std::collections::BinaryHeap<std::cmp::Reverse<ScoreEntry>> =
                std::collections::BinaryHeap::with_capacity(k + 1);

            while let Some(row) = rows.next().map_err(store)? {
                let id: String = row.get(0).map_err(store)?;
                let dim: i64 = row.get(1).map_err(store)?;
                if dim as usize != q_dim {
                    continue;
                }
                let blob: Vec<u8> = row.get(2).map_err(store)?;
                let v = bytes_to_f32s(&blob);
                if v.len() != q_dim {
                    continue;
                }
                // Both sides are already L2-normalised, so cosine = dot.
                let score = dot(&q, &v);
                heap.push(std::cmp::Reverse(ScoreEntry { id, score }));
                if heap.len() > k {
                    heap.pop();
                }
            }

            let mut out: Vec<VectorHit> = heap
                .into_iter()
                .map(|std::cmp::Reverse(e)| VectorHit {
                    id: e.id,
                    score: e.score,
                })
                .collect();
            // BinaryHeap's iter order is unspecified, sort descending here.
            out.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            Ok(out)
        })
        .await
        .map_err(|e| Error::Store(format!("join: {e}")))?
    }

    /// Total number of stored vectors across all models.
    pub async fn count(&self) -> Result<i64> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<i64> {
            let c = conn.lock();
            let n: i64 = c
                .query_row("SELECT COUNT(*) FROM vectors", [], |r| r.get(0))
                .map_err(store)?;
            Ok(n)
        })
        .await
        .map_err(|e| Error::Store(format!("join: {e}")))?
    }
}

// ---- numeric helpers -------------------------------------------------------

fn normalise(v: &[f32]) -> Vec<f32> {
    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n == 0.0 {
        return v.to_vec();
    }
    v.iter().map(|x| x / n).collect()
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

fn f32s_to_bytes(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

fn bytes_to_f32s(b: &[u8]) -> Vec<f32> {
    let mut out = Vec::with_capacity(b.len() / 4);
    for chunk in b.chunks_exact(4) {
        out.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    out
}

#[derive(Debug, Clone)]
struct ScoreEntry {
    id: String,
    score: f32,
}

impl PartialEq for ScoreEntry {
    fn eq(&self, other: &Self) -> bool {
        self.score == other.score
    }
}
impl Eq for ScoreEntry {}
impl PartialOrd for ScoreEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for ScoreEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.score
            .partial_cmp(&other.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    }
}

fn store<E: std::fmt::Display>(e: E) -> Error {
    Error::Store(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn upsert_search_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.sqlite");
        let vs = VectorStore::open(&path).await.unwrap();

        vs.upsert("a", "m", &[1.0, 0.0, 0.0]).await.unwrap();
        vs.upsert("b", "m", &[0.0, 1.0, 0.0]).await.unwrap();
        vs.upsert("c", "m", &[0.7, 0.7, 0.0]).await.unwrap();

        let hits = vs.search("m", &[1.0, 0.0, 0.0], 2).await.unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, "a");
        assert!(hits[0].score > hits[1].score);
    }

    #[tokio::test]
    async fn different_model_isolated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.sqlite");
        let vs = VectorStore::open(&path).await.unwrap();
        vs.upsert("a", "m1", &[1.0, 0.0]).await.unwrap();
        vs.upsert("b", "m2", &[1.0, 0.0]).await.unwrap();

        let h1 = vs.search("m1", &[1.0, 0.0], 10).await.unwrap();
        assert_eq!(h1.len(), 1);
        assert_eq!(h1[0].id, "a");
    }

    #[tokio::test]
    async fn dim_mismatch_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.sqlite");
        let vs = VectorStore::open(&path).await.unwrap();
        vs.upsert("a", "m", &[1.0, 0.0, 0.0]).await.unwrap();

        let hits = vs.search("m", &[1.0, 0.0], 1).await.unwrap();
        assert!(hits.is_empty());
    }
}

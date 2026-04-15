//! LanceDB-backed vector store (feature-gated).
//!
//! Enable with the `lance` Cargo feature. The public surface is intentionally
//! identical to [`crate::VectorStore`] (the SQLite flat-cosine default) so
//! swapping between them is a type substitution — see `DESIGN.md` §3 and §12.
//!
//! ## Layout
//!
//! A single [`lancedb::Connection`] is opened at `path`. Each distinct
//! embedding `model` gets its **own LanceDB table** (Arrow schemas are
//! fixed-width, and different models have different dims, so mixing them in
//! one table would force every row to the widest vector length). Tables are
//! named after the sanitised model string and created lazily on first
//! upsert — the first write fixes the dim for the life of the table.
//!
//! ## Schema (per model)
//!
//! | column | type                              | notes                  |
//! |--------|-----------------------------------|------------------------|
//! | `id`   | `Utf8`                            | primary key for merges |
//! | `vec`  | `FixedSizeList<Float32, dim>`     | L2-normalised          |
//!
//! ## Similarity
//!
//! Vectors are L2-normalised on write (same as `VectorStore`). Search uses
//! [`DistanceType::Cosine`], and we report `score = 1.0 - distance` so the
//! returned number is in `[-1.0, 1.0]` with "higher is better" — matching
//! the contract of [`VectorHit::score`](crate::vector::VectorHit::score).
//!
//! ## API-drift notes
//!
//! `lancedb` has moved faster than its 0.x semver would suggest. If a future
//! cargo check fails here, the most likely spots to adjust are:
//!
//! - `lancedb::connect(uri).execute().await` — connection builder shape
//! - `Connection::create_table(..).execute().await` — table create builder
//! - `MergeInsertBuilder` method names (`when_matched_update_all`,
//!   `when_not_matched_insert_all`, `execute`)
//! - `Query::nearest_to(..)?.distance_type(..).limit(..).execute()`
//! - The `_distance` column name returned by vector search
//!
//! Each of those is isolated to one helper below, so drift should be
//! pinpoint-fixable rather than structural.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::{
    Array, FixedSizeListArray, Float32Array, RecordBatch, RecordBatchIterator, StringArray,
};
use arrow_schema::{DataType, Field, Schema};
use futures::TryStreamExt;
use lancedb::{
    Connection, DistanceType, connect,
    query::{ExecutableQuery, QueryBase},
    table::Table,
};
use tokio::sync::Mutex;

use thoth_core::{Error, Result};

use crate::vector::VectorHit;

/// LanceDB-backed vector store. Cheap to clone.
#[derive(Clone)]
pub struct LanceVectorStore {
    inner: Arc<Inner>,
}

struct Inner {
    conn: Connection,
    #[allow(dead_code)]
    path: PathBuf,
    /// Handle cache per model. Populated on first upsert / first search.
    /// Guarded by a Mutex because table creation is an async op we don't
    /// want to race on with two concurrent upserts under a new model.
    tables: Mutex<HashMap<String, Table>>,
}

impl LanceVectorStore {
    /// Open (or create) a LanceDB dataset rooted at `path`. The path is
    /// treated as a directory — LanceDB manages one subdirectory per table.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // LanceDB expects the root itself to exist.
        tokio::fs::create_dir_all(&path).await?;

        let uri = path.to_string_lossy().to_string();
        let conn = connect(&uri).execute().await.map_err(store)?;

        Ok(Self {
            inner: Arc::new(Inner {
                conn,
                path,
                tables: Mutex::new(HashMap::new()),
            }),
        })
    }

    /// Upsert a single `(id, vector)` pair under `model`.
    pub async fn upsert(&self, id: &str, model: &str, vector: &[f32]) -> Result<()> {
        self.upsert_batch(&[(id.to_string(), vector.to_vec())], model)
            .await
    }

    /// Upsert a batch under a single `model`. All vectors in `items` must
    /// share the same dimension; mismatches return an error rather than
    /// silently storing a mangled row.
    pub async fn upsert_batch(&self, items: &[(String, Vec<f32>)], model: &str) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let dim = items[0].1.len();
        if dim == 0 {
            return Err(Error::Store("upsert_batch: empty vector".into()));
        }
        for (id, v) in items {
            if v.len() != dim {
                return Err(Error::Store(format!(
                    "upsert_batch: dim mismatch for id={id}: expected {dim}, got {}",
                    v.len()
                )));
            }
        }

        let table = self.get_or_create_table(model, dim).await?;
        let batch = build_record_batch(items, dim)?;
        let schema = batch.schema();
        let reader = RecordBatchIterator::new(vec![Ok(batch)], schema);

        // Upsert semantics: rows whose `id` already exists get their `vec`
        // overwritten; new rows get inserted. Matches the SQLite store's
        // `ON CONFLICT(id) DO UPDATE` behaviour.
        //
        // `MergeInsertBuilder` in lancedb 0.10 uses an `&mut self` builder
        // style (each method returns `&mut Self`), so we can't fluent-chain
        // all the way into `.execute()` — bind mutably and stage the config
        // as statements, then execute.
        let mut builder = table.merge_insert(&["id"]);
        builder.when_matched_update_all(None);
        builder.when_not_matched_insert_all();
        builder.execute(Box::new(reader)).await.map_err(store)?;
        Ok(())
    }

    /// Top-k nearest neighbours by cosine similarity, restricted to `model`.
    /// Rows whose stored dim doesn't match `query.len()` are not an error —
    /// we just return an empty result set, since with per-model tables a
    /// dim mismatch means the caller passed the wrong model.
    pub async fn search(&self, model: &str, query: &[f32], k: usize) -> Result<Vec<VectorHit>> {
        if query.is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        let table = match self.open_existing_table(model).await? {
            Some(t) => t,
            None => return Ok(Vec::new()),
        };

        // Guard against dim mismatch: the table's schema is fixed, so if
        // the caller supplied a query of the wrong width, LanceDB would
        // error mid-query. Check up front and short-circuit to empty.
        let tbl_dim = table_vector_dim(&table).await?;
        if tbl_dim != query.len() {
            return Ok(Vec::new());
        }

        let q = normalise(query);
        let stream = table
            .query()
            .nearest_to(q.clone())
            .map_err(store)?
            .distance_type(DistanceType::Cosine)
            .limit(k)
            .execute()
            .await
            .map_err(store)?;
        let batches: Vec<RecordBatch> = stream.try_collect().await.map_err(store)?;

        let mut out = Vec::new();
        for b in &batches {
            let ids = b
                .column_by_name("id")
                .ok_or_else(|| Error::Store("search: missing id column".into()))?
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| Error::Store("search: id column not Utf8".into()))?;
            // LanceDB surfaces the similarity in `_distance`. With
            // DistanceType::Cosine this is `1 - cos(a, b)` — flip it so
            // we expose cosine directly (higher = better) like
            // `VectorStore::search` does.
            let dists = b
                .column_by_name("_distance")
                .ok_or_else(|| Error::Store("search: missing _distance column".into()))?
                .as_any()
                .downcast_ref::<Float32Array>()
                .ok_or_else(|| Error::Store("search: _distance not Float32".into()))?;

            for i in 0..b.num_rows() {
                if ids.is_null(i) || dists.is_null(i) {
                    continue;
                }
                let id = ids.value(i).to_string();
                let d = dists.value(i);
                out.push(VectorHit { id, score: 1.0 - d });
            }
        }
        out.truncate(k);
        Ok(out)
    }

    /// Delete every row whose id begins with `"{path}:"`, across every
    /// model table. Returns the total number of rows deleted.
    pub async fn delete_by_path(&self, path: &str) -> Result<u64> {
        let prefix = format!("{path}:");
        // LanceDB 0.17's DataFusion dialect does not support ESCAPE in LIKE.
        // Use the `starts_with` scalar function instead — it handles any
        // character in the path without needing special escaping.
        let esc = prefix.replace('\'', "''"); // only SQL single-quote needs escaping
        let predicate = format!("starts_with(id, '{esc}')");

        let mut total = 0u64;
        for table in self.all_tables().await? {
            // count → delete → report. LanceDB's `delete` doesn't return
            // a row count, so we ask first. For large tables this is an
            // extra scan, but `delete_by_path` is a per-file op so the
            // cost is bounded.
            let n = table
                .count_rows(Some(predicate.clone()))
                .await
                .map_err(store)? as u64;
            if n == 0 {
                continue;
            }
            table.delete(&predicate).await.map_err(store)?;
            total += n;
        }
        Ok(total)
    }

    /// Delete a single row by id. Searches every model table because ids
    /// don't carry their model — the cost is one small predicate per
    /// table, and the row count on each is typically modest.
    pub async fn delete(&self, id: &str) -> Result<()> {
        let esc = id.replace('\'', "''");
        let predicate = format!("id = '{esc}'");
        for table in self.all_tables().await? {
            table.delete(&predicate).await.map_err(store)?;
        }
        Ok(())
    }

    /// Total number of stored vectors across every model table.
    pub async fn count(&self) -> Result<i64> {
        let mut total = 0i64;
        for table in self.all_tables().await? {
            total += table.count_rows(None).await.map_err(store)? as i64;
        }
        Ok(total)
    }

    // ---- internals --------------------------------------------------------

    /// Return the handle for `model`, creating an empty table with the
    /// right schema if it doesn't exist yet. Cached after the first call.
    async fn get_or_create_table(&self, model: &str, dim: usize) -> Result<Table> {
        let key = sanitise_model(model);
        {
            let cache = self.inner.tables.lock().await;
            if let Some(t) = cache.get(&key) {
                return Ok(t.clone());
            }
        }
        // Either the table exists on disk from a prior session or we need
        // to create it. Try opening first; on "not found", create.
        let mut cache = self.inner.tables.lock().await;
        if let Some(t) = cache.get(&key) {
            // Someone else created it while we were waiting on the lock.
            return Ok(t.clone());
        }

        let existing_names = self
            .inner
            .conn
            .table_names()
            .execute()
            .await
            .map_err(store)?;
        let table = if existing_names.iter().any(|n| n == &key) {
            self.inner
                .conn
                .open_table(&key)
                .execute()
                .await
                .map_err(store)?
        } else {
            let schema = vector_schema(dim);
            let empty_batch = RecordBatch::new_empty(schema.clone());
            let reader = RecordBatchIterator::new(vec![Ok(empty_batch)], schema);
            self.inner
                .conn
                .create_table(&key, Box::new(reader))
                .execute()
                .await
                .map_err(store)?
        };
        cache.insert(key, table.clone());
        Ok(table)
    }

    /// Return the handle for `model` iff the table exists — no creation.
    /// Used by `search`, which should return "nothing" rather than
    /// materialise an empty table for an unknown model.
    async fn open_existing_table(&self, model: &str) -> Result<Option<Table>> {
        let key = sanitise_model(model);
        {
            let cache = self.inner.tables.lock().await;
            if let Some(t) = cache.get(&key) {
                return Ok(Some(t.clone()));
            }
        }
        let names = self
            .inner
            .conn
            .table_names()
            .execute()
            .await
            .map_err(store)?;
        if !names.iter().any(|n| n == &key) {
            return Ok(None);
        }
        let table = self
            .inner
            .conn
            .open_table(&key)
            .execute()
            .await
            .map_err(store)?;
        let mut cache = self.inner.tables.lock().await;
        cache.insert(key, table.clone());
        Ok(Some(table))
    }

    /// Open every table on disk (not just cached ones). Needed by
    /// delete/count operations that must fan out across all models.
    async fn all_tables(&self) -> Result<Vec<Table>> {
        let names = self
            .inner
            .conn
            .table_names()
            .execute()
            .await
            .map_err(store)?;
        let mut out = Vec::with_capacity(names.len());
        // Reuse cached handles where possible; open fresh otherwise.
        let mut cache = self.inner.tables.lock().await;
        for name in names {
            if let Some(t) = cache.get(&name) {
                out.push(t.clone());
                continue;
            }
            let t = self
                .inner
                .conn
                .open_table(&name)
                .execute()
                .await
                .map_err(store)?;
            cache.insert(name, t.clone());
            out.push(t);
        }
        Ok(out)
    }
}

// ---- schema / Arrow helpers ------------------------------------------------

fn vector_schema(dim: usize) -> Arc<Schema> {
    let item = Arc::new(Field::new("item", DataType::Float32, true));
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("vec", DataType::FixedSizeList(item, dim as i32), false),
    ]))
}

fn build_record_batch(items: &[(String, Vec<f32>)], dim: usize) -> Result<RecordBatch> {
    let schema = vector_schema(dim);
    let ids = StringArray::from_iter_values(items.iter().map(|(id, _)| id.as_str()));

    // Flatten all vectors into a single Float32Array for the backing
    // storage of the FixedSizeListArray. Each vector is L2-normalised.
    let mut flat = Vec::with_capacity(items.len() * dim);
    for (_, v) in items {
        let n = normalise(v);
        flat.extend_from_slice(&n);
    }
    let values = Float32Array::from(flat);
    let item_field = Arc::new(Field::new("item", DataType::Float32, true));
    let vec_arr = FixedSizeListArray::try_new(item_field, dim as i32, Arc::new(values), None)
        .map_err(|e| Error::Store(format!("arrow: {e}")))?;

    RecordBatch::try_new(schema, vec![Arc::new(ids), Arc::new(vec_arr)])
        .map_err(|e| Error::Store(format!("arrow: {e}")))
}

/// Pull the per-row vector dim out of a table's schema. Used to short-circuit
/// a mismatched query before LanceDB errors out mid-search.
async fn table_vector_dim(table: &Table) -> Result<usize> {
    let schema = table.schema().await.map_err(store)?;
    let field = schema
        .field_with_name("vec")
        .map_err(|e| Error::Store(format!("arrow: {e}")))?;
    match field.data_type() {
        DataType::FixedSizeList(_, dim) => Ok(*dim as usize),
        other => Err(Error::Store(format!(
            "lance: unexpected `vec` column type {other:?}"
        ))),
    }
}

// ---- misc helpers ----------------------------------------------------------

fn normalise(v: &[f32]) -> Vec<f32> {
    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n == 0.0 {
        return v.to_vec();
    }
    v.iter().map(|x| x / n).collect()
}

/// Model names become table names on disk, so strip anything that could
/// collide with filesystem / LanceDB conventions. We keep the mapping
/// deterministic: same `model` string always hashes to the same table.
fn sanitise_model(model: &str) -> String {
    let mut out = String::with_capacity(model.len());
    for ch in model.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "default".to_string()
    } else {
        out
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
        let vs = LanceVectorStore::open(dir.path().join("lance"))
            .await
            .unwrap();

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
        let vs = LanceVectorStore::open(dir.path().join("lance"))
            .await
            .unwrap();
        vs.upsert("a", "m1", &[1.0, 0.0]).await.unwrap();
        vs.upsert("b", "m2", &[1.0, 0.0]).await.unwrap();

        let h1 = vs.search("m1", &[1.0, 0.0], 10).await.unwrap();
        assert_eq!(h1.len(), 1);
        assert_eq!(h1[0].id, "a");
    }

    #[tokio::test]
    async fn dim_mismatch_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let vs = LanceVectorStore::open(dir.path().join("lance"))
            .await
            .unwrap();
        vs.upsert("a", "m", &[1.0, 0.0, 0.0]).await.unwrap();

        let hits = vs.search("m", &[1.0, 0.0], 1).await.unwrap();
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn delete_by_path_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let vs = LanceVectorStore::open(dir.path().join("lance"))
            .await
            .unwrap();
        vs.upsert("src/a.rs:0-10", "m", &[1.0, 0.0]).await.unwrap();
        vs.upsert("src/a.rs:10-20", "m", &[0.0, 1.0]).await.unwrap();
        vs.upsert("src/b.rs:0-10", "m", &[1.0, 0.0]).await.unwrap();

        let n = vs.delete_by_path("src/a.rs").await.unwrap();
        assert_eq!(n, 2);
        assert_eq!(vs.count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn unknown_model_search_empty() {
        let dir = tempfile::tempdir().unwrap();
        let vs = LanceVectorStore::open(dir.path().join("lance"))
            .await
            .unwrap();
        let hits = vs.search("never-seen", &[1.0, 0.0], 5).await.unwrap();
        assert!(hits.is_empty());
    }
}

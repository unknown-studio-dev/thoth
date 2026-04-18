//! Backend-agnostic contract tests for [`thoth_store::VectorBackend`].
//!
//! Each test is written once, generic over `impl VectorBackend`, and
//! dispatched against every compiled backend: `VectorStore` (SQLite flat
//! cosine, always-on) and `LanceVectorStore` (only when built with
//! `--features lance`).
//!
//! Adding a third backend later = implement `VectorBackend` for it and
//! add a `#[tokio::test]` in its own `mod` below.

use tempfile::TempDir;
use thoth_store::{VectorBackend, VectorStore};

const MODEL: &str = "test-model-3";

// ---------------- generic contract ----------------

async fn upsert_and_search_returns_nearest_first<B: VectorBackend>(backend: B) {
    backend.upsert("a", MODEL, &[1.0, 0.0, 0.0]).await.unwrap();
    backend.upsert("b", MODEL, &[0.9, 0.1, 0.0]).await.unwrap();
    backend.upsert("c", MODEL, &[0.0, 1.0, 0.0]).await.unwrap();

    let hits = backend.search(MODEL, &[1.0, 0.0, 0.0], 3).await.unwrap();
    let ids: Vec<_> = hits.iter().map(|h| h.id.as_str()).collect();
    assert_eq!(ids, vec!["a", "b", "c"], "nearest-first order");
    for w in hits.windows(2) {
        assert!(w[0].score >= w[1].score, "scores non-increasing: {hits:?}");
    }
}

async fn upsert_batch_and_count<B: VectorBackend>(backend: B) {
    let items: Vec<(String, Vec<f32>)> = (0..10)
        .map(|i| (format!("b{i}"), vec![1.0, i as f32 * 0.01, 0.0]))
        .collect();
    backend.upsert_batch(&items, MODEL).await.unwrap();
    assert_eq!(backend.count().await.unwrap(), 10);

    // Re-upserting the same ids must not double-count.
    backend.upsert_batch(&items, MODEL).await.unwrap();
    assert_eq!(backend.count().await.unwrap(), 10);
}

async fn model_partitions_are_isolated<B: VectorBackend>(backend: B) {
    backend.upsert("x", "m1", &[1.0, 0.0]).await.unwrap();
    backend.upsert("y", "m2", &[1.0, 0.0]).await.unwrap();

    let m1 = backend.search("m1", &[1.0, 0.0], 10).await.unwrap();
    assert_eq!(m1.len(), 1);
    assert_eq!(m1[0].id, "x");

    let m2 = backend.search("m2", &[1.0, 0.0], 10).await.unwrap();
    assert_eq!(m2.len(), 1);
    assert_eq!(m2[0].id, "y");
}

async fn delete_removes_id<B: VectorBackend>(backend: B) {
    backend.upsert("alive", MODEL, &[1.0, 0.0]).await.unwrap();
    backend.upsert("dead", MODEL, &[0.9, 0.1]).await.unwrap();
    backend.delete("dead").await.unwrap();

    let hits = backend.search(MODEL, &[1.0, 0.0], 10).await.unwrap();
    let ids: Vec<_> = hits.iter().map(|h| h.id.as_str()).collect();
    assert_eq!(ids, vec!["alive"]);
    // Deleting a missing id is a no-op.
    backend.delete("never-existed").await.unwrap();
}

async fn delete_by_path_drops_chunk_family<B: VectorBackend>(backend: B) {
    // Indexer convention: `<path>:<line-span>`.
    backend
        .upsert("src/a.rs:10-20", MODEL, &[1.0, 0.0])
        .await
        .unwrap();
    backend
        .upsert("src/a.rs:30-40", MODEL, &[0.7, 0.7])
        .await
        .unwrap();
    backend
        .upsert("src/b.rs:5-8", MODEL, &[0.0, 1.0])
        .await
        .unwrap();

    let removed = backend.delete_by_path("src/a.rs").await.unwrap();
    assert_eq!(removed, 2);

    let hits = backend.search(MODEL, &[1.0, 0.0], 10).await.unwrap();
    let ids: Vec<_> = hits.iter().map(|h| h.id.as_str()).collect();
    assert_eq!(ids, vec!["src/b.rs:5-8"]);
}

async fn k_caps_result_count<B: VectorBackend>(backend: B) {
    for i in 0..5 {
        backend
            .upsert(&format!("v{i}"), MODEL, &[1.0, i as f32 * 0.01])
            .await
            .unwrap();
    }
    let hits = backend.search(MODEL, &[1.0, 0.0], 2).await.unwrap();
    assert_eq!(hits.len(), 2);
}

/// The six contract cases. Listed once here and dispatched by every
/// backend driver below — the macro generates one `#[tokio::test]` per
/// (backend × case) pair.
macro_rules! contract_cases {
    ($drive:ident, $run:ident) => {
        #[tokio::test]
        async fn $run() {
            let (_dir, b) = $drive().await;
            upsert_and_search_returns_nearest_first(b).await;
        }

        paste::paste! {
            #[tokio::test]
            async fn [<$run _batch>]() {
                let (_dir, b) = $drive().await;
                upsert_batch_and_count(b).await;
            }

            #[tokio::test]
            async fn [<$run _model_isolation>]() {
                let (_dir, b) = $drive().await;
                model_partitions_are_isolated(b).await;
            }

            #[tokio::test]
            async fn [<$run _delete>]() {
                let (_dir, b) = $drive().await;
                delete_removes_id(b).await;
            }

            #[tokio::test]
            async fn [<$run _delete_by_path>]() {
                let (_dir, b) = $drive().await;
                delete_by_path_drops_chunk_family(b).await;
            }

            #[tokio::test]
            async fn [<$run _k_cap>]() {
                let (_dir, b) = $drive().await;
                k_caps_result_count(b).await;
            }
        }
    };
}

// ---------------- SQLite driver ----------------

async fn open_sqlite() -> (TempDir, VectorStore) {
    let dir = tempfile::tempdir().unwrap();
    let vs = VectorStore::open(dir.path().join("vectors.sqlite"))
        .await
        .unwrap();
    (dir, vs)
}

contract_cases!(open_sqlite, sqlite_contract);

// ---------------- LanceDB driver (feature-gated) ----------------

#[cfg(feature = "lance")]
mod lance_driver {
    use super::*;
    use thoth_store::LanceVectorStore;

    async fn open_lance() -> (TempDir, LanceVectorStore) {
        let dir = tempfile::tempdir().unwrap();
        let vs = LanceVectorStore::open(dir.path().join("chunks.lance"))
            .await
            .unwrap();
        (dir, vs)
    }

    contract_cases!(open_lance, lance_contract);
}

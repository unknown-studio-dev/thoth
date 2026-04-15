//! End-to-end test: index a small project → run hybrid recall → sanity-check
//! that the chunks we expect come back.

use tempfile::tempdir;
use thoth_core::Query;
use thoth_parse::LanguageRegistry;
use thoth_retrieve::{Indexer, Retriever};
use thoth_store::StoreRoot;

const AUTH_RS: &str = r#"
use std::collections::HashMap;

/// Verifies a JWT token signed with RS256.
pub fn verify_token(jwt: &str) -> bool {
    // stub
    !jwt.is_empty()
}

/// Signs a token with RS256.
pub fn sign_token(claims: &str) -> String {
    format!("jwt::{claims}")
}
"#;

const USERS_RS: &str = r#"
pub struct User {
    pub id: u64,
    pub name: String,
}

impl User {
    pub fn new(id: u64, name: String) -> Self {
        Self { id, name }
    }
}
"#;

#[tokio::test]
async fn index_and_recall_returns_relevant_chunks() {
    // Arrange: a tiny source tree.
    let src_dir = tempdir().unwrap();
    tokio::fs::write(src_dir.path().join("auth.rs"), AUTH_RS)
        .await
        .unwrap();
    tokio::fs::write(src_dir.path().join("users.rs"), USERS_RS)
        .await
        .unwrap();

    // Open a `.thoth/` alongside.
    let thoth_dir = tempdir().unwrap();
    let store = StoreRoot::open(thoth_dir.path()).await.unwrap();

    // Index.
    let idx = Indexer::new(store.clone(), LanguageRegistry::new());
    let stats = idx.index_path(src_dir.path()).await.unwrap();
    assert!(stats.files >= 2, "indexed files: {stats:?}");
    assert!(stats.chunks >= 4, "indexed chunks: {stats:?}");
    assert!(stats.symbols >= 3, "indexed symbols: {stats:?}");

    // Recall.
    let r = Retriever::new(store);
    let q = Query::text("verify jwt token");
    let out = r.recall(&q).await.unwrap();

    assert!(!out.chunks.is_empty(), "no chunks returned");
    // The top hit should be from auth.rs — either the function symbol or the
    // BM25 body match.
    let top = &out.chunks[0];
    assert!(
        top.path.ends_with("auth.rs"),
        "top chunk should be from auth.rs: {top:?}"
    );
    // And at least one chunk in the result should reference verify_token.
    assert!(
        out.chunks.iter().any(|c| c
            .symbol
            .as_deref()
            .map(|s| s.contains("verify_token"))
            .unwrap_or(false)
            || c.body.contains("verify_token")),
        "verify_token missing from {:?}",
        out.chunks
            .iter()
            .map(|c| c.symbol.clone().unwrap_or_default())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn recall_fuses_markdown_memory_hits() {
    let thoth_dir = tempdir().unwrap();
    let store = StoreRoot::open(thoth_dir.path()).await.unwrap();

    // Seed MEMORY.md directly.
    tokio::fs::write(
        thoth_dir.path().join("MEMORY.md"),
        "\n### auth uses JWT with RS256\nSigning keys live in Vault.\ntags: auth, security\n",
    )
    .await
    .unwrap();

    let r = Retriever::new(store);
    let out = r.recall(&Query::text("auth jwt vault")).await.unwrap();

    assert!(
        out.chunks.iter().any(|c| c.path.ends_with("MEMORY.md")),
        "expected a markdown-sourced chunk: {:?}",
        out.chunks
    );
}

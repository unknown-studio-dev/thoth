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
async fn recall_enriches_top_chunks_with_graph_context() {
    // A small source tree where `caller` calls `callee` in the same
    // file. After indexing, a recall for `caller` should come back with
    // `callee` surfaced under `context.callees` and a docstring lifted
    // from the `///` block above the function.
    let src = r#"
/// Do the heavy lifting.
///
/// This is the second paragraph.
pub fn do_heavy_lifting(x: i32) -> i32 {
    helper(x) + 1
}

/// Internal helper.
pub fn helper(x: i32) -> i32 {
    x * 2
}
"#;
    let src_dir = tempdir().unwrap();
    tokio::fs::write(src_dir.path().join("work.rs"), src)
        .await
        .unwrap();

    let thoth_dir = tempdir().unwrap();
    let store = StoreRoot::open(thoth_dir.path()).await.unwrap();
    let idx = Indexer::new(store.clone(), LanguageRegistry::new());
    idx.index_path(src_dir.path()).await.unwrap();

    let r = Retriever::new(store);
    let out = r.recall(&Query::text("do_heavy_lifting")).await.unwrap();

    // Find the chunk whose symbol is do_heavy_lifting.
    let heavy = out
        .chunks
        .iter()
        .find(|c| {
            c.symbol
                .as_deref()
                .is_some_and(|s| s.contains("do_heavy_lifting"))
        })
        .expect("do_heavy_lifting chunk present");

    let ctx = heavy
        .context
        .as_ref()
        .expect("graph context populated on top chunks");

    // Docstring lifted from the `///` block.
    let doc = ctx.doc.as_deref().unwrap_or("");
    assert!(
        doc.contains("Do the heavy lifting."),
        "doc should contain the first line: {doc:?}"
    );

    // `helper` should appear as a callee or a sibling (depending on
    // whether the parser produced a Calls edge — both are acceptable
    // signals).
    let referenced = ctx
        .callees
        .iter()
        .chain(ctx.siblings.iter())
        .any(|s| s.fqn.contains("helper"));
    assert!(
        referenced,
        "helper should be surfaced as callee or sibling; got callees={:?} siblings={:?}",
        ctx.callees, ctx.siblings
    );
}

#[tokio::test]
async fn retrieval_render_surfaces_enriched_sections() {
    // Whatever the graph produces, the rendered text should at least
    // include the chunk header and full body — and when a docstring is
    // present, its first line.
    let src = r#"
/// Handle login.
pub fn login() {}
"#;
    let src_dir = tempdir().unwrap();
    tokio::fs::write(src_dir.path().join("auth.rs"), src)
        .await
        .unwrap();

    let thoth_dir = tempdir().unwrap();
    let store = StoreRoot::open(thoth_dir.path()).await.unwrap();
    Indexer::new(store.clone(), LanguageRegistry::new())
        .index_path(src_dir.path())
        .await
        .unwrap();

    let r = Retriever::new(store);
    let out = r.recall(&Query::text("login")).await.unwrap();
    let rendered = out.render();

    assert!(rendered.contains("login"), "rendered: {rendered}");
    assert!(rendered.contains("auth.rs"), "rendered: {rendered}");
    // Docstring gutter marker from `Chunk::render_into`.
    assert!(
        rendered.contains("Handle login."),
        "doc line missing from: {rendered}"
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

//! End-to-end sync test: FileIngestor → redact → SnapshotStore.
//!
//! Exercises the same code path the CLI's `thoth domain sync` will call
//! with a real adapter.

use std::sync::Arc;

use thoth_domain::{
    IngestFilter, SnapshotStore, file::FileIngestor, snapshot::parse, sync_source,
};

#[tokio::test]
async fn full_sync_round_trip() {
    let input = tempfile::tempdir().unwrap();
    let store_root = tempfile::tempdir().unwrap();

    // Write two rules as TOML, one of which belongs to a context and
    // one of which will be dropped by `map_to_context` (empty context).
    tokio::fs::write(
        input.path().join("r1.toml"),
        r#"
id = "R-001"
source_uri = "file:///specs/R-001.md"
context = "billing"
kind = "invariant"
title = "Refund over 500 needs manager"
body = "Any refund above $500 requires manager approval."
updated_at = "2026-04-16T08:00:00Z"
tags = ["compliance"]
"#,
    )
    .await
    .unwrap();
    tokio::fs::write(
        input.path().join("r2.toml"),
        r#"
id = "R-002"
source_uri = "file:///specs/R-002.md"
context = ""
kind = "policy"
title = "Unscoped"
body = "Has no bounded context."
updated_at = "2026-04-16T08:00:00Z"
"#,
    )
    .await
    .unwrap();

    let ing = Arc::new(FileIngestor::new(input.path()));
    let snap = SnapshotStore::new(store_root.path());

    let rep = sync_source(ing.clone(), &snap, &IngestFilter::default())
        .await
        .unwrap();

    assert_eq!(rep.stats.created, 1, "one rule ingested");
    assert_eq!(rep.stats.unmapped, 1, "one rule without context dropped");
    assert_eq!(rep.stats.unchanged, 0);

    // Snapshot lives at the expected path.
    let expected = store_root
        .path()
        .join("domain/billing/_remote/file/R-001.md");
    assert!(expected.exists(), "snapshot path: {}", expected.display());

    let raw = tokio::fs::read_to_string(&expected).await.unwrap();
    let (fm, body) = parse(&raw).unwrap();
    assert_eq!(fm.id, "R-001");
    assert_eq!(fm.context, "billing");
    assert_eq!(fm.source, "file");
    assert!(fm.source_hash.starts_with("blake3:"));
    assert!(body.contains("Refund over 500 needs manager"));

    // Second run with no changes — everything should be unchanged.
    let rep = sync_source(ing, &snap, &IngestFilter::default())
        .await
        .unwrap();
    assert_eq!(rep.stats.unchanged, 1);
    assert_eq!(rep.stats.created, 0);
    assert_eq!(rep.stats.updated, 0);
}

#[tokio::test]
async fn redaction_blocks_provider_token() {
    let input = tempfile::tempdir().unwrap();
    let store_root = tempfile::tempdir().unwrap();

    tokio::fs::write(
        input.path().join("bad.toml"),
        r#"
id = "BAD-1"
source_uri = "file:///leaked"
context = "billing"
kind = "invariant"
title = "Leak"
body = "internal key: sk-abcdefghijklmnopqrstuv"
updated_at = "2026-04-16T08:00:00Z"
"#,
    )
    .await
    .unwrap();

    let ing = Arc::new(FileIngestor::new(input.path()));
    let snap = SnapshotStore::new(store_root.path());
    let rep = sync_source(ing, &snap, &IngestFilter::default())
        .await
        .unwrap();

    assert_eq!(rep.stats.redacted, 1);
    assert_eq!(rep.stats.created, 0);
    assert_eq!(rep.errors.len(), 1);

    // Make sure nothing was written under domain/.
    let dir = store_root.path().join("domain");
    if dir.exists() {
        let mut rd = tokio::fs::read_dir(&dir).await.unwrap();
        assert!(
            rd.next_entry().await.unwrap().is_none(),
            "no snapshots should exist after redaction"
        );
    }
}

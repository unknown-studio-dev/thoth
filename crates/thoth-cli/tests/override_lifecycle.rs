//! Integration test — full OverrideManager lifecycle end-to-end.
//!
//! Covers TEST-SPEC `override_full_lifecycle` (REQ-15, REQ-16, REQ-17, REQ-18):
//! request → list shows pending → approve → consume once (succeeds, TTL
//! drops to 0) → consume again (fails, token already consumed).

use tempfile::TempDir;
use thoth_memory::r#override::{OverrideManager, OverrideStatus};

#[test]
fn full_lifecycle_request_approve_consume_then_blocked() {
    let tmp = TempDir::new().expect("tempdir");
    let mgr = OverrideManager::new(tmp.path());

    // Step 1: agent files a request after a Block rule fires.
    let req = mgr
        .request(
            "no-rm-rf",
            "legit test cleanup",
            "hash-abc",
            "sess-1",
            1_700_000_000,
        )
        .expect("request");
    assert_eq!(req.status, OverrideStatus::Pending);

    // Step 2: `thoth override list` equivalent → one pending entry.
    let pending = mgr.list_pending().expect("list");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, req.id);

    // Step 3: user approves → moves to overrides/ with TTL.
    let approved = mgr.approve(&req.id, 1_700_000_100, 1).expect("approve");
    assert!(matches!(approved.status, OverrideStatus::Approved { .. }));
    assert!(mgr.list_pending().unwrap().is_empty());
    assert_eq!(mgr.list_approved().unwrap().len(), 1);

    // Step 4: agent re-runs same tool call → gate consumes the token.
    let consumed_ok = mgr
        .consume_if_match("no-rm-rf", "hash-abc", 1_700_000_200)
        .expect("consume");
    assert!(consumed_ok, "first consume should succeed");
    let after = mgr.get(&req.id).expect("get");
    assert!(matches!(after.status, OverrideStatus::Consumed { .. }));

    // Step 5: agent tries same command again → no token left, must block.
    let replay = mgr
        .consume_if_match("no-rm-rf", "hash-abc", 1_700_000_300)
        .expect("consume replay");
    assert!(!replay, "token already consumed — second consume must fail");
}

#[test]
fn ttl_greater_than_one_allows_multiple_consumes_then_blocks() {
    // Edge of the TTL ladder: approving with ttl=2 permits 2 consumes,
    // the third is blocked.
    let tmp = TempDir::new().expect("tempdir");
    let mgr = OverrideManager::new(tmp.path());

    let req = mgr
        .request("r1", "reason", "hx", "s1", 100)
        .expect("request");
    mgr.approve(&req.id, 200, 2).expect("approve");

    // First consume: TTL decrements 2 → 1.
    assert!(mgr.consume_if_match("r1", "hx", 210).expect("c1"));
    let after = mgr.get(&req.id).unwrap();
    match after.status {
        OverrideStatus::Approved { ttl_turns, .. } => assert_eq!(ttl_turns, 1),
        other => panic!("expected Approved(ttl=1), got {other:?}"),
    }

    // Second consume: TTL decrements 1 → 0 → Consumed.
    assert!(mgr.consume_if_match("r1", "hx", 220).expect("c2"));
    let after = mgr.get(&req.id).unwrap();
    assert!(matches!(after.status, OverrideStatus::Consumed { .. }));

    // Third consume: blocked.
    assert!(!mgr.consume_if_match("r1", "hx", 230).expect("c3"));
}

#[test]
fn reject_flow_never_becomes_consumable() {
    let tmp = TempDir::new().expect("tempdir");
    let mgr = OverrideManager::new(tmp.path());

    let req = mgr
        .request("r1", "reason", "hx", "s1", 100)
        .expect("request");
    mgr.reject(&req.id, 200, Some("unsafe".into()))
        .expect("reject");

    // Rejected entries live in override-rejected/ and are invisible to
    // find_approved / consume_if_match.
    assert!(mgr.find_approved("r1", "hx").unwrap().is_none());
    assert!(!mgr.consume_if_match("r1", "hx", 300).unwrap());
    assert_eq!(mgr.list_rejected().unwrap().len(), 1);
}

#[test]
fn consume_ignores_non_matching_hash_or_rule() {
    let tmp = TempDir::new().expect("tempdir");
    let mgr = OverrideManager::new(tmp.path());

    let req = mgr.request("r1", "x", "hash-A", "s1", 100).unwrap();
    mgr.approve(&req.id, 200, 1).unwrap();

    // Wrong hash → no consume.
    assert!(!mgr.consume_if_match("r1", "hash-B", 250).unwrap());
    // Wrong rule → no consume.
    assert!(!mgr.consume_if_match("r2", "hash-A", 250).unwrap());

    // Correct pair still works and consumes.
    assert!(mgr.consume_if_match("r1", "hash-A", 260).unwrap());
    // Now token is spent.
    assert!(!mgr.consume_if_match("r1", "hash-A", 270).unwrap());
}

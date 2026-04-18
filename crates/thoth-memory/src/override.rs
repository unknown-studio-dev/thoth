//! Override request flow types — violations recorded by the gate and
//! user-mediated override requests for rule-enforced lessons.
//!
//! See `DESIGN-SPEC.md` §"Violation / OverrideRequest" in the
//! `feat/thoth-enforcement-layer` session for the authoritative schema.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A recorded violation of a rule or rule-enforced lesson.
///
/// Violations are appended by the gate when a tool call is blocked
/// (or would have been blocked in warn-only mode) and consumed by the
/// outcome-harvest loop to drive promote/demote decisions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Violation {
    /// Stable UUID for this violation record.
    pub id: String,
    /// Source lesson, if the violation was triggered by a lesson-derived rule.
    pub lesson_id: Option<String>,
    /// Source rule, if the violation was triggered by an explicit rule.
    pub rule_id: Option<String>,
    /// Hash of the offending tool call (tool name + canonical args).
    pub tool_call_hash: String,
    /// Tool name (e.g. `"Bash"`, `"Edit"`).
    pub tool: String,
    /// Unix timestamp (seconds) when the violation was detected.
    pub detected_at: i64,
    /// Session ID that produced the violation.
    pub session_id: String,
}

/// A request from an agent to override a rule that just blocked it.
///
/// The request is persisted under `.thoth/override-requests/<id>.json`
/// and awaits user approval / rejection. Approval carries a TTL measured
/// in turns; consumption happens when the agent re-runs the blocked
/// tool call and the gate sees an approved, unexpired override.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OverrideRequest {
    /// Stable UUID for this request.
    pub id: String,
    /// Rule being overridden.
    pub rule_id: String,
    /// Agent-provided justification.
    pub reason: String,
    /// Hash of the tool call that prompted the request.
    pub tool_call_hash: String,
    /// Unix timestamp (seconds) when the request was filed.
    pub requested_at: i64,
    /// Session ID that filed the request.
    pub session_id: String,
    /// Current lifecycle status.
    pub status: OverrideStatus,
}

/// Lifecycle of an [`OverrideRequest`].
///
/// Transitions: `Pending` → `Approved` → `Consumed`, or `Pending` → `Rejected`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OverrideStatus {
    /// Awaiting user decision.
    Pending,
    /// User approved; valid for `ttl_turns` tool-call turns.
    Approved {
        /// Unix timestamp (seconds) when approval was granted.
        approved_at: i64,
        /// Remaining turns the approval is valid for.
        ttl_turns: u32,
    },
    /// User rejected the override; optional reason recorded for audit.
    Rejected {
        /// Unix timestamp (seconds) when the rejection was recorded.
        rejected_at: i64,
        /// Optional user-provided rejection reason.
        reason: Option<String>,
    },
    /// Approval was consumed by a matching tool call.
    Consumed {
        /// Unix timestamp (seconds) when consumption occurred.
        consumed_at: i64,
    },
}

/// Filesystem-backed manager for [`OverrideRequest`] lifecycle.
///
/// Layout rooted at a thoth directory (typically `.thoth/`):
///
/// - `override-requests/<id>.json` — Pending requests.
/// - `overrides/<id>.json` — Approved requests (possibly Consumed).
/// - `override-rejected/<id>.json` — Rejected requests.
///
/// All writes are atomic (write-then-rename) and moves between buckets use
/// `fs::rename` so the gate never observes a half-written file.
#[derive(Debug, Clone)]
pub struct OverrideManager {
    root: PathBuf,
}

/// Subdirectory for pending requests.
pub const PENDING_DIR: &str = "override-requests";
/// Subdirectory for approved (and consumed) overrides.
pub const APPROVED_DIR: &str = "overrides";
/// Subdirectory for rejected overrides.
pub const REJECTED_DIR: &str = "override-rejected";

/// Errors produced by [`OverrideManager`].
#[derive(Debug, thiserror::Error)]
pub enum OverrideError {
    /// Request with the given id was not found in the expected bucket.
    #[error("override request `{0}` not found")]
    NotFound(String),
    /// Request exists but is in a status that does not permit the operation.
    #[error("override request `{id}` in invalid state: {reason}")]
    InvalidState {
        /// Request id.
        id: String,
        /// Human-readable state mismatch reason.
        reason: String,
    },
    /// Underlying filesystem I/O error.
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    /// JSON (de)serialization error.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

impl OverrideManager {
    /// Create a manager rooted at `root` (typically the `.thoth/` directory).
    /// The target directories are created lazily on first write.
    pub fn new<P: AsRef<Path>>(root: P) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    fn pending_dir(&self) -> PathBuf {
        self.root.join(PENDING_DIR)
    }
    fn approved_dir(&self) -> PathBuf {
        self.root.join(APPROVED_DIR)
    }
    fn rejected_dir(&self) -> PathBuf {
        self.root.join(REJECTED_DIR)
    }

    fn ensure_dirs(&self) -> io::Result<()> {
        fs::create_dir_all(self.pending_dir())?;
        fs::create_dir_all(self.approved_dir())?;
        fs::create_dir_all(self.rejected_dir())?;
        Ok(())
    }

    /// File a new override request. Returns the persisted record (status
    /// `Pending`). Request id is a fresh v4 UUID.
    pub fn request(
        &self,
        rule_id: impl Into<String>,
        reason: impl Into<String>,
        tool_call_hash: impl Into<String>,
        session_id: impl Into<String>,
        requested_at: i64,
    ) -> Result<OverrideRequest, OverrideError> {
        self.ensure_dirs()?;
        let req = OverrideRequest {
            id: Uuid::new_v4().to_string(),
            rule_id: rule_id.into(),
            reason: reason.into(),
            tool_call_hash: tool_call_hash.into(),
            requested_at,
            session_id: session_id.into(),
            status: OverrideStatus::Pending,
        };
        let path = self.pending_dir().join(format!("{}.json", req.id));
        atomic_write_json(&path, &req)?;
        Ok(req)
    }

    /// Read a request by id, searching pending → approved → rejected.
    pub fn get(&self, id: &str) -> Result<OverrideRequest, OverrideError> {
        for dir in [self.pending_dir(), self.approved_dir(), self.rejected_dir()] {
            let path = dir.join(format!("{id}.json"));
            if path.exists() {
                return read_json(&path);
            }
        }
        Err(OverrideError::NotFound(id.into()))
    }

    /// List pending requests (those sitting in `override-requests/`).
    pub fn list_pending(&self) -> Result<Vec<OverrideRequest>, OverrideError> {
        read_dir_json(&self.pending_dir())
    }

    /// List approved (or consumed) overrides.
    pub fn list_approved(&self) -> Result<Vec<OverrideRequest>, OverrideError> {
        read_dir_json(&self.approved_dir())
    }

    /// List rejected overrides.
    pub fn list_rejected(&self) -> Result<Vec<OverrideRequest>, OverrideError> {
        read_dir_json(&self.rejected_dir())
    }

    /// Approve a pending request, moving it from `override-requests/` to
    /// `overrides/` and setting status to [`OverrideStatus::Approved`].
    pub fn approve(
        &self,
        id: &str,
        approved_at: i64,
        ttl_turns: u32,
    ) -> Result<OverrideRequest, OverrideError> {
        self.ensure_dirs()?;
        let src = self.pending_dir().join(format!("{id}.json"));
        if !src.exists() {
            return Err(OverrideError::NotFound(id.into()));
        }
        let mut req: OverrideRequest = read_json(&src)?;
        if !matches!(req.status, OverrideStatus::Pending) {
            return Err(OverrideError::InvalidState {
                id: id.into(),
                reason: format!("expected Pending, found {:?}", req.status),
            });
        }
        req.status = OverrideStatus::Approved {
            approved_at,
            ttl_turns,
        };
        let dst = self.approved_dir().join(format!("{id}.json"));
        atomic_write_json(&dst, &req)?;
        fs::remove_file(&src)?;
        Ok(req)
    }

    /// Reject a pending request, moving it to `override-rejected/` and
    /// setting status to [`OverrideStatus::Rejected`].
    pub fn reject(
        &self,
        id: &str,
        rejected_at: i64,
        reason: Option<String>,
    ) -> Result<OverrideRequest, OverrideError> {
        self.ensure_dirs()?;
        let src = self.pending_dir().join(format!("{id}.json"));
        if !src.exists() {
            return Err(OverrideError::NotFound(id.into()));
        }
        let mut req: OverrideRequest = read_json(&src)?;
        if !matches!(req.status, OverrideStatus::Pending) {
            return Err(OverrideError::InvalidState {
                id: id.into(),
                reason: format!("expected Pending, found {:?}", req.status),
            });
        }
        req.status = OverrideStatus::Rejected {
            rejected_at,
            reason,
        };
        let dst = self.rejected_dir().join(format!("{id}.json"));
        atomic_write_json(&dst, &req)?;
        fs::remove_file(&src)?;
        Ok(req)
    }

    /// Find an approved override for `(rule_id, tool_call_hash)` that still
    /// has remaining TTL, if any. Does not consume it.
    pub fn find_approved(
        &self,
        rule_id: &str,
        tool_call_hash: &str,
    ) -> Result<Option<OverrideRequest>, OverrideError> {
        for req in self.list_approved()? {
            if req.rule_id != rule_id || req.tool_call_hash != tool_call_hash {
                continue;
            }
            if let OverrideStatus::Approved { ttl_turns, .. } = req.status
                && ttl_turns > 0
            {
                return Ok(Some(req));
            }
        }
        Ok(None)
    }

    /// Attempt to consume an approved override matching `(rule_id, hash)`.
    ///
    /// - On the first call after approval, decrements `ttl_turns`. If it
    ///   drops to zero the request is transitioned to
    ///   [`OverrideStatus::Consumed`]. Returns `true`.
    /// - On subsequent calls once consumed (or when no approval exists),
    ///   returns `false`.
    pub fn consume_if_match(
        &self,
        rule_id: &str,
        tool_call_hash: &str,
        consumed_at: i64,
    ) -> Result<bool, OverrideError> {
        let mut found_id: Option<String> = None;
        let mut found_req: Option<OverrideRequest> = None;
        for req in self.list_approved()? {
            if req.rule_id != rule_id || req.tool_call_hash != tool_call_hash {
                continue;
            }
            if let OverrideStatus::Approved { ttl_turns, .. } = req.status
                && ttl_turns > 0
            {
                found_id = Some(req.id.clone());
                found_req = Some(req);
                break;
            }
        }
        let (id, mut req) = match (found_id, found_req) {
            (Some(id), Some(req)) => (id, req),
            _ => return Ok(false),
        };
        let new_status = match req.status {
            OverrideStatus::Approved {
                approved_at,
                ttl_turns,
            } => {
                let remaining = ttl_turns.saturating_sub(1);
                if remaining == 0 {
                    OverrideStatus::Consumed { consumed_at }
                } else {
                    OverrideStatus::Approved {
                        approved_at,
                        ttl_turns: remaining,
                    }
                }
            }
            other => other,
        };
        req.status = new_status;
        let path = self.approved_dir().join(format!("{id}.json"));
        atomic_write_json(&path, &req)?;
        Ok(true)
    }
}

fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), OverrideError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(value)?;
    fs::write(&tmp, body)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, OverrideError> {
    let body = fs::read(path)?;
    Ok(serde_json::from_slice(&body)?)
}

fn read_dir_json<T: for<'de> Deserialize<'de>>(dir: &Path) -> Result<Vec<T>, OverrideError> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        out.push(read_json(&path)?);
    }
    Ok(out)
}

#[cfg(test)]
mod types {
    use super::*;

    fn sample_violation() -> Violation {
        Violation {
            id: "v-1".into(),
            lesson_id: Some("lesson-abc".into()),
            rule_id: None,
            tool_call_hash: "hash-xyz".into(),
            tool: "Bash".into(),
            detected_at: 1_700_000_000,
            session_id: "sess-1".into(),
        }
    }

    fn sample_request(status: OverrideStatus) -> OverrideRequest {
        OverrideRequest {
            id: "req-1".into(),
            rule_id: "rule-1".into(),
            reason: "legitimate edge case".into(),
            tool_call_hash: "hash-xyz".into(),
            requested_at: 1_700_000_100,
            session_id: "sess-1".into(),
            status,
        }
    }

    #[test]
    fn violation_roundtrips_through_json() {
        let v = sample_violation();
        let s = serde_json::to_string(&v).expect("serialize");
        let back: Violation = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(v, back);
    }

    #[test]
    fn override_request_roundtrips_pending() {
        let r = sample_request(OverrideStatus::Pending);
        let s = serde_json::to_string(&r).expect("serialize");
        let back: OverrideRequest = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(r, back);
    }

    #[test]
    fn override_status_approved_snake_case_tag() {
        let r = sample_request(OverrideStatus::Approved {
            approved_at: 1_700_000_200,
            ttl_turns: 3,
        });
        let s = serde_json::to_string(&r).expect("serialize");
        assert!(
            s.contains("\"approved\""),
            "expected snake_case variant tag, got: {s}"
        );
        assert!(s.contains("\"approved_at\":1700000200"));
        assert!(s.contains("\"ttl_turns\":3"));
        let back: OverrideRequest = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(r, back);
    }

    #[test]
    fn override_status_rejected_with_reason() {
        let r = sample_request(OverrideStatus::Rejected {
            rejected_at: 1_700_000_300,
            reason: Some("not safe".into()),
        });
        let s = serde_json::to_string(&r).expect("serialize");
        let back: OverrideRequest = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(r, back);
        assert!(s.contains("\"rejected\""));
    }

    #[test]
    fn override_status_rejected_without_reason() {
        let r = sample_request(OverrideStatus::Rejected {
            rejected_at: 1_700_000_300,
            reason: None,
        });
        let s = serde_json::to_string(&r).expect("serialize");
        let back: OverrideRequest = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(r, back);
    }

    #[test]
    fn override_status_consumed_roundtrip() {
        let r = sample_request(OverrideStatus::Consumed {
            consumed_at: 1_700_000_400,
        });
        let s = serde_json::to_string(&r).expect("serialize");
        let back: OverrideRequest = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(r, back);
        assert!(s.contains("\"consumed\""));
    }
}

#[cfg(test)]
mod lifecycle {
    use super::*;
    use tempfile::TempDir;

    fn mgr() -> (TempDir, OverrideManager) {
        let dir = TempDir::new().expect("tempdir");
        let m = OverrideManager::new(dir.path());
        (dir, m)
    }

    #[test]
    fn request_writes_file() {
        let (dir, m) = mgr();
        let req = m
            .request("no-rm-rf", "legit cleanup", "hash-1", "sess-1", 100)
            .expect("request");
        assert_eq!(req.status, OverrideStatus::Pending);
        let path = dir
            .path()
            .join(PENDING_DIR)
            .join(format!("{}.json", req.id));
        assert!(path.exists(), "pending file must exist at {path:?}");
        let fetched = m.get(&req.id).expect("get");
        assert_eq!(fetched, req);
    }

    #[test]
    fn approve_moves_to_approved_dir() {
        let (dir, m) = mgr();
        let req = m.request("r1", "reason", "h1", "s1", 100).expect("request");
        let approved = m.approve(&req.id, 200, 1).expect("approve");
        assert!(matches!(
            approved.status,
            OverrideStatus::Approved {
                ttl_turns: 1,
                approved_at: 200
            }
        ));
        assert!(
            !dir.path()
                .join(PENDING_DIR)
                .join(format!("{}.json", req.id))
                .exists()
        );
        assert!(
            dir.path()
                .join(APPROVED_DIR)
                .join(format!("{}.json", req.id))
                .exists()
        );
    }

    #[test]
    fn reject_moves_to_rejected_dir_with_reason() {
        let (dir, m) = mgr();
        let req = m.request("r1", "reason", "h1", "s1", 100).expect("request");
        let rejected = m
            .reject(&req.id, 300, Some("unsafe".into()))
            .expect("reject");
        match rejected.status {
            OverrideStatus::Rejected {
                rejected_at,
                reason,
            } => {
                assert_eq!(rejected_at, 300);
                assert_eq!(reason.as_deref(), Some("unsafe"));
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
        assert!(
            dir.path()
                .join(REJECTED_DIR)
                .join(format!("{}.json", req.id))
                .exists()
        );
    }

    #[test]
    fn single_use() {
        // Approved override with ttl=1 → first consume_if_match returns
        // true + marks Consumed, second returns false.
        let (_dir, m) = mgr();
        let req = m.request("r1", "reason", "hx", "s1", 100).expect("request");
        m.approve(&req.id, 200, 1).expect("approve");
        assert!(m.find_approved("r1", "hx").expect("find").is_some());

        let first = m.consume_if_match("r1", "hx", 250).expect("consume1");
        assert!(first, "first consume should succeed");
        let after = m.get(&req.id).expect("get");
        assert!(matches!(after.status, OverrideStatus::Consumed { .. }));

        let second = m.consume_if_match("r1", "hx", 260).expect("consume2");
        assert!(!second, "second consume should fail (token used)");
    }

    #[test]
    fn consume_decrements_ttl_when_greater_than_one() {
        let (_dir, m) = mgr();
        let req = m.request("r1", "reason", "hx", "s1", 100).expect("request");
        m.approve(&req.id, 200, 2).expect("approve");

        assert!(m.consume_if_match("r1", "hx", 250).expect("consume1"));
        let after = m.get(&req.id).expect("get");
        match after.status {
            OverrideStatus::Approved { ttl_turns, .. } => assert_eq!(ttl_turns, 1),
            other => panic!("expected Approved(ttl=1), got {other:?}"),
        }

        assert!(m.consume_if_match("r1", "hx", 260).expect("consume2"));
        let after = m.get(&req.id).expect("get");
        assert!(matches!(after.status, OverrideStatus::Consumed { .. }));

        assert!(!m.consume_if_match("r1", "hx", 270).expect("consume3"));
    }

    #[test]
    fn approve_unknown_returns_not_found() {
        let (_dir, m) = mgr();
        let err = m.approve("does-not-exist", 1, 1).unwrap_err();
        assert!(matches!(err, OverrideError::NotFound(_)));
    }

    #[test]
    fn reject_already_approved_fails() {
        let (_dir, m) = mgr();
        let req = m.request("r1", "reason", "hx", "s1", 100).expect("request");
        m.approve(&req.id, 200, 1).expect("approve");
        // File moved out of pending, so reject sees NotFound.
        let err = m.reject(&req.id, 300, None).unwrap_err();
        assert!(matches!(err, OverrideError::NotFound(_)));
    }

    #[test]
    fn list_pending_only_returns_pending() {
        let (_dir, m) = mgr();
        let a = m.request("r1", "a", "h1", "s1", 100).unwrap();
        let b = m.request("r2", "b", "h2", "s1", 101).unwrap();
        let c = m.request("r3", "c", "h3", "s1", 102).unwrap();
        m.approve(&b.id, 200, 1).unwrap();
        m.reject(&c.id, 201, None).unwrap();

        let pending = m.list_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, a.id);
        assert_eq!(m.list_approved().unwrap().len(), 1);
        assert_eq!(m.list_rejected().unwrap().len(), 1);
    }

    #[test]
    fn find_approved_requires_matching_rule_and_hash() {
        let (_dir, m) = mgr();
        let req = m.request("r1", "x", "hash-A", "s1", 100).unwrap();
        m.approve(&req.id, 200, 1).unwrap();

        assert!(m.find_approved("r1", "hash-A").unwrap().is_some());
        assert!(m.find_approved("r1", "hash-B").unwrap().is_none());
        assert!(m.find_approved("r2", "hash-A").unwrap().is_none());
    }

    #[test]
    fn concurrent_requests_same_rule_unique_ids_independent_approval() {
        // Edge case per TEST-SPEC: two pending requests for the same rule
        // must coexist under distinct UUIDs and be independently approvable.
        let (_dir, m) = mgr();
        let a = m
            .request("no-rm-rf", "first agent", "hash-A", "s-1", 100)
            .unwrap();
        let b = m
            .request("no-rm-rf", "second agent", "hash-B", "s-2", 101)
            .unwrap();
        assert_ne!(a.id, b.id, "UUIDs must be distinct");
        let pending = m.list_pending().unwrap();
        assert_eq!(pending.len(), 2);

        // Approve only `a`. `b` must remain pending.
        m.approve(&a.id, 200, 1).unwrap();
        let still_pending = m.list_pending().unwrap();
        assert_eq!(still_pending.len(), 1);
        assert_eq!(still_pending[0].id, b.id);
        assert_eq!(m.list_approved().unwrap().len(), 1);

        // `b` is still approvable independently.
        m.approve(&b.id, 210, 2).unwrap();
        assert!(m.list_pending().unwrap().is_empty());
        assert_eq!(m.list_approved().unwrap().len(), 2);
    }

    #[test]
    fn ttl_zero_at_approval_cannot_be_consumed() {
        // Edge case: approving with ttl_turns=0 yields a token that
        // find_approved treats as spent — consume must return false.
        let (_dir, m) = mgr();
        let req = m.request("r1", "x", "h1", "s1", 100).unwrap();
        m.approve(&req.id, 200, 0).expect("approve ttl=0");
        assert!(m.find_approved("r1", "h1").unwrap().is_none());
        assert!(!m.consume_if_match("r1", "h1", 250).unwrap());
    }

    #[test]
    fn ttl_larger_walks_down_one_per_consume() {
        // Verify the TTL ladder is strictly decrementing (not jumping to
        // Consumed early).
        let (_dir, m) = mgr();
        let req = m.request("r1", "x", "h1", "s1", 100).unwrap();
        m.approve(&req.id, 200, 3).unwrap();
        for expected_remaining in [2u32, 1u32] {
            assert!(m.consume_if_match("r1", "h1", 201).unwrap());
            let after = m.get(&req.id).unwrap();
            match after.status {
                OverrideStatus::Approved { ttl_turns, .. } => {
                    assert_eq!(ttl_turns, expected_remaining);
                }
                other => panic!("expected Approved, got {other:?}"),
            }
        }
        // Final consume flips to Consumed.
        assert!(m.consume_if_match("r1", "h1", 300).unwrap());
        let after = m.get(&req.id).unwrap();
        assert!(matches!(after.status, OverrideStatus::Consumed { .. }));
        // Any further consume is a no-op.
        assert!(!m.consume_if_match("r1", "h1", 301).unwrap());
    }

    #[test]
    fn approve_twice_second_errors() {
        // TEST-SPEC edge case: "Override approve 2 lần cùng ID" → second
        // call errors because the pending file was moved by the first.
        let (_dir, m) = mgr();
        let req = m.request("r1", "x", "h1", "s1", 100).unwrap();
        m.approve(&req.id, 200, 1).unwrap();
        let err = m.approve(&req.id, 201, 1).unwrap_err();
        assert!(
            matches!(err, OverrideError::NotFound(_)),
            "expected NotFound, got {err:?}"
        );
    }

    #[test]
    fn reject_unknown_returns_not_found() {
        let (_dir, m) = mgr();
        let err = m.reject("ghost", 100, None).unwrap_err();
        assert!(matches!(err, OverrideError::NotFound(_)));
    }

    #[test]
    fn get_searches_all_buckets() {
        // After a lifecycle transition the request must still be findable.
        let (_dir, m) = mgr();
        let req = m.request("r1", "x", "h1", "s1", 100).unwrap();
        m.approve(&req.id, 200, 1).unwrap();
        let found = m.get(&req.id).unwrap();
        assert!(matches!(found.status, OverrideStatus::Approved { .. }));

        let rej = m.request("r2", "x", "h2", "s1", 110).unwrap();
        m.reject(&rej.id, 210, Some("nope".into())).unwrap();
        let found = m.get(&rej.id).unwrap();
        assert!(matches!(found.status, OverrideStatus::Rejected { .. }));
    }

    #[test]
    fn consume_ignores_rejected_entries() {
        // Rejected requests live in a different directory — consume
        // operating on approved/ should never see them.
        let (_dir, m) = mgr();
        let req = m.request("r1", "x", "h1", "s1", 100).unwrap();
        m.reject(&req.id, 200, None).unwrap();
        assert!(m.find_approved("r1", "h1").unwrap().is_none());
        assert!(!m.consume_if_match("r1", "h1", 300).unwrap());
    }

    #[test]
    fn partial_failure_malformed_json_ignored_in_listing() {
        // A malformed pending file must not take down list_pending wholesale;
        // callers must either skip it or surface a JSON error. Either is
        // acceptable — we only require the manager to not panic.
        let (dir, m) = mgr();
        let _ok = m.request("r1", "x", "h1", "s1", 100).unwrap();
        fs::create_dir_all(dir.path().join(PENDING_DIR)).unwrap();
        fs::write(dir.path().join(PENDING_DIR).join("junk.json"), "not json").unwrap();
        // Either errors cleanly (JSON error) or skips — never panics.
        let result = m.list_pending();
        match result {
            Ok(list) => {
                // If it silently skipped, at least our legit entry is kept.
                assert!(list.iter().any(|r| r.rule_id == "r1"));
            }
            Err(OverrideError::Json(_)) | Err(OverrideError::Io(_)) => {}
            Err(other) => panic!("unexpected error kind: {other:?}"),
        }
    }

    #[test]
    fn full_lifecycle_request_approve_consume() {
        let (_dir, m) = mgr();
        let req = m
            .request("no-rm-rf", "legit", "call-hash", "sess-xyz", 1_700_000_000)
            .expect("request");
        let pending = m.list_pending().expect("list");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].status, OverrideStatus::Pending);

        m.approve(&req.id, 1_700_000_100, 1).expect("approve");
        assert!(m.list_pending().unwrap().is_empty());
        assert_eq!(m.list_approved().unwrap().len(), 1);

        let ok = m
            .consume_if_match("no-rm-rf", "call-hash", 1_700_000_200)
            .expect("consume");
        assert!(ok);
        let final_state = m.get(&req.id).expect("get");
        assert!(matches!(
            final_state.status,
            OverrideStatus::Consumed { .. }
        ));

        // Token is single-use.
        let replay = m
            .consume_if_match("no-rm-rf", "call-hash", 1_700_000_300)
            .expect("consume-replay");
        assert!(!replay);
    }
}

//! Handlers for `thoth workflow {list,reset}` (REQ-19 / DESIGN-SPEC §CLI).
//!
//! - `list`  — print every `Active` workflow state under
//!   `<root>/workflow/*.json`.
//! - `reset` — mark an `Active` session as `Abandoned` and drop any
//!   accumulated rows in `workflow-violations.jsonl` for that session,
//!   so the gate's violation counter falls back below threshold.

use std::io;
use std::path::Path;

use thoth_memory::workflow::{WorkflowState, WorkflowStateManager};

/// Current wall-clock in unix epoch seconds. Broken out so tests can
/// shim it via direct manager calls.
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Outcome of a `reset` invocation. Public so tests in this module and
/// integration callers can assert on the shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetOutcome {
    /// Session id that was targeted.
    pub session_id: String,
    /// `true` if a state file existed and was flipped to `Abandoned`.
    /// `false` if no state file was found (reset is still allowed — we
    /// clear the violation counter regardless).
    pub abandoned: bool,
    /// Number of violation rows removed from
    /// `workflow-violations.jsonl` for this session.
    pub violations_cleared: u32,
}

/// `thoth workflow list` — print Active workflows, one per line.
pub async fn cmd_list(root: &Path, json: bool) -> anyhow::Result<()> {
    let mgr = WorkflowStateManager::new(root.to_path_buf());
    let mut active = mgr.list_active()?;
    active.sort_by(|a, b| a.session_id.cmp(&b.session_id));

    if json {
        let payload: Vec<_> = active
            .iter()
            .map(|s| {
                serde_json::json!({
                    "session_id":      s.session_id,
                    "workflow_name":   s.workflow_name,
                    "started_at":      s.started_at,
                    "completed_steps": s.completed_steps,
                    "expected_steps":  s.expected_steps,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    if active.is_empty() {
        println!("No active workflows.");
        return Ok(());
    }
    println!("Active workflows:");
    for s in &active {
        print_row(s);
    }
    Ok(())
}

fn print_row(s: &WorkflowState) {
    let steps = if s.expected_steps.is_empty() {
        format!("{} step(s)", s.completed_steps.len())
    } else {
        format!("{}/{}", s.completed_steps.len(), s.expected_steps.len())
    };
    println!(
        "  {}  workflow={}  started_at={}  steps={}",
        s.session_id, s.workflow_name, s.started_at, steps,
    );
}

/// `thoth workflow reset <session_id>` — flip an active session to
/// `Abandoned` and clear its rows from the violation log.
pub async fn cmd_reset(root: &Path, session_id: &str, json: bool) -> anyhow::Result<()> {
    let outcome = reset(root, session_id, now_secs())?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "session_id":         outcome.session_id,
                "abandoned":          outcome.abandoned,
                "violations_cleared": outcome.violations_cleared,
            }))?
        );
        return Ok(());
    }

    if outcome.abandoned {
        println!("Marked session {} as abandoned.", outcome.session_id);
    } else {
        println!(
            "No active workflow state for session {} (nothing to abandon).",
            outcome.session_id
        );
    }
    println!(
        "Cleared {} violation row(s) from workflow-violations.jsonl.",
        outcome.violations_cleared
    );
    Ok(())
}

/// Testable core of `reset` — no clock / printing.
pub fn reset(root: &Path, session_id: &str, at: i64) -> io::Result<ResetOutcome> {
    let mgr = WorkflowStateManager::new(root.to_path_buf());
    let abandoned = match mgr.abandon_workflow(session_id, at) {
        Ok(_) => true,
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => return Err(e),
    };
    let violations_cleared = mgr.clear_violations_for(session_id)?;
    Ok(ResetOutcome {
        session_id: session_id.to_string(),
        abandoned,
        violations_cleared,
    })
}

#[cfg(test)]
mod reset {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn flips_active_to_abandoned_and_clears_violations() {
        let td = tempdir().unwrap();
        let mgr = WorkflowStateManager::new(td.path().to_path_buf());
        mgr.start_workflow("s1", "hoangsa:cook", 1_000).unwrap();
        // Two violations for the target + one for a different session.
        let window = 7 * 24 * 60 * 60;
        mgr.increment_violation("s1", "hoangsa:cook", "stop_without_complete", 1_100, window)
            .unwrap();
        mgr.increment_violation("s1", "hoangsa:cook", "stop_without_complete", 1_200, window)
            .unwrap();
        mgr.increment_violation("s2", "hoangsa:cook", "stop_without_complete", 1_300, window)
            .unwrap();

        let outcome = super::reset(td.path(), "s1", 2_000).unwrap();
        assert_eq!(outcome.session_id, "s1");
        assert!(outcome.abandoned);
        assert_eq!(outcome.violations_cleared, 2);

        // State now Abandoned.
        let back = mgr.get("s1").unwrap().unwrap();
        assert_eq!(
            back.status,
            thoth_memory::workflow::WorkflowStatus::Abandoned
        );
        // list_active no longer sees s1 (only one with a state file).
        let active: Vec<String> = mgr
            .list_active()
            .unwrap()
            .into_iter()
            .map(|s| s.session_id)
            .collect();
        assert!(active.is_empty());
        // Untouched session still has its violation row.
        let raw = std::fs::read_to_string(mgr.violations_path()).unwrap();
        assert!(raw.contains("\"s2\""));
        assert!(!raw.contains("\"s1\""));
    }

    #[test]
    fn unknown_session_is_not_an_error() {
        let td = tempdir().unwrap();
        let outcome = super::reset(td.path(), "ghost", 42).unwrap();
        assert!(!outcome.abandoned);
        assert_eq!(outcome.violations_cleared, 0);
    }

    #[test]
    fn reset_is_idempotent() {
        let td = tempdir().unwrap();
        let mgr = WorkflowStateManager::new(td.path().to_path_buf());
        mgr.start_workflow("s", "wf", 1).unwrap();
        let window = 7 * 24 * 60 * 60;
        mgr.increment_violation("s", "wf", "r", 10, window).unwrap();

        let first = super::reset(td.path(), "s", 100).unwrap();
        assert!(first.abandoned);
        assert_eq!(first.violations_cleared, 1);

        // Second call: state is already Abandoned, nothing to clear.
        let second = super::reset(td.path(), "s", 200).unwrap();
        // abandon_workflow on a non-Active row still succeeds (it just
        // rewrites the same status), so we report abandoned=true again.
        assert!(second.abandoned);
        assert_eq!(second.violations_cleared, 0);
    }
}

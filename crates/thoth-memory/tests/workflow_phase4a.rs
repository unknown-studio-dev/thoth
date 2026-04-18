//! Phase 4a integration tests: Stop hook semantics — starting a workflow
//! and finishing the session without `thoth_workflow_complete` must
//! accumulate violations. Running `thoth workflow reset` (modeled here as
//! `clear_violations_for`) must wipe the counter.
//!
//! Covers REQ-19. See TEST-SPEC `workflow_4a_end_to_end`.

use tempfile::tempdir;
use thoth_memory::workflow::{WorkflowStateManager, WorkflowStatus};

const WINDOW_SECS: i64 = 7 * 24 * 60 * 60;

/// Simulate the Stop hook: if the session has an Active workflow without
/// `all_steps_completed`, log a `stop_without_complete` violation and
/// mark the workflow Abandoned. Returns the running violation count in
/// the window, or 0 if no active workflow existed.
fn simulate_stop_hook(mgr: &WorkflowStateManager, session_id: &str, now: i64) -> u32 {
    let Some(state) = mgr.get(session_id).unwrap() else {
        return 0;
    };
    if state.status != WorkflowStatus::Active {
        return 0;
    }
    let count = mgr
        .increment_violation(
            session_id,
            state.workflow_name.clone(),
            "stop_without_complete",
            now,
            WINDOW_SECS,
        )
        .unwrap();
    mgr.abandon_workflow(session_id, now).unwrap();
    count
}

#[test]
fn stop_after_start_without_complete_increments_violation_counter() {
    let td = tempdir().unwrap();
    let mgr = WorkflowStateManager::new(td.path().to_path_buf());

    mgr.start_workflow("sess-1", "hoangsa:cook", 1_700_000_000)
        .unwrap();
    let count = simulate_stop_hook(&mgr, "sess-1", 1_700_000_100);
    assert_eq!(count, 1, "one violation after one stop-without-complete");

    // State is flipped to Abandoned so a follow-up Stop is idempotent.
    let state = mgr.get("sess-1").unwrap().unwrap();
    assert_eq!(state.status, WorkflowStatus::Abandoned);
    let count2 = simulate_stop_hook(&mgr, "sess-1", 1_700_000_200);
    assert_eq!(count2, 0, "second stop on abandoned session is a no-op");
}

#[test]
fn three_stop_without_complete_cycles_accumulate_then_reset_clears() {
    let td = tempdir().unwrap();
    let mgr = WorkflowStateManager::new(td.path().to_path_buf());

    let session = "sess-ab";
    let mut t = 1_700_000_000;
    for expected in 1..=3u32 {
        mgr.start_workflow(session, "hoangsa:cook", t).unwrap();
        t += 10;
        let n = simulate_stop_hook(&mgr, session, t);
        assert_eq!(n, expected, "violation count after cycle {expected}");
        t += 10;
    }

    // Emulate `thoth workflow reset`.
    let removed = mgr.clear_violations_for(session).unwrap();
    assert_eq!(removed, 3);

    // Fresh start → a subsequent stop shows count=1.
    mgr.start_workflow(session, "hoangsa:cook", t).unwrap();
    t += 5;
    let count = simulate_stop_hook(&mgr, session, t);
    assert_eq!(count, 1, "reset fully cleared the violation window");
}

#[test]
fn stop_without_active_workflow_is_noop() {
    let td = tempdir().unwrap();
    let mgr = WorkflowStateManager::new(td.path().to_path_buf());

    // No `start_workflow` call — Stop fires anyway.
    let count = simulate_stop_hook(&mgr, "ghost", 42);
    assert_eq!(count, 0);
    assert!(!mgr.violations_path().exists());
}

#[test]
fn full_lifecycle_start_complete_records_no_violation() {
    let td = tempdir().unwrap();
    let mgr = WorkflowStateManager::new(td.path().to_path_buf());

    mgr.start_workflow("sess-ok", "hoangsa:cook", 1_000)
        .unwrap();
    mgr.complete_workflow("sess-ok", 2_000).unwrap();

    // Stop hook after a clean complete must NOT log a violation.
    let count = simulate_stop_hook(&mgr, "sess-ok", 3_000);
    assert_eq!(count, 0);

    // Violation log is absent / empty.
    let violations = mgr.violations_path();
    if violations.exists() {
        let raw = std::fs::read_to_string(violations).unwrap();
        assert!(
            raw.trim().is_empty(),
            "no violations after clean complete, got: {raw}"
        );
    }

    let state = mgr.get("sess-ok").unwrap().unwrap();
    assert_eq!(state.status, WorkflowStatus::Completed);
}

#[test]
fn violations_for_other_sessions_untouched_by_reset() {
    let td = tempdir().unwrap();
    let mgr = WorkflowStateManager::new(td.path().to_path_buf());

    mgr.start_workflow("a", "wf", 0).unwrap();
    simulate_stop_hook(&mgr, "a", 10);
    mgr.start_workflow("b", "wf", 20).unwrap();
    simulate_stop_hook(&mgr, "b", 30);

    let removed = mgr.clear_violations_for("a").unwrap();
    assert_eq!(removed, 1);

    // `b` violation survives.
    let raw = std::fs::read_to_string(mgr.violations_path()).unwrap();
    assert!(raw.contains("\"session_id\":\"b\""));
    assert!(!raw.contains("\"session_id\":\"a\""));
}

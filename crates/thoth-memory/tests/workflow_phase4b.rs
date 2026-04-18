//! Phase 4b integration tests: `expected_steps` + `advance_step` +
//! `detect_gap`. Covers REQ-20. See TEST-SPEC `workflow_4b_missing_step`.

use tempfile::tempdir;
use thoth_memory::workflow::{WorkflowStateManager, WorkflowStatus};

const WINDOW_SECS: i64 = 7 * 24 * 60 * 60;

#[test]
fn advance_out_of_order_detect_gap_reports_skipped_steps() {
    let td = tempdir().unwrap();
    let mgr = WorkflowStateManager::new(td.path().to_path_buf());

    mgr.start_workflow_with_steps(
        "sess-1",
        "hoangsa:cook",
        1_000,
        vec!["1a".into(), "1b".into(), "2".into()],
    )
    .unwrap();

    // Agent skips "1a" and "1b" entirely and jumps to "2".
    mgr.advance_step("sess-1", "2", 1_100).unwrap();

    let gap = mgr.detect_gap("sess-1").unwrap();
    assert_eq!(gap, vec!["1a".to_string(), "1b".to_string()]);
}

#[test]
fn partial_progress_then_complete_leaves_detectable_gap_and_logs_violation() {
    let td = tempdir().unwrap();
    let mgr = WorkflowStateManager::new(td.path().to_path_buf());

    let session = "sess-2";
    mgr.start_workflow_with_steps(
        session,
        "hoangsa:cook",
        0,
        vec!["1a".into(), "1b".into(), "2".into()],
    )
    .unwrap();

    mgr.advance_step(session, "1a", 10).unwrap();
    // Skip "1b".
    let gap_before_complete = mgr.detect_gap(session).unwrap();
    assert_eq!(gap_before_complete, vec!["1b".to_string(), "2".to_string()]);

    // Caller elects to complete anyway — gap is non-empty, so log violation.
    let reason = format!("skipped {}", gap_before_complete.join(", "));
    let count = mgr
        .increment_violation(session, "hoangsa:cook", &reason, 20, WINDOW_SECS)
        .unwrap();
    assert_eq!(count, 1);

    mgr.complete_workflow(session, 30).unwrap();

    // Completed state preserves the record of partial completion.
    let state = mgr.get(session).unwrap().unwrap();
    assert_eq!(state.status, WorkflowStatus::Completed);
    assert_eq!(state.completed_steps, vec!["1a".to_string()]);

    let raw = std::fs::read_to_string(mgr.violations_path()).unwrap();
    assert!(
        raw.contains("skipped 1b, 2"),
        "violation reason preserved: {raw}"
    );
}

#[test]
fn full_lifecycle_all_steps_completes_with_empty_gap_and_no_violations() {
    let td = tempdir().unwrap();
    let mgr = WorkflowStateManager::new(td.path().to_path_buf());

    let session = "sess-ok";
    mgr.start_workflow_with_steps(
        session,
        "hoangsa:cook",
        0,
        vec!["menu".into(), "prepare".into(), "cook".into()],
    )
    .unwrap();

    mgr.advance_step(session, "menu", 10).unwrap();
    mgr.advance_step(session, "prepare", 20).unwrap();
    mgr.advance_step(session, "cook", 30).unwrap();

    let gap = mgr.detect_gap(session).unwrap();
    assert!(gap.is_empty(), "all expected steps advanced: {gap:?}");

    let state = mgr.get(session).unwrap().unwrap();
    assert!(state.all_steps_completed());

    mgr.complete_workflow(session, 40).unwrap();
    let state = mgr.get(session).unwrap().unwrap();
    assert_eq!(state.status, WorkflowStatus::Completed);

    // No violation log created for a clean lifecycle.
    assert!(
        !mgr.violations_path().exists(),
        "no violations file should exist for clean lifecycle"
    );
}

#[test]
fn detect_gap_preserves_expected_step_order_even_with_interleaved_advances() {
    let td = tempdir().unwrap();
    let mgr = WorkflowStateManager::new(td.path().to_path_buf());

    mgr.start_workflow_with_steps(
        "s",
        "wf",
        0,
        vec!["a".into(), "b".into(), "c".into(), "d".into(), "e".into()],
    )
    .unwrap();

    // Interleaved, non-monotonic advances.
    mgr.advance_step("s", "c", 10).unwrap();
    mgr.advance_step("s", "a", 20).unwrap();
    mgr.advance_step("s", "e", 30).unwrap();

    let gap = mgr.detect_gap("s").unwrap();
    assert_eq!(
        gap,
        vec!["b".to_string(), "d".to_string()],
        "gap preserves `expected_steps` declaration order"
    );
}

#[test]
fn advance_step_after_complete_is_invalid_input() {
    let td = tempdir().unwrap();
    let mgr = WorkflowStateManager::new(td.path().to_path_buf());

    mgr.start_workflow_with_steps("s", "wf", 0, vec!["a".into(), "b".into()])
        .unwrap();
    mgr.advance_step("s", "a", 1).unwrap();
    mgr.complete_workflow("s", 2).unwrap();

    let err = mgr.advance_step("s", "b", 3).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);

    // detect_gap still works on a completed workflow.
    let gap = mgr.detect_gap("s").unwrap();
    assert_eq!(gap, vec!["b".to_string()]);
}

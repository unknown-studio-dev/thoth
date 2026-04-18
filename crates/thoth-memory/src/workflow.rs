//! Workflow gate state types (Phase 4a / 4b of the enforcement layer).
//!
//! Tracks an active workflow session initiated via `thoth_workflow_start`,
//! advanced through checkpoints via `thoth_workflow_advance`, and finished
//! with `thoth_workflow_complete`. Persisted at
//! `.thoth/workflow/<session_id>.json`.
//!
//! See DESIGN-SPEC REQ-19 / REQ-20.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Status of an in-flight workflow session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStatus {
    /// Workflow has been started and not yet completed or abandoned.
    Active,
    /// Workflow finished cleanly via `thoth_workflow_complete`.
    Completed,
    /// Session ended (Stop hook) without completion.
    Abandoned,
}

/// Persisted state for a single workflow session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowState {
    /// Claude Code session id this workflow is bound to.
    pub session_id: String,
    /// Name of the workflow (e.g. the slash command that kicked it off).
    pub workflow_name: String,
    /// Unix epoch seconds when the workflow was started.
    pub started_at: i64,
    /// Optional ordered list of expected checkpoint step ids (Phase 4b).
    /// Empty for Phase 4a (simple start/complete).
    #[serde(default)]
    pub expected_steps: Vec<String>,
    /// Step ids that have been observed via `thoth_workflow_advance`.
    #[serde(default)]
    pub completed_steps: Vec<String>,
    /// Unix epoch seconds of the most recent `thoth_workflow_advance`.
    #[serde(default)]
    pub last_step_at: Option<i64>,
    /// Current status.
    pub status: WorkflowStatus,
}

impl WorkflowState {
    /// Create a fresh Phase 4a workflow (no expected checkpoints).
    pub fn new_simple(
        session_id: impl Into<String>,
        workflow_name: impl Into<String>,
        started_at: i64,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            workflow_name: workflow_name.into(),
            started_at,
            expected_steps: Vec::new(),
            completed_steps: Vec::new(),
            last_step_at: None,
            status: WorkflowStatus::Active,
        }
    }

    /// Whether every expected step has been reported as completed.
    ///
    /// For Phase 4a (`expected_steps` empty) this is vacuously true.
    pub fn all_steps_completed(&self) -> bool {
        self.expected_steps
            .iter()
            .all(|step| self.completed_steps.iter().any(|c| c == step))
    }

    /// Record a step as completed if not already present. Returns true if
    /// this is the first time we've seen `step_id`.
    pub fn record_step(&mut self, step_id: impl Into<String>, at: i64) -> bool {
        let step = step_id.into();
        if self.completed_steps.iter().any(|c| c == &step) {
            return false;
        }
        self.completed_steps.push(step);
        self.last_step_at = Some(at);
        true
    }
}

/// A single violation record appended to `workflow-violations.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowViolation {
    /// Claude Code session id that produced the violation.
    pub session_id: String,
    /// Name of the workflow that was active (or empty if none).
    pub workflow_name: String,
    /// Free-form reason, e.g. `"stop_without_complete"`.
    pub reason: String,
    /// Unix epoch seconds.
    pub detected_at: i64,
}

/// On-disk manager for Phase 4a workflow state.
///
/// Layout under `root`:
///
/// - `workflow/<session_id>.json`          — one file per session
/// - `workflow-violations.jsonl`           — append-only violation log
#[derive(Debug, Clone)]
pub struct WorkflowStateManager {
    root: PathBuf,
}

impl WorkflowStateManager {
    /// Wrap an existing `.thoth` directory. Does not create it eagerly.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Directory holding per-session state files.
    pub fn workflow_dir(&self) -> PathBuf {
        self.root.join("workflow")
    }

    /// Path to the violation log.
    pub fn violations_path(&self) -> PathBuf {
        self.root.join("workflow-violations.jsonl")
    }

    fn state_path(&self, session_id: &str) -> PathBuf {
        self.workflow_dir().join(format!("{session_id}.json"))
    }

    fn ensure_dir(&self) -> io::Result<()> {
        fs::create_dir_all(self.workflow_dir())
    }

    fn write_state(&self, state: &WorkflowState) -> io::Result<()> {
        self.ensure_dir()?;
        let json = serde_json::to_string_pretty(state)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        fs::write(self.state_path(&state.session_id), json)
    }

    fn read_state_from(path: &Path) -> io::Result<WorkflowState> {
        let raw = fs::read_to_string(path)?;
        serde_json::from_str(&raw).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// Start a fresh Phase 4a workflow for `session_id`.
    ///
    /// Overwrites any existing state for the session. Returns the
    /// persisted [`WorkflowState`].
    pub fn start_workflow(
        &self,
        session_id: impl Into<String>,
        workflow_name: impl Into<String>,
        started_at: i64,
    ) -> io::Result<WorkflowState> {
        let state = WorkflowState::new_simple(session_id, workflow_name, started_at);
        self.write_state(&state)?;
        Ok(state)
    }

    /// Start a Phase 4b workflow with an ordered list of expected
    /// checkpoint step ids.
    pub fn start_workflow_with_steps(
        &self,
        session_id: impl Into<String>,
        workflow_name: impl Into<String>,
        started_at: i64,
        expected_steps: Vec<String>,
    ) -> io::Result<WorkflowState> {
        let mut state = WorkflowState::new_simple(session_id, workflow_name, started_at);
        state.expected_steps = expected_steps;
        self.write_state(&state)?;
        Ok(state)
    }

    /// Record a checkpoint step as completed for `session_id` (Phase 4b).
    ///
    /// Loads the persisted state, appends `step_id` to `completed_steps`
    /// (deduped), updates `last_step_at`, and persists. Errors with
    /// `NotFound` if no state exists, or `InvalidInput` if the workflow
    /// is not `Active`.
    pub fn advance_step(
        &self,
        session_id: &str,
        step_id: impl Into<String>,
        at: i64,
    ) -> io::Result<WorkflowState> {
        let path = self.state_path(session_id);
        let mut state = Self::read_state_from(&path)?;
        if state.status != WorkflowStatus::Active {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("workflow for session `{session_id}` is not active"),
            ));
        }
        state.record_step(step_id, at);
        self.write_state(&state)?;
        Ok(state)
    }

    /// Compute the ordered list of expected steps that have not yet been
    /// recorded for `session_id`. Preserves `expected_steps` order.
    ///
    /// Errors with `NotFound` if no state exists. For Phase 4a workflows
    /// (`expected_steps` empty) returns an empty vec.
    pub fn detect_gap(&self, session_id: &str) -> io::Result<Vec<String>> {
        let path = self.state_path(session_id);
        let state = Self::read_state_from(&path)?;
        let gap = state
            .expected_steps
            .iter()
            .filter(|s| !state.completed_steps.iter().any(|c| c == *s))
            .cloned()
            .collect();
        Ok(gap)
    }

    /// Mark an active workflow as completed.
    ///
    /// Returns the updated state. Errors with `NotFound` if no state
    /// exists for the session.
    pub fn complete_workflow(
        &self,
        session_id: &str,
        completed_at: i64,
    ) -> io::Result<WorkflowState> {
        let path = self.state_path(session_id);
        let mut state = Self::read_state_from(&path)?;
        state.status = WorkflowStatus::Completed;
        state.last_step_at = Some(completed_at);
        self.write_state(&state)?;
        Ok(state)
    }

    /// Mark an active workflow as abandoned (e.g. via `thoth workflow
    /// reset`). Returns the updated state. Errors with `NotFound` if no
    /// state exists for the session.
    pub fn abandon_workflow(&self, session_id: &str, at: i64) -> io::Result<WorkflowState> {
        let path = self.state_path(session_id);
        let mut state = Self::read_state_from(&path)?;
        state.status = WorkflowStatus::Abandoned;
        state.last_step_at = Some(at);
        self.write_state(&state)?;
        Ok(state)
    }

    /// Clear violation log rows for `session_id`, leaving rows for other
    /// sessions intact. Returns the number of rows removed. If the log
    /// does not exist, returns `Ok(0)`.
    pub fn clear_violations_for(&self, session_id: &str) -> io::Result<u32> {
        let path = self.violations_path();
        let raw = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e),
        };
        let mut removed: u32 = 0;
        let mut kept = String::new();
        for row in raw.lines() {
            let trimmed = row.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<WorkflowViolation>(trimmed) {
                Ok(v) if v.session_id == session_id => {
                    removed = removed.saturating_add(1);
                }
                _ => {
                    kept.push_str(row);
                    kept.push('\n');
                }
            }
        }
        fs::write(&path, kept)?;
        Ok(removed)
    }

    /// Fetch the persisted state for `session_id`, if any.
    pub fn get(&self, session_id: &str) -> io::Result<Option<WorkflowState>> {
        let path = self.state_path(session_id);
        match Self::read_state_from(&path) {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// List every persisted workflow with status `Active`.
    pub fn list_active(&self) -> io::Result<Vec<WorkflowState>> {
        let dir = self.workflow_dir();
        let mut out = Vec::new();
        let iter = match fs::read_dir(&dir) {
            Ok(i) => i,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e),
        };
        for entry in iter {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            match Self::read_state_from(&path) {
                Ok(s) if s.status == WorkflowStatus::Active => out.push(s),
                Ok(_) => {}
                // Skip malformed files rather than blowing up the listing.
                Err(e) if e.kind() == io::ErrorKind::InvalidData => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    /// Append a violation to `workflow-violations.jsonl` and return the
    /// running total of violations recorded for `session_id` **within
    /// the trailing `window_secs` window** anchored at `detected_at`.
    ///
    /// This count is what REQ-19 compares against
    /// `workflow_violation_threshold`.
    pub fn increment_violation(
        &self,
        session_id: impl Into<String>,
        workflow_name: impl Into<String>,
        reason: impl Into<String>,
        detected_at: i64,
        window_secs: i64,
    ) -> io::Result<u32> {
        self.ensure_dir()?;
        let record = WorkflowViolation {
            session_id: session_id.into(),
            workflow_name: workflow_name.into(),
            reason: reason.into(),
            detected_at,
        };
        let line = serde_json::to_string(&record)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let path = self.violations_path();
        // Append-only, create if missing.
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(file, "{line}")?;

        // Tally matching rows in the window (inclusive lower bound).
        let threshold = detected_at.saturating_sub(window_secs);
        let raw = fs::read_to_string(&path)?;
        let mut count: u32 = 0;
        for row in raw.lines() {
            let row = row.trim();
            if row.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<WorkflowViolation>(row) else {
                continue;
            };
            if v.session_id == record.session_id && v.detected_at >= threshold {
                count = count.saturating_add(1);
            }
        }
        Ok(count)
    }
}

#[cfg(test)]
mod types {
    use super::*;

    #[test]
    fn status_serializes_snake_case() {
        let active = serde_json::to_string(&WorkflowStatus::Active).unwrap();
        assert_eq!(active, "\"active\"");
        let completed = serde_json::to_string(&WorkflowStatus::Completed).unwrap();
        assert_eq!(completed, "\"completed\"");
        let abandoned = serde_json::to_string(&WorkflowStatus::Abandoned).unwrap();
        assert_eq!(abandoned, "\"abandoned\"");
    }

    #[test]
    fn status_roundtrip() {
        for s in [
            WorkflowStatus::Active,
            WorkflowStatus::Completed,
            WorkflowStatus::Abandoned,
        ] {
            let json = serde_json::to_string(&s).unwrap();
            let back: WorkflowStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(s, back);
        }
    }

    #[test]
    fn new_simple_defaults_active_empty_steps() {
        let ws = WorkflowState::new_simple("sess-1", "hoangsa:cook", 1_700_000_000);
        assert_eq!(ws.session_id, "sess-1");
        assert_eq!(ws.workflow_name, "hoangsa:cook");
        assert_eq!(ws.started_at, 1_700_000_000);
        assert!(ws.expected_steps.is_empty());
        assert!(ws.completed_steps.is_empty());
        assert!(ws.last_step_at.is_none());
        assert_eq!(ws.status, WorkflowStatus::Active);
    }

    #[test]
    fn all_steps_completed_is_vacuously_true_when_no_expected() {
        let ws = WorkflowState::new_simple("s", "w", 0);
        assert!(ws.all_steps_completed());
    }

    #[test]
    fn all_steps_completed_tracks_expected() {
        let mut ws = WorkflowState::new_simple("s", "w", 0);
        ws.expected_steps = vec!["1a".into(), "1b".into(), "2".into()];
        assert!(!ws.all_steps_completed());
        ws.record_step("1a", 10);
        ws.record_step("1b", 20);
        assert!(!ws.all_steps_completed());
        ws.record_step("2", 30);
        assert!(ws.all_steps_completed());
        assert_eq!(ws.last_step_at, Some(30));
    }

    #[test]
    fn record_step_dedupes() {
        let mut ws = WorkflowState::new_simple("s", "w", 0);
        assert!(ws.record_step("1a", 10));
        assert!(!ws.record_step("1a", 20));
        assert_eq!(ws.completed_steps, vec!["1a".to_string()]);
        // last_step_at stays at the first acceptance.
        assert_eq!(ws.last_step_at, Some(10));
    }

    #[test]
    fn state_roundtrip_json() {
        let mut ws = WorkflowState::new_simple("sess-xyz", "hoangsa:cook", 1_700_000_000);
        ws.expected_steps = vec!["menu".into(), "prepare".into(), "cook".into()];
        ws.record_step("menu", 1_700_000_100);
        let json = serde_json::to_string(&ws).unwrap();
        let back: WorkflowState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.session_id, ws.session_id);
        assert_eq!(back.workflow_name, ws.workflow_name);
        assert_eq!(back.started_at, ws.started_at);
        assert_eq!(back.expected_steps, ws.expected_steps);
        assert_eq!(back.completed_steps, ws.completed_steps);
        assert_eq!(back.last_step_at, ws.last_step_at);
        assert_eq!(back.status, ws.status);
    }
}

#[cfg(test)]
mod phase4a {
    use super::*;
    use tempfile::tempdir;

    fn mgr() -> (tempfile::TempDir, WorkflowStateManager) {
        let td = tempdir().unwrap();
        let mgr = WorkflowStateManager::new(td.path().to_path_buf());
        (td, mgr)
    }

    #[test]
    fn start_persists_state_file() {
        let (_td, mgr) = mgr();
        let st = mgr
            .start_workflow("sess-1", "hoangsa:cook", 1_700_000_000)
            .unwrap();
        assert_eq!(st.status, WorkflowStatus::Active);
        let path = mgr.workflow_dir().join("sess-1.json");
        assert!(path.exists());
        let back = mgr.get("sess-1").unwrap().unwrap();
        assert_eq!(back.workflow_name, "hoangsa:cook");
        assert_eq!(back.started_at, 1_700_000_000);
    }

    #[test]
    fn get_missing_returns_none() {
        let (_td, mgr) = mgr();
        assert!(mgr.get("nope").unwrap().is_none());
    }

    #[test]
    fn complete_flips_status_and_sets_last_step_at() {
        let (_td, mgr) = mgr();
        mgr.start_workflow("sess-2", "wf", 1_000).unwrap();
        let done = mgr.complete_workflow("sess-2", 2_000).unwrap();
        assert_eq!(done.status, WorkflowStatus::Completed);
        assert_eq!(done.last_step_at, Some(2_000));
        // Re-read from disk to confirm persistence.
        let back = mgr.get("sess-2").unwrap().unwrap();
        assert_eq!(back.status, WorkflowStatus::Completed);
    }

    #[test]
    fn complete_missing_session_is_not_found() {
        let (_td, mgr) = mgr();
        let err = mgr.complete_workflow("ghost", 10).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn list_active_filters_completed() {
        let (_td, mgr) = mgr();
        mgr.start_workflow("a", "wf", 1).unwrap();
        mgr.start_workflow("b", "wf", 2).unwrap();
        mgr.start_workflow("c", "wf", 3).unwrap();
        mgr.complete_workflow("b", 99).unwrap();
        let mut active: Vec<String> = mgr
            .list_active()
            .unwrap()
            .into_iter()
            .map(|s| s.session_id)
            .collect();
        active.sort();
        assert_eq!(active, vec!["a".to_string(), "c".to_string()]);
    }

    #[test]
    fn list_active_on_empty_dir_returns_empty() {
        let (_td, mgr) = mgr();
        assert!(mgr.list_active().unwrap().is_empty());
    }

    #[test]
    fn list_active_skips_malformed_files() {
        let (_td, mgr) = mgr();
        mgr.start_workflow("ok", "wf", 1).unwrap();
        fs::create_dir_all(mgr.workflow_dir()).unwrap();
        fs::write(mgr.workflow_dir().join("junk.json"), "not json").unwrap();
        let active = mgr.list_active().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].session_id, "ok");
    }

    #[test]
    fn increment_violation_appends_and_counts_in_window() {
        let (_td, mgr) = mgr();
        // Window = 7 days.
        let window = 7 * 24 * 60 * 60;
        let t0 = 1_700_000_000;
        let n1 = mgr
            .increment_violation("s1", "wf", "stop_without_complete", t0, window)
            .unwrap();
        assert_eq!(n1, 1);
        let n2 = mgr
            .increment_violation("s1", "wf", "stop_without_complete", t0 + 10, window)
            .unwrap();
        assert_eq!(n2, 2);
        let n3 = mgr
            .increment_violation("s1", "wf", "stop_without_complete", t0 + 20, window)
            .unwrap();
        assert_eq!(n3, 3);
        // Different session is tracked separately.
        let other = mgr
            .increment_violation("s2", "wf", "stop_without_complete", t0 + 30, window)
            .unwrap();
        assert_eq!(other, 1);
    }

    #[test]
    fn increment_violation_drops_rows_outside_window() {
        let (_td, mgr) = mgr();
        let window: i64 = 100;
        // Ancient violation.
        mgr.increment_violation("s", "wf", "r", 1_000, window)
            .unwrap();
        // Newer violation outside the 100s window of the old one.
        let count = mgr
            .increment_violation("s", "wf", "r", 2_000, window)
            .unwrap();
        // Only the t=2000 record falls in [1900, 2000].
        assert_eq!(count, 1);
    }

    #[test]
    fn increment_violation_writes_parseable_jsonl() {
        let (_td, mgr) = mgr();
        mgr.increment_violation("sx", "hoangsa:cook", "stop_without_complete", 42, 7)
            .unwrap();
        let raw = fs::read_to_string(mgr.violations_path()).unwrap();
        let line = raw.lines().next().unwrap();
        let parsed: WorkflowViolation = serde_json::from_str(line).unwrap();
        assert_eq!(parsed.session_id, "sx");
        assert_eq!(parsed.workflow_name, "hoangsa:cook");
        assert_eq!(parsed.reason, "stop_without_complete");
        assert_eq!(parsed.detected_at, 42);
    }
}

#[cfg(test)]
mod phase4b {
    use super::*;
    use tempfile::tempdir;

    fn mgr() -> (tempfile::TempDir, WorkflowStateManager) {
        let td = tempdir().unwrap();
        let m = WorkflowStateManager::new(td.path().to_path_buf());
        (td, m)
    }

    #[test]
    fn start_workflow_with_steps_persists_expected() {
        let (_td, mgr) = mgr();
        let st = mgr
            .start_workflow_with_steps(
                "s1",
                "hoangsa:cook",
                1_000,
                vec!["menu".into(), "prepare".into(), "cook".into()],
            )
            .unwrap();
        assert_eq!(st.expected_steps, vec!["menu", "prepare", "cook"]);
        let back = mgr.get("s1").unwrap().unwrap();
        assert_eq!(back.expected_steps, vec!["menu", "prepare", "cook"]);
    }

    #[test]
    fn advance_step_records_and_persists() {
        let (_td, mgr) = mgr();
        mgr.start_workflow_with_steps("s1", "wf", 1_000, vec!["a".into(), "b".into(), "c".into()])
            .unwrap();
        let st = mgr.advance_step("s1", "a", 1_100).unwrap();
        assert_eq!(st.completed_steps, vec!["a".to_string()]);
        assert_eq!(st.last_step_at, Some(1_100));
        let st = mgr.advance_step("s1", "b", 1_200).unwrap();
        assert_eq!(st.completed_steps, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(st.last_step_at, Some(1_200));
        // Persistence roundtrip.
        let back = mgr.get("s1").unwrap().unwrap();
        assert_eq!(back.completed_steps, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn advance_step_dedupes() {
        let (_td, mgr) = mgr();
        mgr.start_workflow_with_steps("s1", "wf", 0, vec!["a".into()])
            .unwrap();
        mgr.advance_step("s1", "a", 10).unwrap();
        let st = mgr.advance_step("s1", "a", 20).unwrap();
        assert_eq!(st.completed_steps, vec!["a".to_string()]);
        // last_step_at stays at first acceptance.
        assert_eq!(st.last_step_at, Some(10));
    }

    #[test]
    fn advance_step_missing_session_is_not_found() {
        let (_td, mgr) = mgr();
        let err = mgr.advance_step("ghost", "a", 1).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn advance_step_errors_when_not_active() {
        let (_td, mgr) = mgr();
        mgr.start_workflow_with_steps("s1", "wf", 0, vec!["a".into()])
            .unwrap();
        mgr.complete_workflow("s1", 10).unwrap();
        let err = mgr.advance_step("s1", "a", 20).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn detect_gap_returns_expected_minus_completed_in_order() {
        let (_td, mgr) = mgr();
        mgr.start_workflow_with_steps(
            "s1",
            "wf",
            0,
            vec!["1a".into(), "1b".into(), "2".into(), "3".into()],
        )
        .unwrap();
        mgr.advance_step("s1", "1b", 10).unwrap();
        mgr.advance_step("s1", "2", 20).unwrap();
        let gap = mgr.detect_gap("s1").unwrap();
        assert_eq!(gap, vec!["1a".to_string(), "3".to_string()]);
    }

    #[test]
    fn detect_gap_empty_when_all_done() {
        let (_td, mgr) = mgr();
        mgr.start_workflow_with_steps("s1", "wf", 0, vec!["a".into(), "b".into()])
            .unwrap();
        mgr.advance_step("s1", "a", 1).unwrap();
        mgr.advance_step("s1", "b", 2).unwrap();
        assert!(mgr.detect_gap("s1").unwrap().is_empty());
    }

    #[test]
    fn detect_gap_phase4a_workflow_is_empty() {
        let (_td, mgr) = mgr();
        mgr.start_workflow("s1", "wf", 0).unwrap();
        assert!(mgr.detect_gap("s1").unwrap().is_empty());
    }

    #[test]
    fn detect_gap_missing_session_is_not_found() {
        let (_td, mgr) = mgr();
        let err = mgr.detect_gap("ghost").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}

//! Outcome harvester — turns PostToolUse events into lesson reinforcement
//! signals, violation records, and auto-promote/demote tier changes.
//!
//! See `DESIGN-SPEC.md` REQ-12 / REQ-13 / REQ-14 and the T-13 task context.
//!
//! # Role
//!
//! After every tool call Claude Code runs, the PostToolUse hook calls into
//! this module with:
//!
//! - the [`ToolCall`] that was executed (tool name + path / command / content),
//! - the outcome (`is_error`),
//! - the active session id,
//! - the set of lessons currently on disk paired with their structured
//!   [`LessonTrigger`] (decoupled here — the store has no opinion on
//!   triggers; callers assemble this view).
//!
//! The harvester then, for every lesson whose structured trigger matches:
//!
//! 1. Bumps `success_count` on a clean tool call, or `failure_count` +
//!    appends a [`Violation`] row to `.thoth/violations.jsonl` on an error.
//! 2. Runs the [`promotion`] engine's `evaluate_and_apply` to auto-promote
//!    or auto-demote the lesson's enforcement tier in place.
//! 3. Records the outcome in a [`HarvestReport`] so callers can persist the
//!    mutated lessons via `rewrite_lessons` and log tier flips to the audit
//!    trail.
//!
//! The module is pure-ish: it mutates `Lesson` values in place and appends
//! to `.thoth/violations.jsonl`, but it does **not** own the markdown store.
//! That keeps the PostToolUse hook free to batch the rewrite once per turn.
//!
//! [`promotion`]: crate::promotion

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thoth_core::memory::{Lesson, LessonTrigger};
use tracing::warn;
use uuid::Uuid;

use crate::EnforcementConfig;
use crate::lesson_matcher::{LessonTriggerExt, ToolCall};
use crate::r#override::Violation;
use crate::promotion::{PromotionAction, evaluate_and_apply};
use crate::workflow::WorkflowStateManager;

/// Filename under the `.thoth/` root where violation rows are appended.
pub const VIOLATIONS_FILE: &str = "violations.jsonl";

/// Lesson + structured trigger passed into the harvester.
///
/// `Lesson.trigger` itself is free text (the natural-language header that
/// gets rendered into `LESSONS.md`); the structured matcher used by the
/// enforcement layer lives alongside it as a separate [`LessonTrigger`]
/// compiled from the lesson's frontmatter. The harvester works on both
/// together so the store layer (which is unaware of [`LessonTrigger`]) can
/// stay decoupled.
#[derive(Debug, Clone)]
pub struct LessonEntry {
    /// The lesson itself — mutated in place when the promotion engine
    /// fires.
    pub lesson: Lesson,
    /// Compiled structured trigger. Use [`LessonTrigger::natural_only`]
    /// for legacy lessons; those will never match mechanically and so are
    /// harmlessly ignored by the harvester.
    pub trigger: LessonTrigger,
}

/// Per-lesson outcome of a single harvest pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LessonOutcome {
    /// `meta.id` of the affected lesson.
    pub lesson_id: String,
    /// Whether the tool call was a clean success (`false`) or an error
    /// (`true`).
    pub was_error: bool,
    /// Tier change produced by the promotion engine, if any. `NoChange`
    /// when the counters didn't cross a threshold.
    pub promotion: PromotionAction,
}

/// Aggregate of everything the harvester did in one pass.
///
/// Returned so callers can persist the mutated lessons and append audit
/// rows without re-walking the lesson list.
#[derive(Debug, Clone, Default)]
pub struct HarvestReport {
    /// One entry per lesson whose trigger matched the tool call.
    pub lesson_outcomes: Vec<LessonOutcome>,
    /// Violation rows appended to `violations.jsonl` during this pass.
    pub violations: Vec<Violation>,
}

impl HarvestReport {
    /// True when the harvester mutated at least one lesson tier.
    pub fn any_promotion(&self) -> bool {
        self.lesson_outcomes
            .iter()
            .any(|o| !matches!(o.promotion, PromotionAction::NoChange))
    }

    /// True when at least one violation was appended.
    pub fn any_violation(&self) -> bool {
        !self.violations.is_empty()
    }
}

/// Filesystem-bound harvester. Owns the `.thoth/` root (for violation log
/// writes) and a copy of the enforcement config.
#[derive(Debug, Clone)]
pub struct OutcomeHarvester {
    root: PathBuf,
    config: EnforcementConfig,
}

impl OutcomeHarvester {
    /// Wrap an existing `.thoth/` directory with the given config. Does
    /// not create the directory eagerly; `violations.jsonl` is lazily
    /// created on the first violation.
    pub fn new(root: impl Into<PathBuf>, config: EnforcementConfig) -> Self {
        Self {
            root: root.into(),
            config,
        }
    }

    /// Path to the violation JSONL log.
    pub fn violations_path(&self) -> PathBuf {
        self.root.join(VIOLATIONS_FILE)
    }

    /// Process a single PostToolUse event against every lesson.
    ///
    /// See the module doc for the algorithm. All file I/O is confined to
    /// appending matched violations to `violations.jsonl`; lesson
    /// mutations are in-place on the input slice and must be persisted by
    /// the caller (typically via `MarkdownStore::rewrite_lessons`).
    pub fn harvest_post_tool(
        &self,
        call: &ToolCall,
        is_error: bool,
        session_id: &str,
        tool_call_hash: &str,
        detected_at: i64,
        lessons: &mut [LessonEntry],
    ) -> io::Result<HarvestReport> {
        let mut report = HarvestReport::default();

        for entry in lessons.iter_mut() {
            if !entry.trigger.matches(call) {
                continue;
            }
            let lesson_id = entry.lesson.meta.id.to_string();

            if is_error {
                entry.lesson.failure_count = entry.lesson.failure_count.saturating_add(1);
                let v = Violation {
                    id: Uuid::new_v4().to_string(),
                    lesson_id: Some(lesson_id.clone()),
                    rule_id: None,
                    tool_call_hash: tool_call_hash.to_string(),
                    tool: call.tool_name.clone(),
                    detected_at,
                    session_id: session_id.to_string(),
                };
                // A write failure should not poison the harvest — log &
                // continue. The mutated counter is still meaningful.
                match append_violation(&self.violations_path(), &v) {
                    Ok(()) => report.violations.push(v),
                    Err(e) => warn!(error = %e, "outcome_harvest: failed to append violation"),
                }
            } else {
                entry.lesson.success_count = entry.lesson.success_count.saturating_add(1);
            }

            let promotion = evaluate_and_apply(&mut entry.lesson, &self.config);
            report.lesson_outcomes.push(LessonOutcome {
                lesson_id,
                was_error: is_error,
                promotion,
            });
        }

        Ok(report)
    }

    /// Record a workflow-level violation (e.g. Stop hook fires while an
    /// active workflow exists but was never completed).
    ///
    /// Thin pass-through to [`WorkflowStateManager::increment_violation`]
    /// so the PostToolUse / Stop hook has one module to call into.
    pub fn increment_workflow_violation(
        &self,
        session_id: &str,
        workflow_name: &str,
        reason: &str,
        detected_at: i64,
        window_secs: i64,
    ) -> io::Result<u32> {
        let mgr = WorkflowStateManager::new(self.root.clone());
        mgr.increment_violation(
            session_id.to_string(),
            workflow_name.to_string(),
            reason.to_string(),
            detected_at,
            window_secs,
        )
    }
}

fn append_violation(path: &Path, v: &Violation) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let line =
        serde_json::to_string(v).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

/// Convenience: a pass where the caller has already decided there is no
/// lesson mutation to do, but wants to record a direct violation (e.g. a
/// rule-triggered block, surfaced by the gate). Preserves the JSONL
/// format used by [`OutcomeHarvester::harvest_post_tool`].
pub fn append_violation_row(root: impl AsRef<Path>, v: &Violation) -> io::Result<()> {
    append_violation(&root.as_ref().join(VIOLATIONS_FILE), v)
}

/// Serde wrapper so downstream tooling can round-trip a
/// `violations.jsonl` row without depending on `thoth_memory::r#override`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ViolationRow(pub Violation);

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use thoth_core::memory::{Enforcement, MemoryKind, MemoryMeta};

    fn mk_entry(enforcement: Enforcement, trigger: LessonTrigger) -> LessonEntry {
        LessonEntry {
            lesson: Lesson {
                meta: MemoryMeta::new(MemoryKind::Reflective),
                trigger: trigger.natural.clone(),
                advice: "advice".into(),
                success_count: 0,
                failure_count: 0,
                enforcement,
                suggested_enforcement: None,
                block_message: None,
            },
            trigger,
        }
    }

    fn edit_rs_trigger() -> LessonTrigger {
        LessonTrigger {
            tool: Some("Edit".into()),
            path_glob: Some("**/*.rs".into()),
            natural: "edits to rust source".into(),
            ..Default::default()
        }
    }

    fn mk_call() -> ToolCall {
        ToolCall::new("Edit").with_path("src/foo.rs")
    }

    fn setup() -> (TempDir, OutcomeHarvester) {
        let td = TempDir::new().unwrap();
        let h = OutcomeHarvester::new(td.path().to_path_buf(), EnforcementConfig::default());
        (td, h)
    }

    #[test]
    fn non_matching_trigger_is_ignored() {
        let (_td, h) = setup();
        let other = LessonTrigger {
            tool: Some("Bash".into()),
            natural: "bash only".into(),
            ..Default::default()
        };
        let mut entries = vec![mk_entry(Enforcement::Advise, other)];
        let report = h
            .harvest_post_tool(&mk_call(), false, "sess-1", "hash-1", 100, &mut entries)
            .unwrap();
        assert!(report.lesson_outcomes.is_empty());
        assert!(report.violations.is_empty());
        assert_eq!(entries[0].lesson.success_count, 0);
        assert_eq!(entries[0].lesson.failure_count, 0);
    }

    #[test]
    fn success_bumps_success_counter_only() {
        let (_td, h) = setup();
        let mut entries = vec![mk_entry(Enforcement::Advise, edit_rs_trigger())];
        let report = h
            .harvest_post_tool(&mk_call(), false, "s", "hash", 100, &mut entries)
            .unwrap();
        assert_eq!(report.lesson_outcomes.len(), 1);
        assert!(report.violations.is_empty());
        assert_eq!(entries[0].lesson.success_count, 1);
        assert_eq!(entries[0].lesson.failure_count, 0);
    }

    #[test]
    fn error_bumps_failure_and_appends_violation() {
        let (td, h) = setup();
        let mut entries = vec![mk_entry(Enforcement::Advise, edit_rs_trigger())];
        let report = h
            .harvest_post_tool(
                &mk_call(),
                true,
                "sess-xyz",
                "hash-abc",
                1_700_000_000,
                &mut entries,
            )
            .unwrap();
        assert_eq!(entries[0].lesson.failure_count, 1);
        assert_eq!(entries[0].lesson.success_count, 0);
        assert_eq!(report.violations.len(), 1);
        let v = &report.violations[0];
        assert_eq!(v.tool_call_hash, "hash-abc");
        assert_eq!(v.session_id, "sess-xyz");
        assert_eq!(v.detected_at, 1_700_000_000);
        assert_eq!(v.tool, "Edit");
        assert!(v.lesson_id.is_some());
        assert!(v.rule_id.is_none());

        // JSONL file present and parseable.
        let log = td.path().join(VIOLATIONS_FILE);
        assert!(log.exists());
        let raw = fs::read_to_string(&log).unwrap();
        let first = raw.lines().next().unwrap();
        let back: Violation = serde_json::from_str(first).unwrap();
        assert_eq!(&back, v);
    }

    #[test]
    fn two_failures_promote_advise_to_require() {
        let (_td, h) = setup();
        let mut entries = vec![mk_entry(Enforcement::Advise, edit_rs_trigger())];
        // First failure: no promotion yet.
        let r1 = h
            .harvest_post_tool(&mk_call(), true, "s", "h", 100, &mut entries)
            .unwrap();
        assert!(matches!(
            r1.lesson_outcomes[0].promotion,
            PromotionAction::NoChange
        ));
        assert_eq!(entries[0].lesson.enforcement, Enforcement::Advise);

        // Second failure: promote_threshold=2 kicks in.
        let r2 = h
            .harvest_post_tool(&mk_call(), true, "s", "h", 101, &mut entries)
            .unwrap();
        assert!(matches!(
            r2.lesson_outcomes[0].promotion,
            PromotionAction::Promote {
                to: Enforcement::Require,
                ..
            }
        ));
        assert_eq!(entries[0].lesson.enforcement, Enforcement::Require);
        assert!(r2.any_promotion());
    }

    #[test]
    fn two_successes_demote_block_to_require() {
        let (_td, h) = setup();
        let mut entries = vec![mk_entry(Enforcement::Block, edit_rs_trigger())];
        h.harvest_post_tool(&mk_call(), false, "s", "h", 100, &mut entries)
            .unwrap();
        let r2 = h
            .harvest_post_tool(&mk_call(), false, "s", "h", 101, &mut entries)
            .unwrap();
        assert!(matches!(
            r2.lesson_outcomes[0].promotion,
            PromotionAction::Demote {
                to: Enforcement::Require,
                ..
            }
        ));
        assert_eq!(entries[0].lesson.enforcement, Enforcement::Require);
    }

    #[test]
    fn multiple_lessons_matched_independently() {
        let (_td, h) = setup();
        let any_edit = LessonTrigger {
            tool: Some("Any".into()),
            natural: "any tool".into(),
            ..Default::default()
        };
        let mut entries = vec![
            mk_entry(Enforcement::Advise, edit_rs_trigger()),
            mk_entry(Enforcement::Advise, any_edit),
            // Legacy natural-only: never fires mechanically.
            mk_entry(
                Enforcement::Advise,
                LessonTrigger::natural_only("free text"),
            ),
        ];
        let report = h
            .harvest_post_tool(&mk_call(), true, "s", "h", 100, &mut entries)
            .unwrap();
        assert_eq!(report.lesson_outcomes.len(), 2);
        assert_eq!(report.violations.len(), 2);
        // First two matched, counters bumped.
        assert_eq!(entries[0].lesson.failure_count, 1);
        assert_eq!(entries[1].lesson.failure_count, 1);
        // Legacy trigger stayed at zero.
        assert_eq!(entries[2].lesson.failure_count, 0);
    }

    #[test]
    fn auto_promote_off_keeps_tier() {
        let td = TempDir::new().unwrap();
        let cfg = EnforcementConfig {
            auto_promote: false,
            ..EnforcementConfig::default()
        };
        let h = OutcomeHarvester::new(td.path().to_path_buf(), cfg);
        let mut entries = vec![mk_entry(Enforcement::Advise, edit_rs_trigger())];
        for i in 0..5 {
            h.harvest_post_tool(&mk_call(), true, "s", "h", 100 + i, &mut entries)
                .unwrap();
        }
        // Failure counter climbed, but tier is frozen.
        assert_eq!(entries[0].lesson.failure_count, 5);
        assert_eq!(entries[0].lesson.enforcement, Enforcement::Advise);
    }

    #[test]
    fn violation_log_is_append_only() {
        let (td, h) = setup();
        let mut entries = vec![mk_entry(Enforcement::Advise, edit_rs_trigger())];
        h.harvest_post_tool(&mk_call(), true, "s", "h1", 100, &mut entries)
            .unwrap();
        h.harvest_post_tool(&mk_call(), true, "s", "h2", 101, &mut entries)
            .unwrap();
        let raw = fs::read_to_string(td.path().join(VIOLATIONS_FILE)).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in lines {
            let _: Violation = serde_json::from_str(line).unwrap();
        }
    }

    #[test]
    fn workflow_violation_increment_via_harvester() {
        let (td, h) = setup();
        let window: i64 = 7 * 24 * 60 * 60;
        let n1 = h
            .increment_workflow_violation("s", "wf", "stop_without_complete", 1_000, window)
            .unwrap();
        let n2 = h
            .increment_workflow_violation("s", "wf", "stop_without_complete", 1_010, window)
            .unwrap();
        assert_eq!(n1, 1);
        assert_eq!(n2, 2);
        // File materialized under the shared .thoth root.
        let log = td.path().join("workflow-violations.jsonl");
        assert!(log.exists());
    }

    #[test]
    fn append_violation_row_helper_matches_harvester_format() {
        let (td, _h) = setup();
        let v = Violation {
            id: "v-1".into(),
            lesson_id: None,
            rule_id: Some("rule-x".into()),
            tool_call_hash: "h".into(),
            tool: "Bash".into(),
            detected_at: 42,
            session_id: "s".into(),
        };
        append_violation_row(td.path(), &v).unwrap();
        let raw = fs::read_to_string(td.path().join(VIOLATIONS_FILE)).unwrap();
        let parsed: Violation = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
        assert_eq!(parsed, v);
    }

    #[test]
    fn report_flags_track_state() {
        let mut r = HarvestReport::default();
        assert!(!r.any_promotion());
        assert!(!r.any_violation());
        r.lesson_outcomes.push(LessonOutcome {
            lesson_id: "x".into(),
            was_error: false,
            promotion: PromotionAction::NoChange,
        });
        assert!(!r.any_promotion());
        r.lesson_outcomes.push(LessonOutcome {
            lesson_id: "y".into(),
            was_error: true,
            promotion: PromotionAction::Promote {
                from: Enforcement::Advise,
                to: Enforcement::Require,
            },
        });
        assert!(r.any_promotion());
    }

    // ---- Acceptance anchor: the plan wants `outcome_harvest` in the name.

    #[test]
    fn outcome_harvest_end_to_end() {
        let (td, h) = setup();
        let mut entries = vec![
            mk_entry(Enforcement::Advise, edit_rs_trigger()),
            mk_entry(Enforcement::Block, edit_rs_trigger()),
        ];
        // Error on first pass: Advise → NoChange (1/2), Block → NoChange.
        let r1 = h
            .harvest_post_tool(&mk_call(), true, "s", "h", 100, &mut entries)
            .unwrap();
        assert_eq!(r1.violations.len(), 2);
        // Error on second pass: Advise → Require (2/2 threshold).
        let r2 = h
            .harvest_post_tool(&mk_call(), true, "s", "h", 101, &mut entries)
            .unwrap();
        assert!(r2.any_promotion());
        assert_eq!(entries[0].lesson.enforcement, Enforcement::Require);
        assert_eq!(entries[1].lesson.enforcement, Enforcement::Block);
        // Four violation rows total.
        let raw = fs::read_to_string(td.path().join(VIOLATIONS_FILE)).unwrap();
        assert_eq!(raw.lines().count(), 4);
    }
}

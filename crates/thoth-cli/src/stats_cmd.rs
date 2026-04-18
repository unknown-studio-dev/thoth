//! `thoth stats` — surface enforcement-layer telemetry in a single place.
//!
//! Consolidates four on-disk sources the enforcement layer writes across
//! a session:
//!
//! - `<root>/gate.jsonl` — PreToolUse gate verdicts (via `gate.rs`).
//! - `<root>/override-requests`, `overrides/`, `override-rejected/` —
//!   filesystem buckets managed by [`OverrideManager`].
//! - `<root>/workflow-violations.jsonl` — append-only log from
//!   [`WorkflowStateManager::increment_violation`].
//!
//! Counters reported (REQ-27):
//!
//! 1. `blocks` — gate verdicts with `decision == "block"` in window.
//! 2. `overrides` — pending / approved / consumed / rejected counts.
//! 3. `workflow_violations` — top sessions by violation count.
//! 4. `repeated_rules` — top rule ids extracted from rule-layer telemetry
//!    (rows where `reason == "<rule_id>"` and `decision` ∈ {block, nudge}).
//!
//! `--json` (honoured via the global `--json` flag on the CLI root) emits
//! a single machine-readable blob; otherwise a compact human table prints.
//!
//! The window is `weeks` weeks (default 1, `0` = all-time). Filtering is
//! intentionally best-effort: `gate.jsonl` stores ISO-ish `YYYY-MM-DDTHH:MM:SSZ`
//! timestamps that we lex-compare against a computed cutoff string;
//! override / violation records carry Unix epoch seconds natively.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;
use thoth_memory::r#override::{OverrideManager, OverrideStatus};
use thoth_memory::workflow::WorkflowStateManager;

/// Top-N cap for "top sessions" and "top rules" lists.
const TOP_N: usize = 5;

/// Entry point for `thoth stats`.
///
/// * `root` — `.thoth/` directory.
/// * `weeks` — lookback window in weeks. `0` = all-time.
/// * `json`  — honour the global `--json` flag.
pub async fn run(root: &Path, weeks: u32, json: bool) -> Result<()> {
    let now = now_seconds();
    let cutoff_secs = if weeks == 0 {
        0
    } else {
        now.saturating_sub(i64::from(weeks) * 7 * 86_400)
    };
    let cutoff_iso = if weeks == 0 {
        String::new()
    } else {
        iso_from_unix(cutoff_secs)
    };

    let blocks = count_blocks(root, &cutoff_iso).context("scan gate.jsonl")?;
    let rule_hits = count_rule_hits(root, &cutoff_iso).context("scan gate.jsonl (rules)")?;
    let overrides = count_overrides(root, cutoff_secs).context("scan overrides")?;
    let violations =
        count_violations(root, cutoff_secs).context("scan workflow-violations.jsonl")?;

    if json {
        emit_json(weeks, &blocks, &rule_hits, &overrides, &violations)?;
    } else {
        emit_text(weeks, &blocks, &rule_hits, &overrides, &violations);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Counters
// ---------------------------------------------------------------------------

#[derive(Default, Debug, Clone, Copy)]
struct BlockCounts {
    block: u64,
    nudge: u64,
    pass: u64,
    total: u64,
}

#[derive(Default, Debug, Clone)]
struct OverrideCounts {
    pending: u64,
    approved: u64,
    consumed: u64,
    rejected: u64,
}

fn count_blocks(root: &Path, cutoff_iso: &str) -> Result<BlockCounts> {
    let path = root.join("gate.jsonl");
    let mut c = BlockCounts::default();
    let Ok(body) = fs::read_to_string(&path) else {
        return Ok(c); // missing log → zero counts
    };
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue, // tolerate corrupt rows
        };
        if !ts_in_window(&v, cutoff_iso) {
            continue;
        }
        c.total += 1;
        match v.get("decision").and_then(|d| d.as_str()) {
            Some("block") => c.block += 1,
            Some("nudge") => c.nudge += 1,
            Some("pass") => c.pass += 1,
            _ => {}
        }
    }
    Ok(c)
}

/// Rule-layer telemetry writes `reason = "<rule_id>"` for block/nudge rows
/// (see `rule_telemetry` in gate.rs). We aggregate those only — generic
/// recency / relevance rows would swamp the table otherwise.
fn count_rule_hits(root: &Path, cutoff_iso: &str) -> Result<BTreeMap<String, u64>> {
    let path = root.join("gate.jsonl");
    let mut hits: BTreeMap<String, u64> = BTreeMap::new();
    let Ok(body) = fs::read_to_string(&path) else {
        return Ok(hits);
    };
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if !ts_in_window(&v, cutoff_iso) {
            continue;
        }
        let decision = v.get("decision").and_then(|d| d.as_str()).unwrap_or("");
        if decision != "block" && decision != "nudge" {
            continue;
        }
        let reason = match v.get("reason").and_then(|r| r.as_str()) {
            Some(r) if looks_like_rule_id(r) => r,
            _ => continue,
        };
        *hits.entry(reason.to_string()).or_default() += 1;
    }
    Ok(hits)
}

fn count_overrides(root: &Path, cutoff_secs: i64) -> Result<OverrideCounts> {
    let mgr = OverrideManager::new(root);
    let mut c = OverrideCounts::default();
    let in_window = |ts: i64| cutoff_secs == 0 || ts >= cutoff_secs;

    // `list_pending` / `list_approved` / `list_rejected` gracefully handle
    // missing directories by returning an empty vec, so no pre-check.
    for r in mgr.list_pending().unwrap_or_default() {
        if in_window(r.requested_at) {
            c.pending += 1;
        }
    }
    for r in mgr.list_approved().unwrap_or_default() {
        if !in_window(r.requested_at) {
            continue;
        }
        match r.status {
            OverrideStatus::Consumed { .. } => c.consumed += 1,
            _ => c.approved += 1,
        }
    }
    for r in mgr.list_rejected().unwrap_or_default() {
        if in_window(r.requested_at) {
            c.rejected += 1;
        }
    }
    Ok(c)
}

fn count_violations(root: &Path, cutoff_secs: i64) -> Result<BTreeMap<String, u64>> {
    let mgr = WorkflowStateManager::new(root);
    let path = mgr.violations_path();
    let mut by_session: BTreeMap<String, u64> = BTreeMap::new();
    let Ok(body) = fs::read_to_string(&path) else {
        return Ok(by_session);
    };
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let detected = v.get("detected_at").and_then(|t| t.as_i64()).unwrap_or(0);
        if cutoff_secs != 0 && detected < cutoff_secs {
            continue;
        }
        let sid = v
            .get("session_id")
            .and_then(|s| s.as_str())
            .unwrap_or("<unknown>")
            .to_string();
        *by_session.entry(sid).or_default() += 1;
    }
    Ok(by_session)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn emit_json(
    weeks: u32,
    blocks: &BlockCounts,
    rule_hits: &BTreeMap<String, u64>,
    overrides: &OverrideCounts,
    violations: &BTreeMap<String, u64>,
) -> Result<()> {
    let top_rules = top_n(rule_hits, TOP_N);
    let top_sessions = top_n(violations, TOP_N);
    let payload = serde_json::json!({
        "window_weeks": weeks,
        "blocks": {
            "block": blocks.block,
            "nudge": blocks.nudge,
            "pass":  blocks.pass,
            "total": blocks.total,
        },
        "overrides": {
            "pending":  overrides.pending,
            "approved": overrides.approved,
            "consumed": overrides.consumed,
            "rejected": overrides.rejected,
        },
        "workflow_violations": {
            "total": violations.values().sum::<u64>(),
            "top_sessions": top_sessions
                .iter()
                .map(|(s, n)| serde_json::json!({"session_id": s, "count": n}))
                .collect::<Vec<_>>(),
        },
        "repeated_rules": top_rules
            .iter()
            .map(|(r, n)| serde_json::json!({"rule_id": r, "count": n}))
            .collect::<Vec<_>>(),
    });
    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(())
}

fn emit_text(
    weeks: u32,
    blocks: &BlockCounts,
    rule_hits: &BTreeMap<String, u64>,
    overrides: &OverrideCounts,
    violations: &BTreeMap<String, u64>,
) {
    if weeks == 0 {
        println!("Enforcement stats (all-time):");
    } else {
        println!("Enforcement stats (last {weeks} week(s)):");
    }

    println!("  Gate verdicts:");
    println!("    block  {}", blocks.block);
    println!("    nudge  {}", blocks.nudge);
    println!("    pass   {}", blocks.pass);
    println!("    total  {}", blocks.total);

    println!("  Overrides:");
    println!("    pending   {}", overrides.pending);
    println!("    approved  {}", overrides.approved);
    println!("    consumed  {}", overrides.consumed);
    println!("    rejected  {}", overrides.rejected);

    let total_viol: u64 = violations.values().sum();
    println!("  Workflow violations ({total_viol} total):");
    let top_sessions = top_n(violations, TOP_N);
    if top_sessions.is_empty() {
        println!("    (none)");
    } else {
        for (sid, n) in &top_sessions {
            println!("    {n:>4}  {sid}");
        }
    }

    println!("  Repeated rules (top {TOP_N}):");
    let top_rules = top_n(rule_hits, TOP_N);
    if top_rules.is_empty() {
        println!("    (none)");
    } else {
        for (rid, n) in &top_rules {
            println!("    {n:>4}  {rid}");
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn top_n(map: &BTreeMap<String, u64>, n: usize) -> Vec<(String, u64)> {
    let mut v: Vec<(String, u64)> = map.iter().map(|(k, v)| (k.clone(), *v)).collect();
    // Descending by count, then ascending by key for stable output.
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.truncate(n);
    v
}

fn ts_in_window(v: &Value, cutoff_iso: &str) -> bool {
    if cutoff_iso.is_empty() {
        return true;
    }
    match v.get("ts").and_then(|t| t.as_str()) {
        // ISO-8601 UTC with fixed width → lexicographic compare is a valid
        // chronological compare. Matches `gate.rs::now_iso` output exactly.
        Some(ts) => ts >= cutoff_iso,
        None => true, // legacy rows without ts — include rather than silently drop
    }
}

/// Rule ids are dotted / dashed identifiers; gate.rs writes reasons like
/// `"require_recall: rule-id"` or free-text for non-rule rows. We only
/// treat the token as a rule id when it matches a conservative pattern.
fn looks_like_rule_id(s: &str) -> bool {
    !s.is_empty()
        && !s.contains(' ')
        && !s.contains(':')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Minimal epoch → `YYYY-MM-DDTHH:MM:SSZ`. Matches `gate.rs::now_iso`'s
/// format so `ts_in_window` can lex-compare strings.
fn iso_from_unix(secs: i64) -> String {
    // Clamp negatives just in case — predates 1970 means "include everything".
    let secs = secs.max(0) as u64;
    let days = secs / 86_400;
    let h = (secs / 3_600) % 24;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    let (y, mo, d) = ymd_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Civil-date conversion (Howard Hinnant). Mirrors `gate.rs::ymd_from_days`.
fn ymd_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_gate_log(root: &Path, lines: &[&str]) {
        let p = root.join("gate.jsonl");
        let mut f = std::fs::File::create(p).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    fn write_violations(root: &Path, rows: &[(&str, i64)]) {
        let p = root.join("workflow-violations.jsonl");
        let mut f = std::fs::File::create(p).unwrap();
        for (sid, at) in rows {
            writeln!(
                f,
                r#"{{"session_id":"{sid}","workflow_name":"w","reason":"stop_without_complete","detected_at":{at}}}"#
            )
            .unwrap();
        }
    }

    #[test]
    fn count_blocks_counts_by_decision() {
        let dir = TempDir::new().unwrap();
        write_gate_log(
            dir.path(),
            &[
                r#"{"ts":"2026-04-18T10:00:00Z","decision":"block","reason":"no-rm-rf"}"#,
                r#"{"ts":"2026-04-18T10:01:00Z","decision":"block","reason":"no-force-push"}"#,
                r#"{"ts":"2026-04-18T10:02:00Z","decision":"nudge","reason":"soft-rule"}"#,
                r#"{"ts":"2026-04-18T10:03:00Z","decision":"pass","reason":""}"#,
                "garbage line",
                "",
            ],
        );
        let c = count_blocks(dir.path(), "").unwrap();
        assert_eq!(c.block, 2);
        assert_eq!(c.nudge, 1);
        assert_eq!(c.pass, 1);
        assert_eq!(c.total, 4);
    }

    #[test]
    fn count_blocks_applies_iso_cutoff() {
        let dir = TempDir::new().unwrap();
        write_gate_log(
            dir.path(),
            &[
                r#"{"ts":"2026-04-10T10:00:00Z","decision":"block","reason":"r"}"#,
                r#"{"ts":"2026-04-17T10:00:00Z","decision":"block","reason":"r"}"#,
            ],
        );
        // Cutoff between the two rows → one row survives.
        let c = count_blocks(dir.path(), "2026-04-15T00:00:00Z").unwrap();
        assert_eq!(c.block, 1);
        assert_eq!(c.total, 1);
    }

    #[test]
    fn count_rule_hits_only_counts_identifier_reasons() {
        let dir = TempDir::new().unwrap();
        write_gate_log(
            dir.path(),
            &[
                r#"{"ts":"2026-04-18T10:00:00Z","decision":"block","reason":"no-rm-rf"}"#,
                r#"{"ts":"2026-04-18T10:00:00Z","decision":"block","reason":"no-rm-rf"}"#,
                r#"{"ts":"2026-04-18T10:00:00Z","decision":"nudge","reason":"soft.rule"}"#,
                // Free-text reasons — excluded.
                r#"{"ts":"2026-04-18T10:00:00Z","decision":"block","reason":"require_recall: x"}"#,
                r#"{"ts":"2026-04-18T10:00:00Z","decision":"pass","reason":"no-rm-rf"}"#,
            ],
        );
        let hits = count_rule_hits(dir.path(), "").unwrap();
        assert_eq!(hits.get("no-rm-rf"), Some(&2));
        assert_eq!(hits.get("soft.rule"), Some(&1));
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn count_overrides_buckets_pending_approved_consumed_rejected() {
        let dir = TempDir::new().unwrap();
        let mgr = OverrideManager::new(dir.path());
        let now = now_seconds();
        // pending
        mgr.request("rule-a", "r", "h1", "s", now).unwrap();
        // approved
        let a = mgr.request("rule-b", "r", "h2", "s", now).unwrap();
        mgr.approve(&a.id, now, 1).unwrap();
        // approved+consumed
        let c = mgr.request("rule-c", "r", "h3", "s", now).unwrap();
        mgr.approve(&c.id, now, 1).unwrap();
        mgr.consume_if_match("rule-c", "h3", now).unwrap();
        // rejected
        let r = mgr.request("rule-d", "r", "h4", "s", now).unwrap();
        mgr.reject(&r.id, now, Some("no".into())).unwrap();

        let counts = count_overrides(dir.path(), 0).unwrap();
        assert_eq!(counts.pending, 1);
        assert_eq!(counts.approved, 1);
        assert_eq!(counts.consumed, 1);
        assert_eq!(counts.rejected, 1);
    }

    #[test]
    fn count_violations_groups_by_session() {
        let dir = TempDir::new().unwrap();
        let now = now_seconds();
        write_violations(
            dir.path(),
            &[
                ("sess-a", now),
                ("sess-a", now),
                ("sess-a", now),
                ("sess-b", now),
                ("sess-c", now - 30 * 86_400), // outside 1w window
            ],
        );
        let all = count_violations(dir.path(), 0).unwrap();
        assert_eq!(all.get("sess-a"), Some(&3));
        assert_eq!(all.get("sess-b"), Some(&1));
        assert_eq!(all.get("sess-c"), Some(&1));

        let cutoff = now - 7 * 86_400;
        let windowed = count_violations(dir.path(), cutoff).unwrap();
        assert_eq!(windowed.get("sess-c"), None);
        assert_eq!(windowed.get("sess-a"), Some(&3));
    }

    #[test]
    fn top_n_sorts_desc_then_lex() {
        let mut m = BTreeMap::new();
        m.insert("b".into(), 2u64);
        m.insert("a".into(), 2u64);
        m.insert("c".into(), 5u64);
        m.insert("d".into(), 1u64);
        let v = top_n(&m, 2);
        assert_eq!(v, vec![("c".into(), 5), ("a".into(), 2)]);
    }

    #[test]
    fn looks_like_rule_id_filters() {
        assert!(looks_like_rule_id("no-rm-rf"));
        assert!(looks_like_rule_id("work.flow_1"));
        assert!(!looks_like_rule_id(""));
        assert!(!looks_like_rule_id("has space"));
        assert!(!looks_like_rule_id("require_recall: x"));
    }

    #[test]
    fn iso_from_unix_is_zero_padded() {
        assert_eq!(iso_from_unix(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_from_unix(1_776_643_200), "2026-04-20T00:00:00Z");
        // Simple round-trip through `now` to sanity-check the formatter.
        let iso = iso_from_unix(now_seconds());
        assert_eq!(iso.len(), 20);
        assert!(iso.ends_with('Z'));
    }

    #[tokio::test]
    async fn run_empty_root_succeeds_text_and_json() {
        let dir = TempDir::new().unwrap();
        run(dir.path(), 1, false).await.unwrap();
        run(dir.path(), 0, true).await.unwrap();
    }

    #[tokio::test]
    async fn run_with_seeded_data_succeeds() {
        let dir = TempDir::new().unwrap();
        write_gate_log(
            dir.path(),
            &[r#"{"ts":"2026-04-18T10:00:00Z","decision":"block","reason":"no-rm-rf"}"#],
        );
        let now = now_seconds();
        write_violations(dir.path(), &[("sess-a", now)]);
        let mgr = OverrideManager::new(dir.path());
        mgr.request("rule-a", "r", "h1", "s", now).unwrap();
        run(dir.path(), 1, false).await.unwrap();
        run(dir.path(), 0, true).await.unwrap();
    }
}

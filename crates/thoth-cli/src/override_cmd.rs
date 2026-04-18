//! `thoth override {list,approve,reject,stats}` — user-facing CLI for
//! reviewing and acting on agent-filed override requests.
//!
//! Backed by [`thoth_memory::r#override::OverrideManager`]. The manager is
//! rooted at `<cli.root>` (typically `.thoth/`) and handles the filesystem
//! layout under `override-requests/`, `overrides/`, `override-rejected/`.
//!
//! Output is intentionally terse: a compact table for `list`, per-rule
//! counts for `stats`. Pass `--json` on the top-level CLI to emit
//! machine-readable output instead.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use thoth_memory::r#override::{OverrideManager, OverrideRequest, OverrideStatus};

/// Default TTL (turns) applied by `thoth override approve` when the user
/// does not override it. Matches the design-spec default of 1.
const DEFAULT_TTL_TURNS: u32 = 1;

/// List pending override requests.
pub async fn cmd_list(root: &Path, json: bool) -> Result<()> {
    let mgr = OverrideManager::new(root);
    let mut pending = mgr.list_pending().context("list pending overrides")?;
    // Stable output: oldest first.
    pending.sort_by_key(|r| r.requested_at);

    if json {
        println!("{}", serde_json::to_string_pretty(&pending)?);
        return Ok(());
    }

    if pending.is_empty() {
        println!("No pending override requests.");
        return Ok(());
    }

    println!("{:<36}  {:<24}  {:<10}  REASON", "ID", "RULE", "AGE");
    let now = now_seconds();
    for req in pending {
        let age = format_age(now.saturating_sub(req.requested_at));
        let reason = truncate(&req.reason, 60);
        println!(
            "{:<36}  {:<24}  {:<10}  {}",
            req.id,
            truncate(&req.rule_id, 24),
            age,
            reason
        );
    }
    Ok(())
}

/// Approve a pending override request by id.
pub async fn cmd_approve(root: &Path, id: &str, ttl_turns: u32, json: bool) -> Result<()> {
    let mgr = OverrideManager::new(root);
    let approved = mgr
        .approve(id, now_seconds(), ttl_turns)
        .with_context(|| format!("approve override `{id}`"))?;

    if json {
        println!("{}", serde_json::to_string_pretty(&approved)?);
    } else {
        println!(
            "Approved override {} (rule={}, ttl_turns={}).",
            approved.id, approved.rule_id, ttl_turns
        );
    }
    Ok(())
}

/// Reject a pending override request by id.
pub async fn cmd_reject(root: &Path, id: &str, reason: Option<String>, json: bool) -> Result<()> {
    let mgr = OverrideManager::new(root);
    let rejected = mgr
        .reject(id, now_seconds(), reason.clone())
        .with_context(|| format!("reject override `{id}`"))?;

    if json {
        println!("{}", serde_json::to_string_pretty(&rejected)?);
    } else {
        match &reason {
            Some(r) => println!(
                "Rejected override {} (rule={}): {}",
                rejected.id, rejected.rule_id, r
            ),
            None => println!(
                "Rejected override {} (rule={}).",
                rejected.id, rejected.rule_id
            ),
        }
    }
    Ok(())
}

/// Summary statistics: counts per rule across pending, approved, rejected.
///
/// `weeks` filters by `requested_at` within the last `weeks * 7 * 86_400`
/// seconds (`0` disables the filter — count everything on disk).
pub async fn cmd_stats(root: &Path, weeks: u32, json: bool) -> Result<()> {
    let mgr = OverrideManager::new(root);
    let pending = mgr.list_pending().context("list pending")?;
    let approved = mgr.list_approved().context("list approved")?;
    let rejected = mgr.list_rejected().context("list rejected")?;

    let cutoff = if weeks == 0 {
        0
    } else {
        now_seconds().saturating_sub(i64::from(weeks) * 7 * 86_400)
    };
    let in_window = |r: &OverrideRequest| r.requested_at >= cutoff;

    let mut stats: BTreeMap<String, RuleStats> = BTreeMap::new();
    for r in pending.iter().filter(|r| in_window(r)) {
        stats.entry(r.rule_id.clone()).or_default().pending += 1;
    }
    for r in approved.iter().filter(|r| in_window(r)) {
        let entry = stats.entry(r.rule_id.clone()).or_default();
        match r.status {
            OverrideStatus::Consumed { .. } => entry.consumed += 1,
            _ => entry.approved += 1,
        }
    }
    for r in rejected.iter().filter(|r| in_window(r)) {
        stats.entry(r.rule_id.clone()).or_default().rejected += 1;
    }

    if json {
        let payload: Vec<_> = stats
            .iter()
            .map(|(rule, s)| {
                serde_json::json!({
                    "rule_id": rule,
                    "pending": s.pending,
                    "approved": s.approved,
                    "consumed": s.consumed,
                    "rejected": s.rejected,
                    "total": s.total(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "window_weeks": weeks,
                "by_rule": payload,
            }))?
        );
        return Ok(());
    }

    if stats.is_empty() {
        if weeks == 0 {
            println!("No override activity recorded.");
        } else {
            println!("No override activity in the last {weeks} week(s).");
        }
        return Ok(());
    }

    if weeks == 0 {
        println!("Override activity (all-time):");
    } else {
        println!("Override activity (last {weeks} week(s)):");
    }
    println!(
        "  {:<24}  {:>8}  {:>8}  {:>8}  {:>8}  {:>8}",
        "RULE", "PENDING", "APPROVED", "CONSUMED", "REJECTED", "TOTAL"
    );
    let mut totals = RuleStats::default();
    for (rule, s) in &stats {
        println!(
            "  {:<24}  {:>8}  {:>8}  {:>8}  {:>8}  {:>8}",
            truncate(rule, 24),
            s.pending,
            s.approved,
            s.consumed,
            s.rejected,
            s.total(),
        );
        totals.pending += s.pending;
        totals.approved += s.approved;
        totals.consumed += s.consumed;
        totals.rejected += s.rejected;
    }
    println!(
        "  {:<24}  {:>8}  {:>8}  {:>8}  {:>8}  {:>8}",
        "TOTAL",
        totals.pending,
        totals.approved,
        totals.consumed,
        totals.rejected,
        totals.total(),
    );
    Ok(())
}

#[derive(Default, Clone, Copy)]
struct RuleStats {
    pending: u32,
    approved: u32,
    consumed: u32,
    rejected: u32,
}

impl RuleStats {
    fn total(&self) -> u32 {
        self.pending + self.approved + self.consumed + self.rejected
    }
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn format_age(seconds: i64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

/// Public re-export for CLI wiring — `override` is a reserved keyword so
/// `main.rs` refers to this module via the raw identifier `r#override`
/// or we expose this module as `override_cmd`.
pub const DEFAULT_APPROVE_TTL: u32 = DEFAULT_TTL_TURNS;

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use thoth_memory::r#override::OverrideManager;

    fn seed_request(dir: &Path, rule: &str, reason: &str) -> String {
        let m = OverrideManager::new(dir);
        let r = m
            .request(rule, reason, "hash-x", "sess-1", now_seconds())
            .expect("request");
        r.id
    }

    #[tokio::test]
    async fn list_empty_prints_no_pending() {
        let dir = TempDir::new().unwrap();
        // Does not panic; returns Ok on empty state.
        cmd_list(dir.path(), false).await.unwrap();
        cmd_list(dir.path(), true).await.unwrap();
    }

    #[tokio::test]
    async fn approve_then_reject_fails() {
        let dir = TempDir::new().unwrap();
        let id = seed_request(dir.path(), "no-rm-rf", "legit");
        cmd_approve(dir.path(), &id, 1, false).await.unwrap();
        // Reject after approve should error (NotFound in pending).
        let err = cmd_reject(dir.path(), &id, None, false).await;
        assert!(err.is_err(), "expected rejection of already-approved id");
    }

    #[tokio::test]
    async fn reject_with_reason_ok() {
        let dir = TempDir::new().unwrap();
        let id = seed_request(dir.path(), "no-force-push", "merge conflict");
        cmd_reject(dir.path(), &id, Some("unsafe".into()), false)
            .await
            .unwrap();
        let m = OverrideManager::new(dir.path());
        assert!(m.list_rejected().unwrap().iter().any(|r| r.id == id));
    }

    #[tokio::test]
    async fn stats_counts_by_rule() {
        let dir = TempDir::new().unwrap();
        seed_request(dir.path(), "rule-a", "r1");
        seed_request(dir.path(), "rule-a", "r2");
        let idb = seed_request(dir.path(), "rule-b", "r3");
        let m = OverrideManager::new(dir.path());
        m.approve(&idb, now_seconds(), 1).unwrap();
        // Smoke: both output paths succeed.
        cmd_stats(dir.path(), 0, false).await.unwrap();
        cmd_stats(dir.path(), 1, true).await.unwrap();
    }

    #[test]
    fn truncate_handles_multibyte() {
        assert_eq!(truncate("hello", 10), "hello");
        let out = truncate("abcdefghij", 5);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn format_age_buckets() {
        assert_eq!(format_age(10), "10s");
        assert_eq!(format_age(120), "2m");
        assert_eq!(format_age(7_200), "2h");
        assert_eq!(format_age(172_800), "2d");
    }
}

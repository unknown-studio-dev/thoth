//! Integration — `thoth stats` end-to-end.
//!
//! Covers TEST-SPEC `stats_output_all_counters` (REQ-27). Exercises the
//! `thoth` binary via `CARGO_BIN_EXE_thoth`, both in text and `--json`
//! modes, against a fresh repo and a seeded `gate.jsonl`.

use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

fn run_thoth(root: &Path, args: &[&str]) -> Output {
    let bin = env!("CARGO_BIN_EXE_thoth");
    let mut cmd = Command::new(bin);
    cmd.arg("--root").arg(root);
    for a in args {
        cmd.arg(a);
    }
    cmd.output().expect("spawn thoth")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Empty repo → `thoth stats` succeeds with all counters at zero
/// (text mode).
#[test]
fn stats_on_empty_repo_text_reports_zero_counts() {
    let dir = TempDir::new().unwrap();
    let out = run_thoth(dir.path(), &["stats"]);
    assert!(
        out.status.success(),
        "stats failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = stdout(&out);
    // Each counter block must show up with its zero value.
    assert!(text.contains("block  0"), "block counter missing: {text}");
    assert!(text.contains("pass   0"), "pass counter missing: {text}");
    assert!(
        text.contains("pending   0"),
        "pending counter missing: {text}"
    );
    assert!(
        text.contains("Workflow violations (0 total)"),
        "workflow violation total missing: {text}"
    );
}

/// Empty repo → `--json stats` emits a valid payload with every
/// counter at zero.
#[test]
fn stats_on_empty_repo_json_is_all_zeros() {
    let dir = TempDir::new().unwrap();
    let out = run_thoth(dir.path(), &["--json", "stats"]);
    assert!(
        out.status.success(),
        "stats --json failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let payload: serde_json::Value = serde_json::from_str(stdout(&out).trim()).expect("parse JSON");
    assert_eq!(payload["blocks"]["block"], 0);
    assert_eq!(payload["blocks"]["nudge"], 0);
    assert_eq!(payload["blocks"]["pass"], 0);
    assert_eq!(payload["blocks"]["total"], 0);
    assert_eq!(payload["overrides"]["pending"], 0);
    assert_eq!(payload["overrides"]["approved"], 0);
    assert_eq!(payload["overrides"]["consumed"], 0);
    assert_eq!(payload["overrides"]["rejected"], 0);
    assert_eq!(payload["workflow_violations"]["total"], 0);
    assert_eq!(
        payload["repeated_rules"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(99),
        0
    );
}

/// Seeded `gate.jsonl` with a block row is reflected in both the
/// `blocks.block` counter and the `repeated_rules` top-N list.
#[test]
fn stats_reflects_seeded_gate_block() {
    let dir = TempDir::new().unwrap();
    let gate = dir.path().join("gate.jsonl");
    // Use all-time (--weeks 0) so the literal timestamp does not need
    // to fall inside the rolling 1-week window.
    std::fs::write(
        &gate,
        r#"{"ts":"2026-04-18T10:00:00Z","decision":"block","reason":"no-rm-rf"}
{"ts":"2026-04-18T10:01:00Z","decision":"pass","reason":""}
"#,
    )
    .unwrap();

    let out = run_thoth(dir.path(), &["--json", "stats", "--weeks", "0"]);
    assert!(
        out.status.success(),
        "stats failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let payload: serde_json::Value = serde_json::from_str(stdout(&out).trim()).expect("parse JSON");
    assert_eq!(payload["blocks"]["block"], 1, "block count: {payload}");
    assert_eq!(payload["blocks"]["pass"], 1);
    assert_eq!(payload["blocks"]["total"], 2);

    let rules = payload["repeated_rules"].as_array().expect("array");
    assert!(
        rules
            .iter()
            .any(|r| r["rule_id"].as_str() == Some("no-rm-rf") && r["count"] == 1),
        "repeated_rules should list no-rm-rf:1 → {payload}"
    );
}

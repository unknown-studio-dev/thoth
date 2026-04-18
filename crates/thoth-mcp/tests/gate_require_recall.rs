//! Integration tests — T-28 / REQ-09.
//!
//! `RequireRecall` tier: a rule that demands a recent `thoth_recall`
//! entry in `.thoth/gate.jsonl` matching the rule's `natural` trigger
//! before a matching tool call proceeds.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

fn run_gate(root: &Path, home: &Path, input: &Value) -> (Value, String) {
    let bin = env!("CARGO_BIN_EXE_thoth-gate");
    let mut child = Command::new(bin)
        .env("THOTH_ROOT", root)
        .env("HOME", home)
        .env_remove("THOTH_ACTOR")
        .env_remove("THOTH_DEFER_REFLECT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn thoth-gate");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("wait thoth-gate");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let verdict: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("non-JSON stdout {stdout:?}: {e}"));
    (verdict, stderr)
}

/// Seed `.thoth/` with a project rule demanding a recall for edits to
/// `retriever.rs`, and disable reflection-debt + telemetry so nothing
/// else interferes.
fn seed_root_with_require_recall(root: &Path) {
    std::fs::write(
        root.join("config.toml"),
        r#"
[discipline]
mode = "nudge"
reflect_debt_block = 0
telemetry_enabled = false
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("rules.project.toml"),
        r#"
[rules.needs-recall]
tool = "Edit"
path_glob = "**/retriever.rs"
natural = "retriever"
enforcement = { RequireRecall = { recall_within_turns = 5 } }
"#,
    )
    .unwrap();
}

/// REQ-09: `RequireRecall` blocks when `gate.jsonl` has no matching
/// recall event (empty file in this case).
#[test]
fn blocks_empty() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    seed_root_with_require_recall(root.path());
    // Explicit empty gate.jsonl → definitely no matching recall.
    std::fs::write(root.path().join("gate.jsonl"), "").unwrap();

    let input = json!({
        "tool_name": "Edit",
        "tool_input": {
            "file_path": "src/retriever.rs",
            "old_string": "a",
            "new_string": "b"
        }
    });
    let (verdict, _stderr) = run_gate(root.path(), home.path(), &input);
    assert_eq!(
        verdict.get("decision").and_then(Value::as_str),
        Some("block"),
        "expected block, got: {verdict}"
    );
    let reason = verdict
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        reason.contains("thoth_recall"),
        "block reason should point the agent at thoth_recall: {reason}"
    );
}

/// REQ-09: `RequireRecall` passes when `gate.jsonl` contains a recent
/// `thoth_recall` event whose `query` field matches the rule's
/// `natural` trigger.
#[test]
fn passes_with_recent() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    seed_root_with_require_recall(root.path());
    // One recall event mentioning the rule's trigger word.
    std::fs::write(
        root.path().join("gate.jsonl"),
        "{\"tool\":\"thoth_recall\",\"query\":\"retriever updates\"}\n",
    )
    .unwrap();

    let input = json!({
        "tool_name": "Edit",
        "tool_input": {
            "file_path": "src/retriever.rs",
            "old_string": "a",
            "new_string": "b"
        }
    });
    let (verdict, _stderr) = run_gate(root.path(), home.path(), &input);
    // RequireRecall satisfied → dispatch returns None → gate falls
    // through to the recall-relevance engine. With an empty episodes DB
    // and `nudge` mode, that path approves (optionally with a nudge
    // reason). Either way, the decision must NOT be `block`.
    assert_ne!(
        verdict.get("decision").and_then(Value::as_str),
        Some("block"),
        "RequireRecall satisfied must not block: {verdict}"
    );
    assert_eq!(
        verdict.get("decision").and_then(Value::as_str),
        Some("approve"),
        "expected approve, got: {verdict}"
    );
}

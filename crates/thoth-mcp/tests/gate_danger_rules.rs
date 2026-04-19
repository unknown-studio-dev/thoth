//! Integration tests — T-28 / REQ-07, REQ-08.
//!
//! Drive the `thoth-gate` binary end-to-end against the shipped default
//! danger rules (`rules.default.toml`). We don't try to assert on exit
//! codes — the binary intentionally exits 0 and encodes the decision in
//! the stdout JSON — so these tests parse stdout instead.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

/// Spawn `thoth-gate` with an isolated `$HOME` and `$THOTH_ROOT`, feed
/// it the given hook envelope JSON on stdin, and return
/// `(stdout_json, stderr_string)`.
fn run_gate(root: &Path, home: &Path, input: &Value) -> (Value, String) {
    let bin = env!("CARGO_BIN_EXE_thoth-gate");
    let mut child = Command::new(bin)
        .env("THOTH_ROOT", root)
        .env("HOME", home)
        // Clear any ambient actor / defer-reflect so the test starts
        // from a clean decision surface.
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
    // Gate writes exactly one JSON line to stdout.
    let verdict: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("non-JSON stdout {stdout:?}: {e}"));
    (verdict, stderr)
}

/// Write a minimal `.thoth/config.toml` so reflection-debt doesn't fire
/// on a pristine tempdir (no `gate.jsonl` → debt=0 → no block, but we
/// also want telemetry OFF to keep the test hermetic).
fn seed_root(root: &Path) {
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
}

fn tool_call_hash(input: &Value) -> String {
    let canonical = serde_json::to_vec(input).unwrap_or_default();
    blake3::hash(&canonical).to_hex().to_string()
}

/// REQ-07 / REQ-08: the shipped `no-rm-rf` default rule must Block a
/// `Bash` tool call whose command matches `rm -rf` on an absolute path.
#[test]
fn rm_rf_blocked() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    seed_root(root.path());

    let input = json!({
        "tool_name": "Bash",
        "tool_input": { "command": "rm -rf /tmp/foo" }
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
        reason.contains("rm -rf"),
        "block reason should name the forbidden command: {reason}"
    );
}

/// REQ-08 + REQ-16: a `Block` rule with a pre-approved override for the
/// *exact* tool-call hash passes through with `override_consumed` in
/// the approve reason, and the override is single-use — the second call
/// blocks again.
#[test]
fn rm_rf_with_approved_override_passes_then_blocks() {
    use thoth_memory::r#override::OverrideManager;

    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    seed_root(root.path());

    let input = json!({
        "tool_name": "Bash",
        "tool_input": { "command": "rm -rf /tmp/foo" }
    });

    // Pre-approve an override keyed on (rule_id, hash). ttl_turns=1 so
    // the second invocation with the same hash must block again.
    let mgr = OverrideManager::new(root.path());
    let hash = tool_call_hash(&input);
    let req = mgr
        .request("no-rm-rf", "integration test cleanup", &hash, "sess-it", 1)
        .unwrap();
    mgr.approve(&req.id, 2, 1).unwrap();

    let (v1, _) = run_gate(root.path(), home.path(), &input);
    assert_eq!(
        v1.get("decision").and_then(Value::as_str),
        Some("approve"),
        "first call should pass with override: {v1}"
    );
    assert!(
        v1.get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .contains("override_consumed"),
        "first call reason should say override_consumed: {v1}"
    );

    let (v2, _) = run_gate(root.path(), home.path(), &input);
    assert_eq!(
        v2.get("decision").and_then(Value::as_str),
        Some("block"),
        "second call must block — override was single-use: {v2}"
    );
}

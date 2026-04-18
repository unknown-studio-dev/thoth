//! Integration tests — T-28 / REQ-10.
//!
//! `Require` tier inject. The gate v2 design keeps the actual text
//! injection *out-of-band* in the `SessionStart` / `UserPromptSubmit`
//! hook surfaces. Inside the PreToolUse gate itself, a `Require` rule
//! match must fall through to the recall-relevance engine (i.e. not
//! short-circuit the call) so the injected lesson can exert influence
//! without double-gating the edit.
//!
//! This test locks that contract in so future inject work doesn't
//! silently regress the pass-through path.

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

fn seed_root_with_require_rule(root: &Path) {
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
[rules.require-md-reformat]
enforcement = "Require"
tool = "Edit"
path_glob = "**/*.md"
natural = "reformat before commit"
message = "reformat before commit"
"#,
    )
    .unwrap();
}

/// REQ-10 (PreToolUse slice): a `Require` rule must NOT short-circuit
/// the gate. It falls through to the recall-relevance engine, which in
/// nudge mode approves the edit. The downstream `SessionStart` /
/// `UserPromptSubmit` hook is what surfaces the inject content.
#[test]
fn require_rule_falls_through_to_approve() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    seed_root_with_require_rule(root.path());

    let input = json!({
        "tool_name": "Edit",
        "tool_input": {
            "file_path": "docs/readme.md",
            "old_string": "a",
            "new_string": "b"
        }
    });
    let (verdict, _stderr) = run_gate(root.path(), home.path(), &input);
    assert_eq!(
        verdict.get("decision").and_then(Value::as_str),
        Some("approve"),
        "Require rules must fall through to approve inside the gate: {verdict}"
    );
    // Confirm the rule's message is not smuggled through as a block
    // reason — inject is a separate surface.
    let reason = verdict
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        !reason.contains("reformat before commit")
            || !matches!(
                verdict.get("decision").and_then(Value::as_str),
                Some("block")
            ),
        "Require message must not surface as a block reason: {verdict}"
    );
}

//! `thoth-gate` — PreToolUse hook for the thoth Claude Code plugin.
//!
//! Gate v2 design (see `design/gate-v2.md` in conversation history). Three
//! factors decide whether a mutation tool call proceeds:
//!
//! 1. **Intent** — read-only Bash (cargo test / git status / grep / …) is
//!    whitelisted and skips the gate entirely.
//! 2. **Recency** — if a `query_issued` event fired within
//!    `window_short_secs`, that alone is enough. This preserves the old
//!    "just called recall, let me edit" flow.
//! 3. **Relevance** — past that short window, the gate scores token
//!    overlap between the edit context and recent recall queries (up to
//!    `window_long_secs` back). Ritual recalls (`recall("x")` to reset
//!    the clock) score 0.0 and fail; real topical recalls score high
//!    and pass.
//!
//! Verdict is driven by the resolved **policy**, which is the first
//! `[[discipline.policies]]` entry whose `actor` glob matches the
//! `THOTH_ACTOR` env var. Default policy applies on no match.
//!
//! The gate always **fails open** on internal errors (bad config, IO,
//! SQLite) — a broken gate must never brick the editor. Errors go to
//! stderr; the verdict is `approve`.
//!
//! ## Output shape
//!
//! stdout is exactly one line of JSON:
//!
//! ```json
//! {"decision": "approve"}
//! {"decision": "approve", "reason": "..."}   // nudge
//! {"decision": "block",   "reason": "..."}   // strict + miss
//! ```
//!
//! stderr holds the human-readable explanation (edit tokens, recent
//! recalls with overlap scores, suggested recall query) — Claude Code
//! surfaces this to the agent so it can self-correct.

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use thoth_memory::{DisciplineConfig, gate_defaults};

// ===========================================================================
// Defaults — surface for user-facing docs.
// ===========================================================================

/// Legacy: `soft` / `strict`. New: `off` / `nudge` / `relevance` / `strict`.
/// Default picked by policy:
///
/// - `nudge` — pass every call; emit stderr warning on relevance miss.
///   Chosen as default because a noisy false block is worse than a silent
///   miss. Opt-in to stricter mode via config.
const DEFAULT_MODE: &str = "nudge";

/// Recency shortcut — recall within this window passes without a relevance
/// check. Kept short because recency alone is the weakest signal.
const DEFAULT_WINDOW_SHORT_SECS: u64 = gate_defaults::WINDOW_SHORT_SECS;

/// Relevance pool — how far back we search for a topically-matching recall.
/// 30 min covers a typical coding session; older recalls are stale context
/// that probably doesn't justify the current edit anyway.
const DEFAULT_WINDOW_LONG_SECS: u64 = gate_defaults::WINDOW_LONG_SECS;

/// Containment ratio threshold for relevance. See the `threshold` comment
/// block on `GateConfig::relevance_threshold` for the user-facing range
/// guidance.
const DEFAULT_RELEVANCE_THRESHOLD: f64 = gate_defaults::RELEVANCE_THRESHOLD;

/// Default read-only Bash prefixes — expanded from the most common
/// developer loops. Anything that only *observes* the system (no writes,
/// no network side effects) is a safe pass.
const DEFAULT_BASH_READONLY_PREFIXES: &[&str] = &[
    "cargo build",
    "cargo test",
    "cargo check",
    "cargo clippy",
    "cargo fmt --check",
    "cargo doc",
    "cargo tree",
    "git status",
    "git diff",
    "git log",
    "git show",
    "git branch",
    "git remote -v",
    "git stash list",
    "grep ",
    "rg ",
    "ls ",
    "ls\n",
    "find ",
    "tree ",
    "echo ",
    "cat ",
    "head ",
    "tail ",
    "wc ",
    "which ",
    "pwd",
    "env",
    "printenv",
    "npx tsc --noEmit",
    "pnpm test",
    "pnpm lint",
    "pnpm tsc",
    "npm test",
    "npm run lint",
    "yarn test",
    "yarn lint",
    "go test",
    "go build",
    "go vet",
    "python -m pytest",
    "pytest",
    "mypy",
    "ruff check",
    // Thoth's own read-only / recovery commands. Omitting these
    // deadlocks the reflection-debt enforcement: at high debt the
    // agent can't run `thoth curate` to see why, `thoth memory
    // show|pending` to inspect, or — critically — `thoth memory
    // fact|lesson|promote|reject` to resolve the block. Anything
    // listed here is either pure read/audit or a deliberate
    // memory-curation action the user would explicitly want
    // unblocked during a debt lockdown.
    //
    // Deliberately NOT whitelisted: `thoth setup`, `thoth index`,
    // `thoth watch`, `thoth uninstall`, `thoth skills install` —
    // these are real mutations (write DBs, settings.json, skill
    // directories) and shouldn't bypass discipline.
    "thoth curate",
    "thoth compact",
    "thoth review",
    "thoth query ",
    "thoth impact ",
    "thoth context ",
    "thoth changes",
    "thoth memory show",
    "thoth memory edit",
    "thoth memory pending",
    "thoth memory log",
    "thoth memory promote ",
    "thoth memory reject ",
    "thoth memory forget",
    "thoth memory fact ",
    "thoth memory lesson ",
    // REQ-09 / REQ-11: the migrate verb + the edit verbs it calls under
    // the hood. These are audit-logged memory mutations (not code edits),
    // so they're classified ReadOnly-for-gate-purposes — the gate exists
    // to force a `recall` before *code* mutation, not before curation.
    "thoth memory migrate",
    "thoth memory replace ",
    "thoth memory remove ",
    "thoth memory preference ",
    "thoth skills list",
    "thoth eval ",
];

/// MCP tool names that the gate treats as curation — memory mutations
/// that explicitly don't require a fresh `recall`. The MCP branch of
/// `classify_intent` looks these up so the agent can call them the same
/// way it can use `thoth_recall` / `thoth_remember_fact` under a debt
/// lockdown.
///
/// Source: DESIGN-SPEC REQ-11.
const DEFAULT_MCP_TOOL_READONLY: &[&str] = &[
    "thoth_memory_replace",
    "thoth_memory_remove",
    "thoth_remember_preference",
];

// ===========================================================================
// Entry point
// ===========================================================================

fn main() -> ExitCode {
    // Drain stdin; Claude Code pipes the full tool-call JSON here. We need
    // it for edit-context extraction — unlike v1 which just dropped it.
    let mut buf = String::new();
    let _ = io::stdin().read_to_string(&mut buf);
    let input_json: Value = serde_json::from_str(&buf).unwrap_or(Value::Null);

    let (verdict, stderr_msg, telemetry) = run(&input_json);

    // stderr first (agent-visible explanation), then stdout verdict.
    if let Some(msg) = stderr_msg {
        eprintln!("{msg}");
    }
    println!("{verdict}");

    // Best-effort telemetry append — never let logging failure leak to the
    // verdict or the editor.
    if let Some(rec) = telemetry {
        let _ = append_telemetry(&rec);
    }

    // Exit code is always success; verdict semantics live in the JSON.
    // Claude Code keys off stdout, not our exit code.
    ExitCode::SUCCESS
}

// ===========================================================================
// Top-level flow
// ===========================================================================

/// Run the decision engine. Returns:
/// - JSON verdict for stdout.
/// - Optional stderr message (present when we decided nudge or block).
/// - Optional telemetry record to append (present when telemetry enabled
///   and a decision was actually made — skips the trivial disabled path).
fn run(input: &Value) -> (Value, Option<String>, Option<TelemetryRecord>) {
    // Resolve the .thoth root up front.
    let bootstrap = first_existing_root(true);
    let cfg = match bootstrap.as_deref() {
        Some(p) => load_config(p),
        None => GateConfig::default(),
    };

    // Nudge-before-write off = legacy "discipline disabled" kill switch.
    if !cfg.nudge_before_write {
        return (approve_json(None), None, None);
    }

    let root = match first_existing_root(cfg.global_fallback) {
        Some(p) => p,
        None => {
            return (
                approve_json(Some(
                    "[thoth-gate] no .thoth/ directory found; discipline disabled. \
                     Run `thoth setup` to enable.",
                )),
                None,
                None,
            );
        }
    };

    // Re-read config at the resolved root in case project-local differs
    // from the ~/.thoth bootstrap location.
    let cfg = load_config(&root);

    // Parse the tool-call envelope. Claude Code uses
    //   { "tool_name": "...", "tool_input": { ... } }
    let tool_name = input
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let tool_input = input.get("tool_input").cloned().unwrap_or(Value::Null);

    // Intent classification — read-only ops skip everything.
    let intent = classify_intent(&tool_name, &tool_input, &cfg.bash_readonly_prefixes);
    if matches!(intent, Intent::ReadOnly | Intent::Ignored) {
        let telemetry = cfg.telemetry_enabled.then(|| TelemetryRecord {
            root: root.clone(),
            ts_iso: now_iso(),
            actor: resolve_actor(),
            tool: tool_name.clone(),
            path: tool_input_path(&tool_input),
            decision: "pass".to_string(),
            reason: "readonly_whitelist".to_string(),
            recency_secs: None,
            best_score: None,
            considered: 0,
            missed_tokens: Vec::new(),
        });
        return (approve_json(None), None, telemetry);
    }

    // Resolve actor policy. Actor string is whatever the caller set in
    // `THOTH_ACTOR`; missing → "default" → default policy.
    let actor = resolve_actor();
    let policy = cfg.resolve_policy(&actor);

    // Reflection-debt block. Independent of the recall-relevance
    // decision below — runs even in nudge mode, because reflection is
    // a separate contract (persist what you learn) from grounding
    // (consult what was learned). Only fires for mutations (the
    // `Intent::Mutation` arm we're already in) and only when
    // `discipline.reflect_debt_block` is non-zero.
    //
    // `THOTH_DEFER_REFLECT=1` is the explicit bypass — matches the
    // memory-discipline skill's wording so the stderr message doubles
    // as documentation.
    if policy.mode != PolicyMode::Off {
        let disc = thoth_memory::DisciplineConfig::load_or_default_sync(&root);
        let debt = thoth_memory::ReflectionDebt::compute_sync(&root);
        // Bypass paths for the debt block. Any one is enough.
        //
        // 1. `THOTH_DEFER_REFLECT=1` — the documented session-wide
        //    escape hatch. Requires restarting Claude Code to set
        //    (env is snapshot at session start), so it's coarse but
        //    reliable.
        //
        // 2. The mutation targets `<root>/config.toml` or a `.bak-*`
        //    file inside `.thoth/`. Without this carve-out the gate
        //    deadlocks: you can't raise `reflect_debt_block` or roll
        //    back a compact backup because the very edit that would
        //    fix the lockup is itself blocked. Config tuning and
        //    rollback are recovery operations and must stay reachable
        //    at any debt level.
        //
        // 3. A fresh `.thoth/.reflect-defer` marker file (mtime
        //    within `DEFER_MARKER_TTL_SECS`). Creating this file is an
        //    in-session escape hatch that doesn't require restarting
        //    Claude Code — the MCP daemon's `thoth_defer_reflect` tool
        //    writes it, and MCP tool calls don't route through the
        //    gate (this hook only sees Write/Edit/Bash/NotebookEdit),
        //    so it works even when every mutation is blocked.
        let bypass = std::env::var("THOTH_DEFER_REFLECT").is_ok_and(|v| v == "1" || v == "true")
            || recovery_path(&tool_input, &root)
            || defer_marker_fresh(&root);
        if !bypass && debt.should_block(&disc) {
            let msg = format!(
                "Thoth discipline: reflection debt {} ≥ block threshold {} \
                 ({} mutation(s), {} remember(s) this session). Call \
                 `thoth_remember_fact` / `thoth_remember_lesson` for anything \
                 durable from the recent edits, OR call the MCP tool \
                 `thoth_defer_reflect` to create a 30-min bypass marker, \
                 OR set `THOTH_DEFER_REFLECT=1` (requires restart). Edits to \
                 `.thoth/config.toml` and `.thoth/*.bak-*` always pass.",
                debt.debt(),
                disc.reflect_debt_block,
                debt.mutations,
                debt.remembers,
            );
            let telemetry = cfg.telemetry_enabled.then(|| TelemetryRecord {
                root: root.clone(),
                ts_iso: now_iso(),
                actor: actor.clone(),
                tool: tool_name.clone(),
                path: tool_input_path(&tool_input),
                decision: "block".to_string(),
                reason: "reflection_debt".to_string(),
                recency_secs: None,
                best_score: None,
                considered: 0,
                missed_tokens: Vec::new(),
            });
            return (block_json(&msg), Some(msg), telemetry);
        }
    }

    if policy.mode == PolicyMode::Off {
        let telemetry = cfg.telemetry_enabled.then(|| TelemetryRecord {
            root: root.clone(),
            ts_iso: now_iso(),
            actor: actor.clone(),
            tool: tool_name.clone(),
            path: tool_input_path(&tool_input),
            decision: "pass".to_string(),
            reason: "policy_off".to_string(),
            recency_secs: None,
            best_score: None,
            considered: 0,
            missed_tokens: Vec::new(),
        });
        return (approve_json(None), None, telemetry);
    }

    // Pull the recall pool — up to ~20 rows, within window_long.
    let recalls = match recent_recalls(&root, policy.window_long_secs) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[thoth-gate] sqlite error: {e}");
            return (approve_json(None), None, None); // fail open
        }
    };

    // Extract edit context tokens from the tool input.
    let edit_tokens = extract_edit_tokens(&tool_name, &tool_input);

    let decision = decide(&policy, &recalls, &edit_tokens, now_unix_ns());

    // Render verdict + stderr message.
    let stderr = format_stderr(
        &decision,
        &tool_name,
        &tool_input,
        &recalls,
        &edit_tokens,
        &policy,
    );

    let verdict = match decision.verdict {
        Verdict::Pass => approve_json(None),
        Verdict::Nudge => approve_json(Some(&stderr)),
        Verdict::Block => block_json(&stderr),
    };

    let telemetry = cfg.telemetry_enabled.then(|| TelemetryRecord {
        root: root.clone(),
        ts_iso: now_iso(),
        actor,
        tool: tool_name,
        path: tool_input_path(&tool_input),
        decision: match decision.verdict {
            Verdict::Pass => "pass",
            Verdict::Nudge => "nudge",
            Verdict::Block => "block",
        }
        .to_string(),
        reason: decision.reason.to_string(),
        recency_secs: decision.recency_secs,
        best_score: decision.best_score,
        considered: decision.considered,
        missed_tokens: decision.missed_tokens,
    });

    let stderr_out = match decision.verdict {
        Verdict::Pass => None,
        _ => Some(stderr),
    };

    (verdict, stderr_out, telemetry)
}

// ===========================================================================
// Intent classification
// ===========================================================================

/// Coarse classification of a tool call into gate-relevant buckets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Intent {
    /// Mutation — Edit, Write, NotebookEdit, Bash that writes.
    Mutation,
    /// Bash command matching the read-only prefix whitelist. Pass silent.
    ReadOnly,
    /// Tool the gate doesn't care about (Read, Grep, Glob, WebFetch, …).
    /// Pass silent; the gate exists to check memory-before-mutation, not
    /// memory-before-observation.
    Ignored,
}

fn classify_intent(tool_name: &str, tool_input: &Value, readonly_prefixes: &[String]) -> Intent {
    match tool_name {
        "Edit" | "Write" | "NotebookEdit" => Intent::Mutation,
        "Bash" => {
            let cmd = tool_input
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("");
            // Trim leading whitespace; allow shell-prefixed forms like
            // `set -e; cargo test`. Match only on the first token/line so
            // `cat foo; rm -rf bar` doesn't get whitelisted.
            let head = cmd.trim_start();
            let first_line = head.lines().next().unwrap_or("").trim();
            let matches_readonly = readonly_prefixes
                .iter()
                .any(|p| first_line == p.trim_end() || first_line.starts_with(p.as_str()));
            if matches_readonly {
                Intent::ReadOnly
            } else {
                Intent::Mutation
            }
        }
        // MCP tool dispatches — Claude Code prefixes these with
        // `mcp__<server>__`. REQ-11: the curation verbs map to ReadOnly so
        // the agent can clean up memory under a debt lockdown.
        name if name.starts_with("mcp__") => {
            let tail = name.rsplit("__").next().unwrap_or(name);
            if DEFAULT_MCP_TOOL_READONLY.contains(&tail) {
                Intent::ReadOnly
            } else {
                Intent::Ignored
            }
        }
        // Anything else — Read, Grep, Glob, WebFetch, plain MCP tools
        // we don't care about. Not our concern.
        _ => Intent::Ignored,
    }
}

fn tool_input_path(tool_input: &Value) -> Option<String> {
    tool_input
        .get("file_path")
        .or_else(|| tool_input.get("notebook_path"))
        .and_then(Value::as_str)
        .map(String::from)
}

/// TTL for the defer marker. 30 minutes is long enough that a human
/// can clear a debt lockdown without being rushed, short enough that a
/// stale marker from a previous session won't silently disable the
/// gate for the next session. The marker file is created by the MCP
/// daemon's `thoth_defer_reflect` tool.
const DEFER_MARKER_TTL_SECS: u64 = 1800;

/// Returns `true` when the mutation targets a recovery-safe path
/// inside `<root>/`. These are the edits the user MUST be able to make
/// even when every other mutation is blocked:
///
/// - `<root>/config.toml` — tune the thresholds that caused the block.
/// - `<root>/MEMORY.md.bak-<unix>` / `<root>/LESSONS.md.bak-<unix>` —
///   roll back a bad `thoth compact` output.
fn recovery_path(tool_input: &Value, root: &std::path::Path) -> bool {
    let Some(raw) = tool_input_path(tool_input) else {
        return false;
    };
    let path = std::path::Path::new(&raw);
    // Match either absolute path under root or relative path that
    // resolves to one. We don't canonicalise (file might not exist on
    // Write), just compare the components we care about.
    let config = root.join("config.toml");
    if path == config {
        return true;
    }
    // .bak-<digits> suffixes on MEMORY.md or LESSONS.md anywhere inside root.
    if let Some(name) = path.file_name().and_then(|s| s.to_str())
        && (name.starts_with("MEMORY.md.bak-") || name.starts_with("LESSONS.md.bak-"))
        && path.starts_with(root)
    {
        return true;
    }
    false
}

/// Returns `true` when `<root>/.reflect-defer` exists and its mtime is
/// within [`DEFER_MARKER_TTL_SECS`] of now. Any I/O error is treated
/// as "no defer" so a missing or unreadable marker fails closed (gate
/// stays active).
fn defer_marker_fresh(root: &std::path::Path) -> bool {
    let marker = root.join(".reflect-defer");
    let Ok(meta) = std::fs::metadata(&marker) else {
        return false;
    };
    let Ok(mtime) = meta.modified() else {
        return false;
    };
    let Ok(age) = std::time::SystemTime::now().duration_since(mtime) else {
        // Future mtime — treat as fresh (clock skew, don't penalise).
        return true;
    };
    age.as_secs() < DEFER_MARKER_TTL_SECS
}

// ===========================================================================
// Tokenizer
// ===========================================================================

/// Generic keyword/stopword set — dropped from token sets on both sides
/// (edit context AND recall query) so overlap scores aren't dominated by
/// e.g. `fn`, `let`, `const`, `the`, `and`.
const STOPWORDS: &[&str] = &[
    // English stopwords
    "the",
    "and",
    "for",
    "a",
    "to",
    "of",
    "in",
    "is",
    "it",
    "as",
    "or",
    "be",
    "on",
    "at",
    "this",
    "that",
    "an",
    "with",
    "from",
    "by",
    "but",
    "if",
    "are",
    "was",
    "were",
    "we",
    "you",
    "i",
    "they",
    "he",
    "she",
    "them",
    "his",
    "her",
    "its",
    "our",
    "my",
    "your",
    "their",
    "all",
    "any",
    "some",
    "no",
    "not",
    "so",
    "do",
    "does",
    "did",
    "has",
    "have",
    "had",
    "will",
    "would",
    "could",
    "should",
    "may",
    "might",
    "can",
    "just",
    "only",
    // Code keywords (multi-language intersection)
    "fn",
    "let",
    "const",
    "var",
    "if",
    "else",
    "return",
    "use",
    "mod",
    "struct",
    "impl",
    "pub",
    "def",
    "class",
    "from",
    "import",
    "async",
    "await",
    "while",
    "loop",
    "match",
    "break",
    "continue",
    "true",
    "false",
    "null",
    "none",
    "self",
    "this",
    "new",
    "type",
    "enum",
    "trait",
    "interface",
    "public",
    "private",
    "protected",
    "static",
    "final",
    "abstract",
    "extends",
    "implements",
    "func",
    "package",
    "module",
];

/// Tokenize an arbitrary text blob into a lowercased identifier set.
///
/// Rules:
/// - lowercase
/// - split on any non-`[a-zA-Z0-9_]` character
/// - drop stopwords + single-char tokens + pure digits
/// - for a CamelCase identifier, emit *both* the lowercase joined form
///   *and* each lowercased segment (so `FooBar` → {`foobar`, `foo`, `bar`})
/// - snake_case preserved as a single token (`my_field` stays `my_field`)
fn tokenize(text: &str) -> HashSet<String> {
    let mut out: HashSet<String> = HashSet::new();
    // Split on any non-identifier character.
    for raw in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
        if raw.is_empty() {
            continue;
        }
        // Insert the raw identifier (lowercased). Snake_case stays intact.
        let lower = raw.to_ascii_lowercase();
        insert_token(&mut out, &lower);

        // CamelCase decomposition. We keep the joined lowercase form and
        // split segments when the original has transitions A→a or a→A
        // separated by uppercase. `FooBar` → ["foo","bar"]; `HTTPSServer`
        // → ["https","server"] (consecutive caps absorbed, then lowercase
        // starts a new segment).
        if has_camel_transition(raw) {
            for seg in split_camel(raw) {
                insert_token(&mut out, &seg.to_ascii_lowercase());
            }
        }
    }
    out
}

fn insert_token(out: &mut HashSet<String>, tok: &str) {
    if tok.len() <= 1 {
        return;
    }
    if tok.chars().all(|c| c.is_ascii_digit()) {
        return;
    }
    if STOPWORDS.contains(&tok) {
        return;
    }
    out.insert(tok.to_string());
}

fn has_camel_transition(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    chars
        .windows(2)
        .any(|w| w[0].is_ascii_lowercase() && w[1].is_ascii_uppercase())
}

/// Split a CamelCase identifier into segments. Consecutive uppercase runs
/// are treated as one segment until a lowercase letter starts the next
/// segment — `HTTPSServer` → ["HTTPS", "Server"].
fn split_camel(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = s.chars().collect();
    for i in 0..chars.len() {
        let c = chars[i];
        if !cur.is_empty() {
            let prev = chars[i - 1];
            let next = chars.get(i + 1).copied();
            let boundary = (prev.is_ascii_lowercase() && c.is_ascii_uppercase())
                || (prev.is_ascii_uppercase()
                    && c.is_ascii_uppercase()
                    && next.is_some_and(|n| n.is_ascii_lowercase()));
            if boundary {
                out.push(std::mem::take(&mut cur));
            }
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

// ===========================================================================
// Edit context extraction
// ===========================================================================

/// Cap on the number of tokens we carry forward from a single edit. Large
/// diffs would otherwise drown out meaningful overlap — with 2000 tokens
/// on one side and a 5-token recall query, containment math still works
/// but topic drift gets easier to hide.
const EDIT_TOKEN_CAP: usize = 200;

fn extract_edit_tokens(tool_name: &str, tool_input: &Value) -> HashSet<String> {
    let mut acc = HashSet::new();
    match tool_name {
        "Edit" => {
            if let Some(p) = tool_input.get("file_path").and_then(Value::as_str) {
                acc.extend(tokenize(&basename_stem(p)));
            }
            if let Some(s) = tool_input.get("old_string").and_then(Value::as_str) {
                acc.extend(tokenize(s));
            }
            if let Some(s) = tool_input.get("new_string").and_then(Value::as_str) {
                acc.extend(tokenize(s));
            }
        }
        "Write" => {
            if let Some(p) = tool_input.get("file_path").and_then(Value::as_str) {
                acc.extend(tokenize(&basename_stem(p)));
            }
            if let Some(s) = tool_input.get("content").and_then(Value::as_str) {
                // Cap content at 2KB for tokenization — beyond that point
                // new tokens stop influencing score much.
                let slice = &s[..s.len().min(2048)];
                acc.extend(tokenize(slice));
            }
        }
        "NotebookEdit" => {
            if let Some(p) = tool_input.get("notebook_path").and_then(Value::as_str) {
                acc.extend(tokenize(&basename_stem(p)));
            }
            if let Some(s) = tool_input.get("new_source").and_then(Value::as_str) {
                acc.extend(tokenize(s));
            }
        }
        "Bash" => {
            if let Some(s) = tool_input.get("command").and_then(Value::as_str) {
                acc.extend(tokenize(s));
            }
        }
        _ => {}
    }
    // Cap — drop extras deterministically by sort order so tests stay stable.
    if acc.len() > EDIT_TOKEN_CAP {
        let mut v: Vec<String> = acc.into_iter().collect();
        v.sort();
        v.truncate(EDIT_TOKEN_CAP);
        acc = v.into_iter().collect();
    }
    acc
}

/// Strip directories and extension from a path. `foo/bar/retriever.rs` →
/// `retriever`. Used so the file name contributes identifier tokens to
/// the edit context without path noise (we don't want `crates`, `src`,
/// etc. to dominate overlap).
fn basename_stem(path: &str) -> String {
    let p = Path::new(path);
    p.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string()
}

// ===========================================================================
// Episodes DB — recent recall queries
// ===========================================================================

/// One recall event pulled from `episodes.db`.
#[derive(Debug, Clone)]
struct RecallRow {
    ts_ns: u64,
    text: String,
}

impl RecallRow {
    fn age_secs(&self, now_ns: u64) -> u64 {
        now_ns.saturating_sub(self.ts_ns) / 1_000_000_000
    }
}

/// Hard cap on rows returned — caller (decide) iterates them so a rogue
/// giant pool doesn't blow the p95 latency budget.
const RECALL_POOL_CAP: usize = 20;

fn recent_recalls(root: &Path, window_long_secs: u64) -> rusqlite::Result<Vec<RecallRow>> {
    let db = root.join("episodes.db");
    if !db.is_file() {
        return Ok(Vec::new());
    }
    let conn = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let cutoff_ns: i64 =
        (now_unix_ns().saturating_sub(window_long_secs.saturating_mul(1_000_000_000))) as i64;
    let mut stmt = conn.prepare(
        "SELECT at_unix_ns, payload FROM episodes \
         WHERE kind = 'query_issued' AND at_unix_ns >= ?1 \
         ORDER BY id DESC LIMIT ?2",
    )?;
    let mut rows = stmt.query(rusqlite::params![cutoff_ns, RECALL_POOL_CAP as i64])?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        let ns: i64 = r.get(0)?;
        let payload: String = r.get(1)?;
        let text = extract_query_text(&payload);
        out.push(RecallRow {
            ts_ns: ns.max(0) as u64,
            text,
        });
    }
    Ok(out)
}

/// Pull the human-readable recall query out of an `Event::QueryIssued`
/// payload. The event shape is `{"kind":"query_issued","id":..,"text":"..","at":".."}`
/// when serialized by thoth-core — but be defensive: fall back to the
/// whole payload if the field's missing.
fn extract_query_text(payload: &str) -> String {
    let v: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
    v.get("text")
        .and_then(Value::as_str)
        .unwrap_or(payload)
        .to_string()
}

// ===========================================================================
// Decision engine
// ===========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Pass,
    Nudge,
    Block,
}

#[derive(Debug, Clone)]
struct Decision {
    verdict: Verdict,
    /// Short machine-readable reason: `recency`, `relevance`, `miss`,
    /// `cold_start`, `off`. Used by telemetry.
    reason: &'static str,
    /// Age of the most recent recall, if any.
    recency_secs: Option<u64>,
    /// Highest containment score over the recall pool (0.0 if pool empty).
    best_score: Option<f64>,
    /// Number of recalls considered in scoring.
    considered: usize,
    /// Top-N edit tokens not covered by any recall query — surfaces in
    /// the stderr message so the agent can craft a targeted recall.
    missed_tokens: Vec<String>,
}

fn decide(
    policy: &Policy,
    recalls: &[RecallRow],
    edit_tokens: &HashSet<String>,
    now_ns: u64,
) -> Decision {
    // Empty edit context — nothing to match on, so fall back to recency
    // alone. This handles Write-with-empty-content and similar edge cases.
    let no_context = edit_tokens.is_empty();

    let most_recent = recalls.first();
    let recency_secs = most_recent.map(|r| r.age_secs(now_ns));

    // Recency shortcut.
    if let Some(age) = recency_secs
        && age <= policy.window_short_secs
    {
        return Decision {
            verdict: Verdict::Pass,
            reason: "recency",
            recency_secs,
            best_score: None,
            considered: recalls.len(),
            missed_tokens: Vec::new(),
        };
    }

    // Cold start — never recalled in this session. Pass/nudge/block by mode.
    if recalls.is_empty() {
        return miss_verdict(
            policy,
            Decision {
                verdict: Verdict::Pass, // filled below
                reason: "cold_start",
                recency_secs: None,
                best_score: None,
                considered: 0,
                missed_tokens: top_missed_tokens(edit_tokens, &HashSet::new()),
            },
        );
    }

    // Relevance disabled (threshold <= 0) — old time-window-only behavior:
    // beyond short window → miss branch.
    if policy.relevance_threshold <= 0.0 {
        return miss_verdict(
            policy,
            Decision {
                verdict: Verdict::Pass,
                reason: "time_lapsed",
                recency_secs,
                best_score: None,
                considered: recalls.len(),
                missed_tokens: Vec::new(),
            },
        );
    }

    // Relevance scoring: for each recall in pool, tokenize + containment vs
    // edit tokens. Keep best score + the matching recall's covered set so
    // we can report missed tokens.
    let mut best_score = 0.0f64;
    let mut best_covered: HashSet<String> = HashSet::new();
    for r in recalls {
        let q = tokenize(&r.text);
        let score = containment(edit_tokens, &q);
        if score > best_score {
            best_score = score;
            best_covered = q;
        }
    }

    if !no_context && best_score >= policy.relevance_threshold {
        return Decision {
            verdict: Verdict::Pass,
            reason: "relevance",
            recency_secs,
            best_score: Some(best_score),
            considered: recalls.len(),
            missed_tokens: Vec::new(),
        };
    }

    miss_verdict(
        policy,
        Decision {
            verdict: Verdict::Pass,
            reason: if no_context {
                "no_edit_context"
            } else {
                "relevance_miss"
            },
            recency_secs,
            best_score: Some(best_score),
            considered: recalls.len(),
            missed_tokens: top_missed_tokens(edit_tokens, &best_covered),
        },
    )
}

/// Fill `verdict` from `policy.mode` for the "miss" branch.
fn miss_verdict(policy: &Policy, mut base: Decision) -> Decision {
    base.verdict = match policy.mode {
        PolicyMode::Off => Verdict::Pass,
        PolicyMode::Nudge => Verdict::Nudge,
        PolicyMode::Strict => Verdict::Block,
    };
    base
}

/// Containment ratio: `|A ∩ B| / min(|A|, |B|)`. Fair to asymmetric sets —
/// a short precise query against a big edit gets proper credit.
fn containment(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f64;
    let denom = a.len().min(b.len()) as f64;
    inter / denom
}

/// Edit tokens *not* in any recall's covered set, capped at 5, sorted
/// for determinism. Surfaces in the stderr message as "tokens you're
/// editing that no recent recall mentioned".
fn top_missed_tokens(edit: &HashSet<String>, covered: &HashSet<String>) -> Vec<String> {
    let mut missed: Vec<String> = edit.difference(covered).cloned().collect();
    missed.sort();
    missed.truncate(5);
    missed
}

// ===========================================================================
// Stderr message (agent-facing actionable explanation)
// ===========================================================================

fn format_stderr(
    decision: &Decision,
    tool_name: &str,
    tool_input: &Value,
    recalls: &[RecallRow],
    edit_tokens: &HashSet<String>,
    policy: &Policy,
) -> String {
    if decision.verdict == Verdict::Pass {
        return String::new();
    }

    let path = tool_input_path(tool_input).unwrap_or_else(|| "<unknown>".to_string());
    let mut tokens: Vec<&String> = edit_tokens.iter().collect();
    tokens.sort();
    let tokens_str = if tokens.is_empty() {
        "(none)".to_string()
    } else {
        tokens
            .iter()
            .take(10)
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };

    let now_ns = now_unix_ns();
    let mut recalls_block = String::new();
    if recalls.is_empty() {
        recalls_block.push_str(&format!(
            "  (no recalls in the last {}min)\n",
            policy.window_long_secs / 60
        ));
    } else {
        // Score each recall the same way decide() does, sort by score desc.
        let mut scored: Vec<(f64, &RecallRow)> = recalls
            .iter()
            .map(|r| (containment(edit_tokens, &tokenize(&r.text)), r))
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        for (score, r) in scored.iter().take(5) {
            let age = r.age_secs(now_ns);
            let q = truncate(&r.text, 60);
            recalls_block.push_str(&format!("  [{age:>4}s ago]  {q:<60}  overlap {score:.2}\n"));
        }
    }

    let suggestion = if decision.missed_tokens.is_empty() {
        String::new()
    } else {
        format!(
            "\nSuggested: mcp__thoth__thoth_recall({{\n    \"query\": \"{}\"\n  }})\n",
            decision.missed_tokens.join(" ")
        )
    };

    let verdict_label = match decision.verdict {
        Verdict::Block => "blocking this edit",
        Verdict::Nudge => "warning — this edit will proceed",
        Verdict::Pass => "pass", // unreachable above
    };

    format!(
        "Thoth gate: {label}.\n\
         \n\
         Tool: {tool_name} {path}\n\
         Edit context tokens: {tokens}\n\
         \n\
         Recent recalls (last {window_min} min, sorted by relevance):\n\
         {recalls}\
         Best overlap: {best:.2} (threshold: {threshold:.2}, mode: {mode})\n\
         Actor: {actor}\n\
         {suggestion}",
        label = verdict_label,
        tokens = tokens_str,
        window_min = policy.window_long_secs / 60,
        recalls = recalls_block,
        best = decision.best_score.unwrap_or(0.0),
        threshold = policy.relevance_threshold,
        mode = policy.mode.as_str(),
        actor = resolve_actor(),
        suggestion = suggestion,
    )
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
    out.push('…');
    out
}

// ===========================================================================
// JSON emitters
// ===========================================================================

fn approve_json(reason: Option<&str>) -> Value {
    match reason {
        Some(r) => json!({ "decision": "approve", "reason": r }),
        None => json!({ "decision": "approve" }),
    }
}

fn block_json(reason: &str) -> Value {
    json!({ "decision": "block", "reason": reason })
}

// ===========================================================================
// Config
// ===========================================================================

#[derive(Clone, Debug)]
struct GateConfig {
    /// Legacy: `global_fallback = true` means "fall back to `~/.thoth/`
    /// when no project-local `.thoth/` exists". Kept as-is.
    global_fallback: bool,
    /// Legacy master switch — set to `false` to disable discipline
    /// entirely (pass every call). Matches v1 semantics.
    nudge_before_write: bool,
    /// Whether to append each decision to `.thoth/gate.jsonl`.
    telemetry_enabled: bool,
    /// Bash commands that start with any of these prefixes bypass the
    /// gate. Includes legacy defaults plus user-provided additions.
    bash_readonly_prefixes: Vec<String>,
    /// Default policy — applied when no `[[policies]]` entry matches the
    /// current actor, or no actor is set.
    default_policy: Policy,
    /// Actor-specific overrides. First matching glob wins.
    policies: Vec<ActorPolicy>,
}

impl GateConfig {
    fn resolve_policy(&self, actor: &str) -> Policy {
        for entry in &self.policies {
            if glob_match(&entry.actor_glob, actor) {
                return entry.policy.clone();
            }
        }
        self.default_policy.clone()
    }
}

impl Default for GateConfig {
    fn default() -> Self {
        // Derive every runtime default from the shared `DisciplineConfig`
        // defaults so the gate can never disagree with the rest of the
        // workspace on what "unconfigured" means.
        let disc = DisciplineConfig::default();
        Self {
            global_fallback: disc.global_fallback,
            nudge_before_write: disc.nudge_before_write,
            telemetry_enabled: disc.gate_telemetry_enabled,
            bash_readonly_prefixes: DEFAULT_BASH_READONLY_PREFIXES
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            default_policy: Policy::default(),
            policies: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
struct Policy {
    mode: PolicyMode,
    window_short_secs: u64,
    window_long_secs: u64,
    /// Containment threshold in [0.0, 1.0].
    ///
    /// Suggested ranges:
    /// - `0.0`  — disables relevance (time-only, legacy behavior).
    /// - `0.15` — permissive; catches only clear mismatch (recall about X,
    ///   edit about Y with zero token overlap).
    /// - `0.30` — balanced (default). Normal edits with a topical recall
    ///   comfortably pass; ritual `recall("x")` fails.
    /// - `0.50` — strict; forces exact token overlap, usable but agent will
    ///   sometimes need to re-recall for edits that drift within a
    ///   topic.
    /// - `0.70+` — very strict; expect noticeable friction.
    relevance_threshold: f64,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            mode: PolicyMode::Nudge,
            window_short_secs: DEFAULT_WINDOW_SHORT_SECS,
            window_long_secs: DEFAULT_WINDOW_LONG_SECS,
            relevance_threshold: DEFAULT_RELEVANCE_THRESHOLD,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PolicyMode {
    /// Always pass silently (no gate).
    Off,
    /// Pass on miss but emit a stderr warning — default.
    Nudge,
    /// Block on miss.
    Strict,
}

impl PolicyMode {
    fn as_str(self) -> &'static str {
        match self {
            PolicyMode::Off => "off",
            PolicyMode::Nudge => "nudge",
            PolicyMode::Strict => "strict",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "off" | "disabled" => Some(PolicyMode::Off),
            "nudge" | "soft" | "relevance" => Some(PolicyMode::Nudge),
            "strict" | "block" | "hard" => Some(PolicyMode::Strict),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
struct ActorPolicy {
    actor_glob: String,
    policy: Policy,
}

// ---- config loading --------------------------------------------------------
//
// Gate-v2 keys all live on `thoth_memory::DisciplineConfig`. This is the
// only parser in the workspace that consumes them — earlier, the gate
// kept its own `DisciplineFile` struct with `Option<T>` fields to tell
// "unset" from "default". That turned out to be a liability: adding a
// new field to `DisciplineConfig` (the *other* parser reading the same
// file) and missing it here caused silent config drift. Now there is
// one struct, one parse, and the gate reads the already-defaulted values
// directly.

fn load_config(root: &Path) -> GateConfig {
    let disc = DisciplineConfig::load_or_default_sync(root);

    let default_mode = if disc.mode.is_empty() {
        PolicyMode::Nudge
    } else {
        match PolicyMode::parse(&disc.mode) {
            Some(m) => m,
            None => {
                eprintln!(
                    "[thoth-gate] unknown mode {:?}; using default {DEFAULT_MODE}",
                    disc.mode
                );
                PolicyMode::Nudge
            }
        }
    };

    let mut cfg = GateConfig {
        global_fallback: disc.global_fallback,
        nudge_before_write: disc.nudge_before_write,
        telemetry_enabled: disc.gate_telemetry_enabled,
        bash_readonly_prefixes: DEFAULT_BASH_READONLY_PREFIXES
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        default_policy: Policy {
            mode: default_mode,
            window_short_secs: disc.gate_window_short_secs,
            window_long_secs: disc.gate_window_long_secs,
            relevance_threshold: disc.gate_relevance_threshold.clamp(0.0, 1.0),
        },
        policies: Vec::with_capacity(disc.policies.len()),
    };

    // User-provided read-only prefixes *extend* the built-in list —
    // nobody should lose `cargo test` / `git status` by customising.
    for p in disc.gate_bash_readonly_prefixes {
        if !cfg.bash_readonly_prefixes.contains(&p) {
            cfg.bash_readonly_prefixes.push(p);
        }
    }

    // Actor-specific policies. Missing fields inherit from default_policy.
    for pf in disc.policies {
        let mut p = cfg.default_policy.clone();
        if let Some(m) = pf.mode.as_deref().and_then(PolicyMode::parse) {
            p.mode = m;
        }
        if let Some(w) = pf.window_short_secs {
            p.window_short_secs = w;
        }
        if let Some(w) = pf.window_long_secs {
            p.window_long_secs = w;
        }
        if let Some(r) = pf.relevance_threshold {
            p.relevance_threshold = r.clamp(0.0, 1.0);
        }
        cfg.policies.push(ActorPolicy {
            actor_glob: pf.actor,
            policy: p,
        });
    }

    cfg
}

// ---- actor resolution -------------------------------------------------------

fn resolve_actor() -> String {
    std::env::var("THOTH_ACTOR").unwrap_or_else(|_| "default".to_string())
}

/// Tiny glob matcher supporting `*` and `?`. Anchored full-match. Good
/// enough for actor patterns like `hoangsa/*`, `ci-*`.
fn glob_match(pattern: &str, input: &str) -> bool {
    fn helper(p: &[u8], s: &[u8]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (None, Some(_)) => false,
            (Some(&b'*'), _) => {
                // * matches zero chars...
                if helper(&p[1..], s) {
                    return true;
                }
                // ... or one+ chars.
                if !s.is_empty() && helper(p, &s[1..]) {
                    return true;
                }
                false
            }
            (Some(&b'?'), Some(_)) => helper(&p[1..], &s[1..]),
            (Some(&pc), Some(&sc)) if pc == sc => helper(&p[1..], &s[1..]),
            _ => false,
        }
    }
    helper(pattern.as_bytes(), input.as_bytes())
}

// ===========================================================================
// Telemetry
// ===========================================================================

struct TelemetryRecord {
    root: PathBuf,
    ts_iso: String,
    actor: String,
    tool: String,
    path: Option<String>,
    decision: String,
    reason: String,
    recency_secs: Option<u64>,
    best_score: Option<f64>,
    considered: usize,
    missed_tokens: Vec<String>,
}

/// Truncate `gate.jsonl` at ~1 MiB. Readers filter by `.session-start`
/// mtime so anything past the cap is outside the session window anyway.
const GATE_LOG_CAP_BYTES: u64 = 1024 * 1024;

fn truncate_gate_log_if_oversize(path: &Path) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() < GATE_LOG_CAP_BYTES {
        return;
    }
    if let Err(e) = std::fs::remove_file(path) {
        eprintln!("thoth-gate: log truncate failed: {e}");
    }
}

fn append_telemetry(rec: &TelemetryRecord) -> io::Result<()> {
    let path = rec.root.join("gate.jsonl");
    truncate_gate_log_if_oversize(&path);
    let mut line = serde_json::json!({
        "ts": rec.ts_iso,
        "actor": rec.actor,
        "tool": rec.tool,
        "path": rec.path,
        "decision": rec.decision,
        "reason": rec.reason,
        "considered": rec.considered,
    });
    if let Some(r) = rec.recency_secs {
        line["recency_secs"] = json!(r);
    }
    if let Some(s) = rec.best_score {
        line["best_score"] = json!(s);
    }
    if !rec.missed_tokens.is_empty() {
        line["missed_tokens"] = json!(rec.missed_tokens);
    }
    let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(f, "{line}")?;
    Ok(())
}

// ===========================================================================
// Root resolution + time
// ===========================================================================

fn first_existing_root(allow_home_fallback: bool) -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("THOTH_ROOT") {
        let p = PathBuf::from(explicit);
        if p.is_dir() {
            return Some(p);
        }
    }
    let local = PathBuf::from(".thoth");
    if local.is_dir() {
        return Some(local);
    }
    if allow_home_fallback && let Some(home) = std::env::var_os("HOME") {
        let p = PathBuf::from(home).join(".thoth");
        if p.is_dir() {
            return Some(p);
        }
    }
    None
}

fn now_unix_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn now_iso() -> String {
    // Minimal ISO-ish: `YYYY-MM-DDTHH:MM:SSZ`. We don't pull `time` just
    // for the gate binary — dependency slimming matters here (every
    // PreToolUse invocation links + executes).
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Classic epoch-to-YMD conversion. No leap-second handling; this is
    // telemetry, not an accounting system.
    let days = secs / 86_400;
    let h = (secs / 3600) % 24;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    let (y, mo, d) = ymd_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Civil-date conversion from Unix epoch day. Adapted from Howard Hinnant's
/// `days_from_civil` inverse — small, ASCII-safe, no external dep.
fn ymd_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- tokenizer ----

    #[test]
    fn tokenize_camelcase_emits_both_joined_and_segments() {
        let toks = tokenize("FooBarBaz");
        assert!(toks.contains("foobarbaz"));
        assert!(toks.contains("foo"));
        assert!(toks.contains("bar"));
        assert!(toks.contains("baz"));
    }

    #[test]
    fn tokenize_snake_case_preserved() {
        let toks = tokenize("my_field_name");
        assert!(toks.contains("my_field_name"));
    }

    #[test]
    fn tokenize_drops_stopwords_and_keywords() {
        let toks = tokenize("the fn and impl struct");
        for bad in ["the", "fn", "and", "impl", "struct"] {
            assert!(!toks.contains(bad), "should drop stopword/keyword {bad}");
        }
    }

    #[test]
    fn tokenize_drops_single_chars_and_digits() {
        let toks = tokenize("a 42 999 valid_name");
        assert!(!toks.contains("a"));
        assert!(!toks.contains("42"));
        assert!(!toks.contains("999"));
        assert!(toks.contains("valid_name"));
    }

    #[test]
    fn tokenize_camel_absorbs_acronym_runs() {
        let segs = split_camel("HTTPSServer");
        // HTTPSServer → ["HTTPS", "Server"]
        assert_eq!(segs, vec!["HTTPS".to_string(), "Server".to_string()]);
    }

    // ---- containment ----

    #[test]
    fn containment_handles_empty_sets() {
        let a: HashSet<String> = HashSet::new();
        let b: HashSet<String> = ["foo".into()].into_iter().collect();
        assert_eq!(containment(&a, &b), 0.0);
        assert_eq!(containment(&b, &a), 0.0);
    }

    #[test]
    fn containment_min_denominator() {
        let a: HashSet<String> = ["foo", "bar", "baz"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let b: HashSet<String> = ["foo"].iter().map(|s| s.to_string()).collect();
        // |A ∩ B| = 1; min(|A|, |B|) = 1 → score 1.0 (short query fully covered).
        assert!((containment(&a, &b) - 1.0).abs() < 1e-9);
    }

    // ---- intent classification ----

    #[test]
    fn bash_whitelist_catches_cargo_test() {
        let prefixes = DEFAULT_BASH_READONLY_PREFIXES
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        let input = json!({"command": "cargo test -p thoth-mcp"});
        let intent = classify_intent("Bash", &input, &prefixes);
        assert_eq!(intent, Intent::ReadOnly);
    }

    #[test]
    fn bash_mutation_not_whitelisted() {
        let prefixes = DEFAULT_BASH_READONLY_PREFIXES
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        let input = json!({"command": "rm -rf .thoth"});
        let intent = classify_intent("Bash", &input, &prefixes);
        assert_eq!(intent, Intent::Mutation);
    }

    #[test]
    fn bash_shell_chain_doesnt_sneak_mutation_through() {
        // Attack: prefix with `cat foo;` to try to bypass — we should
        // still catch because first_line is `cat foo; rm -rf bar` which
        // doesn't match any readonly prefix cleanly beyond `cat `.
        // Because we startswith-match, this one WILL match cat. That's
        // a known limitation — agents piping destructive commands after
        // `cat foo;` wouldn't gain much (they'd still need a new gate
        // call to edit). Documented: whitelist is first-token heuristic.
        let prefixes = DEFAULT_BASH_READONLY_PREFIXES
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        let input = json!({"command": "cat foo; rm -rf bar"});
        // With our current naive startswith, this matches `cat `. We
        // accept this — the alternative (shell lexer) blows the
        // complexity budget. If tightening is needed later, restrict to
        // exact-first-word match.
        let intent = classify_intent("Bash", &input, &prefixes);
        assert_eq!(intent, Intent::ReadOnly);
    }

    #[test]
    fn gate_whitelists_memory_replace() {
        // REQ-11: the Bash prefix `thoth memory replace …` must classify
        // as ReadOnly so the agent can curate memory under a debt
        // lockdown.
        let prefixes = DEFAULT_BASH_READONLY_PREFIXES
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        let input = json!({"command": "thoth memory replace --kind fact --query foo --text bar"});
        let intent = classify_intent("Bash", &input, &prefixes);
        assert_eq!(intent, Intent::ReadOnly);
        // And `thoth memory remove` / `preference` follow the same rule.
        let input = json!({"command": "thoth memory remove --kind lesson --query old"});
        assert_eq!(
            classify_intent("Bash", &input, &prefixes),
            Intent::ReadOnly
        );
        // MCP dispatch of `thoth_memory_replace` also passes ReadOnly.
        let input = json!({});
        assert_eq!(
            classify_intent("mcp__thoth__thoth_memory_replace", &input, &prefixes),
            Intent::ReadOnly
        );
    }

    #[test]
    fn edit_tool_is_mutation() {
        let intent = classify_intent(
            "Edit",
            &json!({"file_path": "foo.rs", "old_string": "a", "new_string": "b"}),
            &[],
        );
        assert_eq!(intent, Intent::Mutation);
    }

    // ---- edit context extraction ----

    #[test]
    fn edit_pulls_tokens_from_filename_and_strings() {
        let input = json!({
            "file_path": "crates/thoth-retrieve/src/retriever.rs",
            "old_string": "pub fn old_name() {}",
            "new_string": "pub fn new_name() {}",
        });
        let toks = extract_edit_tokens("Edit", &input);
        assert!(toks.contains("retriever"));
        assert!(toks.contains("old_name"));
        assert!(toks.contains("new_name"));
    }

    #[test]
    fn edit_caps_at_edit_token_cap() {
        // Build a synthetic new_string with 500+ unique identifiers.
        let many = (0..500)
            .map(|i| format!("sym_{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let input = json!({ "file_path": "x.rs", "new_string": many });
        let toks = extract_edit_tokens("Edit", &input);
        assert!(toks.len() <= EDIT_TOKEN_CAP);
    }

    // ---- decision ----

    fn mk_policy(mode: PolicyMode, short: u64, long: u64, threshold: f64) -> Policy {
        Policy {
            mode,
            window_short_secs: short,
            window_long_secs: long,
            relevance_threshold: threshold,
        }
    }

    fn mk_recall(age_secs: u64, text: &str) -> RecallRow {
        let now_ns = now_unix_ns();
        RecallRow {
            ts_ns: now_ns.saturating_sub(age_secs * 1_000_000_000),
            text: text.to_string(),
        }
    }

    #[test]
    fn recency_shortcut_passes_without_relevance_check() {
        // Very recent recall about `xyz`; edit about `abc`. Under relevance
        // alone this would miss. But recency wins first.
        let policy = mk_policy(PolicyMode::Strict, 60, 1800, 0.30);
        let recalls = vec![mk_recall(5, "xyz unrelated")];
        let edit: HashSet<String> = ["abc", "def"].iter().map(|s| s.to_string()).collect();
        let d = decide(&policy, &recalls, &edit, now_unix_ns());
        assert_eq!(d.verdict, Verdict::Pass);
        assert_eq!(d.reason, "recency");
    }

    #[test]
    fn relevance_pass_when_score_ge_threshold() {
        let policy = mk_policy(PolicyMode::Strict, 10, 1800, 0.30);
        let recalls = vec![mk_recall(200, "retriever bfs callers depth")];
        let edit: HashSet<String> = ["retriever", "bfs", "depth"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let d = decide(&policy, &recalls, &edit, now_unix_ns());
        // 3/3 overlap → 1.0, well above 0.30.
        assert_eq!(d.verdict, Verdict::Pass);
        assert_eq!(d.reason, "relevance");
    }

    #[test]
    fn ritual_recall_blocked_under_strict() {
        // Ritual: recall query has nothing to do with the edit.
        let policy = mk_policy(PolicyMode::Strict, 10, 1800, 0.30);
        let recalls = vec![mk_recall(100, "tao cần chỉnh tiếp bypass")];
        let edit: HashSet<String> = ["retriever", "bfs", "depth"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let d = decide(&policy, &recalls, &edit, now_unix_ns());
        assert_eq!(d.verdict, Verdict::Block);
        assert_eq!(d.reason, "relevance_miss");
        assert!(!d.missed_tokens.is_empty());
    }

    #[test]
    fn ritual_recall_nudges_under_default() {
        let policy = mk_policy(PolicyMode::Nudge, 10, 1800, 0.30);
        let recalls = vec![mk_recall(100, "unrelated stuff")];
        let edit: HashSet<String> = ["retriever"].iter().map(|s| s.to_string()).collect();
        let d = decide(&policy, &recalls, &edit, now_unix_ns());
        assert_eq!(d.verdict, Verdict::Nudge);
    }

    #[test]
    fn off_mode_always_passes() {
        let policy = mk_policy(PolicyMode::Off, 10, 1800, 0.30);
        let recalls = vec![];
        let edit: HashSet<String> = ["x"].iter().map(|s| s.to_string()).collect();
        let d = decide(&policy, &recalls, &edit, now_unix_ns());
        assert_eq!(d.verdict, Verdict::Pass);
    }

    #[test]
    fn cold_start_respects_policy_mode() {
        let strict = mk_policy(PolicyMode::Strict, 10, 1800, 0.30);
        let nudge = mk_policy(PolicyMode::Nudge, 10, 1800, 0.30);
        let edit: HashSet<String> = ["x"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            decide(&strict, &[], &edit, now_unix_ns()).verdict,
            Verdict::Block
        );
        assert_eq!(
            decide(&nudge, &[], &edit, now_unix_ns()).verdict,
            Verdict::Nudge
        );
    }

    #[test]
    fn zero_threshold_disables_relevance() {
        // Threshold 0 = legacy behavior: recency-only, beyond window → miss.
        let policy = mk_policy(PolicyMode::Strict, 10, 1800, 0.0);
        let recalls = vec![mk_recall(100, "totally different topic")];
        let edit: HashSet<String> = ["anything"].iter().map(|s| s.to_string()).collect();
        let d = decide(&policy, &recalls, &edit, now_unix_ns());
        // Past short window + threshold 0 → time_lapsed miss → block under strict.
        assert_eq!(d.verdict, Verdict::Block);
        assert_eq!(d.reason, "time_lapsed");
    }

    // ---- reflection-debt block ----

    // These tests mutate process-wide env (`THOTH_ROOT`,
    // `THOTH_DEFER_REFLECT`) — serialise them so the default
    // parallel test runner can't race.
    static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn reflection_debt_block_fires_on_mutation_over_threshold() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        // Set up a .thoth root with:
        //   - config.toml lowering reflect_debt_block to 3
        //   - gate.jsonl with 5 passed Write events (debt = 5)
        //   - no memory-history.jsonl (remembers = 0)
        // Gate should block the incoming Write tool call.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.toml"),
            r#"
[discipline]
mode = "nudge"
reflect_debt_block = 3
"#,
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("gate.jsonl"),
            (0..5)
                .map(|_| r#"{"ts":"2026-04-17T10:00:00Z","tool":"Write","decision":"pass","reason":""}"#)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        // Point the gate at this root via THOTH_ROOT. `THOTH_DEFER_REFLECT`
        // is cleared so the bypass doesn't kick in.
        // SAFETY: tests using env vars serialise via the runtime scheduler;
        // we use distinct vars so no sibling test races on these.
        unsafe {
            std::env::set_var("THOTH_ROOT", tmp.path());
            std::env::remove_var("THOTH_DEFER_REFLECT");
        }

        let input = json!({
            "tool_name": "Write",
            "tool_input": { "file_path": "src/lib.rs", "content": "fn x() {}" }
        });
        let (verdict, stderr, telemetry) = run(&input);

        assert_eq!(verdict["decision"], "block", "verdict: {verdict}");
        let msg = stderr.expect("stderr message on block");
        assert!(
            msg.contains("reflection debt"),
            "stderr should explain why: {msg}"
        );
        assert!(
            msg.contains("THOTH_DEFER_REFLECT"),
            "stderr should mention bypass: {msg}"
        );
        // No telemetry because the config above left telemetry_enabled at
        // its default (false).
        assert!(
            telemetry.is_none(),
            "telemetry shouldn't fire when disabled"
        );

        unsafe {
            std::env::remove_var("THOTH_ROOT");
        }
    }

    #[test]
    fn reflection_debt_bypass_via_env_var() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.toml"),
            r#"
[discipline]
mode = "nudge"
reflect_debt_block = 3
"#,
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("gate.jsonl"),
            (0..10)
                .map(|_| r#"{"ts":"2026-04-17T10:00:00Z","tool":"Write","decision":"pass","reason":""}"#)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        unsafe {
            std::env::set_var("THOTH_ROOT", tmp.path());
            std::env::set_var("THOTH_DEFER_REFLECT", "1");
        }

        let input = json!({
            "tool_name": "Write",
            "tool_input": { "file_path": "src/lib.rs", "content": "fn x() {}" }
        });
        let (verdict, _stderr, _tel) = run(&input);
        // With bypass set, the debt check doesn't fire — we fall through
        // to the recall-relevance path, which on an empty log under
        // nudge mode emits a nudge (not a block).
        assert_ne!(
            verdict["decision"], "block",
            "bypass should prevent debt block: {verdict}"
        );

        unsafe {
            std::env::remove_var("THOTH_ROOT");
            std::env::remove_var("THOTH_DEFER_REFLECT");
        }
    }

    // ---- glob ----

    #[test]
    fn glob_match_basics() {
        assert!(glob_match("hoangsa/*", "hoangsa/cook-wave-3"));
        assert!(glob_match("ci-*", "ci-github"));
        assert!(!glob_match("hoangsa/*", "claude-code-direct"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("exact", "exact"));
        assert!(!glob_match("exact", "exacts"));
    }

    // ---- config load ----

    #[test]
    fn legacy_config_still_parses() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.toml"),
            r#"
[discipline]
mode = "strict"
gate_window_secs = 180
"#,
        )
        .unwrap();
        let cfg = load_config(tmp.path());
        assert_eq!(cfg.default_policy.mode, PolicyMode::Strict);
        // Legacy window maps to short.
        assert_eq!(cfg.default_policy.window_short_secs, 180);
        // New fields default.
        assert!(
            (cfg.default_policy.relevance_threshold - DEFAULT_RELEVANCE_THRESHOLD).abs() < 1e-9
        );
    }

    #[test]
    fn new_config_with_policies() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.toml"),
            r#"
[discipline]
mode = "nudge"
gate_relevance_threshold = 0.50
gate_window_short_secs = 90
gate_telemetry_enabled = true

[[discipline.policies]]
actor = "hoangsa/*"
mode = "nudge"
relevance_threshold = 0.20

[[discipline.policies]]
actor = "ci-*"
mode = "off"
"#,
        )
        .unwrap();
        let cfg = load_config(tmp.path());
        assert_eq!(cfg.default_policy.mode, PolicyMode::Nudge);
        assert!((cfg.default_policy.relevance_threshold - 0.50).abs() < 1e-9);
        assert!(cfg.telemetry_enabled);
        assert_eq!(cfg.policies.len(), 2);

        let hoangsa = cfg.resolve_policy("hoangsa/cook-wave-1");
        assert_eq!(hoangsa.mode, PolicyMode::Nudge);
        assert!((hoangsa.relevance_threshold - 0.20).abs() < 1e-9);

        let ci = cfg.resolve_policy("ci-github");
        assert_eq!(ci.mode, PolicyMode::Off);

        let other = cfg.resolve_policy("random");
        assert_eq!(other.mode, PolicyMode::Nudge);
    }

    #[test]
    fn bash_readonly_prefixes_are_additive() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.toml"),
            r#"
[discipline]
gate_bash_readonly_prefixes = ["my-custom-tool "]
"#,
        )
        .unwrap();
        let cfg = load_config(tmp.path());
        // Built-ins preserved.
        assert!(
            cfg.bash_readonly_prefixes
                .iter()
                .any(|p| p.starts_with("cargo test"))
        );
        // Custom added.
        assert!(
            cfg.bash_readonly_prefixes
                .iter()
                .any(|p| p == "my-custom-tool ")
        );
    }
}

//! `thoth-gate` — strict-mode enforcement binary for the thoth-discipline
//! Claude Code plugin.
//!
//! Called as a PreToolUse hook. Reads `<root>/config.toml` and the most
//! recent `query_issued` row from `<root>/episodes.db`, then emits a
//! verdict on stdout:
//!
//! ```json
//! {"decision": "approve"}
//! {"decision": "block",   "reason": "..."}
//! {"decision": "approve", "reason": "..."}   // soft-mode warning
//! ```
//!
//! The gate **always fails open**: a broken config, missing file, or
//! corrupt SQLite must never brick the user's editor. Errors go to
//! stderr and the decision is `approve`.
//!
//! ## Why a binary
//!
//! An earlier version shelled out to Python. That made the plugin depend
//! on a `python3` interpreter on `$PATH`, which isn't guaranteed on every
//! developer machine (fresh macOS, minimal Docker images, Windows without
//! WSL). The Rust version is a single self-contained executable that
//! `cargo install --path crates/thoth-mcp` ships alongside `thoth-mcp`.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;
use serde_json::json;

// Defaults mirroring `thoth_memory::DisciplineConfig`.
const DEFAULT_MODE: &str = "soft";
const DEFAULT_GLOBAL_FALLBACK: bool = true;
const DEFAULT_NUDGE_BEFORE_WRITE: bool = true;
const DEFAULT_WINDOW_SECS: u64 = 180;
const DEFAULT_GATE_REQUIRE_NUDGE: bool = false;

fn main() -> ExitCode {
    // Drain stdin so Claude Code's hook runner doesn't deadlock when it
    // pipes the tool-call JSON in. We don't need the content — presence
    // of a recent recall is enough to decide.
    let mut _buf = String::new();
    let _ = io::stdin().read_to_string(&mut _buf);

    let verdict = run();
    println!("{verdict}");
    ExitCode::SUCCESS
}

fn run() -> serde_json::Value {
    // Bootstrap: probe project-local first, then home if allowed. We need
    // config to know whether to do the home fallback, so read config from
    // whichever directory exists first.
    let bootstrap = first_existing_root(true);
    let cfg = match bootstrap.as_deref() {
        Some(p) => load_config(p),
        None => Config::default(),
    };

    if !cfg.nudge_before_write {
        return approve(None);
    }

    let root = match first_existing_root(cfg.global_fallback) {
        Some(p) => p,
        None => {
            return approve(Some(
                "[thoth-gate] no .thoth/ directory found; discipline disabled. \
                 Run `thoth index .` to enable.",
            ));
        }
    };

    // Re-read config from the resolved root in case it differs from the
    // bootstrap location (project-local vs ~/.thoth).
    let cfg = load_config(&root);
    let recall_ns = match last_event_at(&root, "query_issued") {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[thoth-gate] sqlite error: {e}");
            return approve(None); // fail open
        }
    };
    let nudge_ns = match last_event_at(&root, "nudge_invoked") {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[thoth-gate] sqlite error (nudge lookup): {e}");
            // Fall through — only matters when gate_require_nudge is set.
            None
        }
    };

    let now_ns = now_unix_ns();
    let recall_age_s = recall_ns.map(|ns| (now_ns.saturating_sub(ns)) / 1_000_000_000);
    let nudge_age_s = nudge_ns.map(|ns| (now_ns.saturating_sub(ns)) / 1_000_000_000);

    let recall_within = recall_age_s
        .map(|s| s <= cfg.gate_window_secs)
        .unwrap_or(false);
    let nudge_within = nudge_age_s
        .map(|s| s <= cfg.gate_window_secs)
        .unwrap_or(false);

    if recall_within && (!cfg.gate_require_nudge || nudge_within) {
        return approve(None);
    }

    // Compose a reason that names every missing signal.
    let mut parts: Vec<String> = Vec::new();
    if !recall_within {
        parts.push(match recall_age_s {
            None => "no `thoth_recall` has been logged for this project".to_string(),
            Some(s) => format!(
                "last `thoth_recall` was {}s ago (window: {}s)",
                s, cfg.gate_window_secs
            ),
        });
    }
    if cfg.gate_require_nudge && !nudge_within {
        parts.push(match nudge_age_s {
            None => "the `thoth.nudge` prompt has not been expanded this session".to_string(),
            Some(s) => format!(
                "last `thoth.nudge` was {}s ago (window: {}s)",
                s, cfg.gate_window_secs
            ),
        });
    }
    let what = parts.join("; ");
    let todo = if cfg.gate_require_nudge {
        "Call `thoth_recall` for the affected files AND expand the `thoth.nudge` prompt \
         with an `intent` describing this edit, then retry."
    } else {
        "Run a fresh `thoth_recall` for the symbols or files this edit touches, then retry."
    };
    let reason = format!("Thoth discipline: {what}. {todo}");

    if cfg.mode.eq_ignore_ascii_case("strict") {
        block(&reason)
    } else {
        approve(Some(&reason))
    }
}

// ---- JSON emitters --------------------------------------------------------

fn approve(reason: Option<&str>) -> serde_json::Value {
    match reason {
        Some(r) => json!({ "decision": "approve", "reason": r }),
        None => json!({ "decision": "approve" }),
    }
}

fn block(reason: &str) -> serde_json::Value {
    json!({ "decision": "block", "reason": reason })
}

// ---- Root resolution ------------------------------------------------------

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

// ---- Config ---------------------------------------------------------------

#[derive(Clone, Debug)]
struct Config {
    mode: String,
    global_fallback: bool,
    nudge_before_write: bool,
    gate_window_secs: u64,
    gate_require_nudge: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: DEFAULT_MODE.to_string(),
            global_fallback: DEFAULT_GLOBAL_FALLBACK,
            nudge_before_write: DEFAULT_NUDGE_BEFORE_WRITE,
            gate_window_secs: DEFAULT_WINDOW_SECS,
            gate_require_nudge: DEFAULT_GATE_REQUIRE_NUDGE,
        }
    }
}

#[derive(Deserialize, Default)]
struct FileShape {
    #[serde(default)]
    discipline: Option<DisciplineFile>,
}

#[derive(Deserialize, Default)]
struct DisciplineFile {
    mode: Option<String>,
    global_fallback: Option<bool>,
    nudge_before_write: Option<bool>,
    gate_window_secs: Option<u64>,
    gate_require_nudge: Option<bool>,
}

fn load_config(root: &Path) -> Config {
    let path = root.join("config.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Config::default(),
    };
    let shape: FileShape = match toml::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[thoth-gate] config.toml parse error: {e}");
            return Config::default();
        }
    };
    let mut cfg = Config::default();
    if let Some(d) = shape.discipline {
        if let Some(m) = d.mode {
            cfg.mode = m;
        }
        if let Some(g) = d.global_fallback {
            cfg.global_fallback = g;
        }
        if let Some(n) = d.nudge_before_write {
            cfg.nudge_before_write = n;
        }
        if let Some(w) = d.gate_window_secs {
            cfg.gate_window_secs = w;
        }
        if let Some(r) = d.gate_require_nudge {
            cfg.gate_require_nudge = r;
        }
    }
    cfg
}

// ---- SQLite ---------------------------------------------------------------

fn last_event_at(root: &Path, kind: &str) -> rusqlite::Result<Option<u64>> {
    let db = root.join("episodes.db");
    if !db.is_file() {
        return Ok(None);
    }
    // Open read-only — the gate must never race the MCP server's writes.
    let conn = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let mut stmt = conn.prepare(
        "SELECT at_unix_ns FROM episodes \
         WHERE kind = ?1 \
         ORDER BY id DESC LIMIT 1",
    )?;
    let mut rows = stmt.query(rusqlite::params![kind])?;
    if let Some(r) = rows.next()? {
        let ns: i64 = r.get(0)?;
        Ok(Some(ns.max(0) as u64))
    } else {
        Ok(None)
    }
}

// ---- Time -----------------------------------------------------------------

fn now_unix_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

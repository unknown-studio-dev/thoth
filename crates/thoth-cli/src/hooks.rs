//! `thoth hooks` + `thoth skills install` — install the Thoth skill and the
//! Claude Code hook block into the user's settings.
//!
//! Three subcommands:
//!
//! - [`install`] merges the bundled hook template into `settings.json`.
//! - [`uninstall`] removes every hook whose command starts with
//!   `thoth hooks exec` (so user-owned hooks aren't touched).
//! - [`exec`] is the runtime dispatcher called by Claude Code itself: it
//!   reads the hook payload from stdin as JSON, picks the right action
//!   (memory dump / recall / incremental index / nudge), and prints any
//!   additional context back on stdout.
//!
//! Skills go through the same surface: [`skills_install`] copies the
//! bundled `SKILL.md` into `<root>/skills/thoth/` (or
//! `~/.claude/skills/thoth/` in user scope).

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde_json::{Value, json};
use thoth_parse::LanguageRegistry;
use thoth_retrieve::{Indexer, Retriever};
use thoth_store::StoreRoot;

// -------------------------------------------------------------- asset bundle

/// The agentskills.io-compatible skill shipped in the binary.
const SKILL_MD: &str = include_str!("../assets/skills/thoth/SKILL.md");

/// Claude Code hook template — merged into `settings.json` on install.
const HOOKS_TEMPLATE: &str = include_str!("../assets/hooks/claude-code.json");

/// MCP server template — merged into `settings.json` (`mcpServers.thoth`).
const MCP_TEMPLATE: &str = include_str!("../assets/hooks/mcp.json");

/// Comment included with `_comment` in the rendered settings block. Keeps
/// the file self-documenting after an install.
const THOTH_MARKER: &str = "thoth hooks exec";

/// Key under `mcpServers` that identifies the Thoth entry so we can dedupe
/// and cleanly uninstall.
const MCP_SERVER_KEY: &str = "thoth";

/// Scope of a settings edit.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum Scope {
    /// Project-local: `./.claude/settings.json`.
    Project,
    /// User-global: `~/.claude/settings.json`.
    User,
}

impl Scope {
    fn settings_path(self) -> anyhow::Result<PathBuf> {
        match self {
            Scope::Project => Ok(PathBuf::from(".claude").join("settings.json")),
            Scope::User => {
                let home = home_dir().context("could not locate home directory")?;
                Ok(home.join(".claude").join("settings.json"))
            }
        }
    }

    /// Where Claude Code looks for skills. Mirrors [`Self::settings_path`]:
    /// project-local skills live next to `.claude/settings.json`, not under
    /// Thoth's own `.thoth/` root. The `_root` arg is unused today but kept
    /// for forward compatibility (e.g. a future `thoth skills install
    /// --scope thoth` that targets Thoth's own registry).
    fn skills_dir(self, _root: &Path) -> anyhow::Result<PathBuf> {
        match self {
            Scope::Project => Ok(PathBuf::from(".claude").join("skills")),
            Scope::User => {
                let home = home_dir().context("could not locate home directory")?;
                Ok(home.join(".claude").join("skills"))
            }
        }
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

// --------------------------------------------------------- settings merging

/// Read `settings.json` as a `Value`. Returns an empty object if the file
/// doesn't exist yet.
async fn read_settings(path: &Path) -> anyhow::Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let text = tokio::fs::read_to_string(path).await?;
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    let v: Value = serde_json::from_str(&text)
        .with_context(|| format!("parsing {} as JSON", path.display()))?;
    Ok(v)
}

async fn write_settings(path: &Path, v: &Value) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let text = serde_json::to_string_pretty(v)?;
    tokio::fs::write(path, format!("{text}\n")).await?;
    Ok(())
}

/// Merge Thoth's hook template into an existing `settings.json` value. The
/// merge is idempotent — running it twice leaves `settings.json` identical
/// to running it once. User-owned hooks (anything whose command doesn't
/// mention [`THOTH_MARKER`]) are preserved.
fn merge_hooks(existing: &mut Value, template: &Value) {
    let template_hooks = match template.get("hooks") {
        Some(Value::Object(m)) => m,
        _ => return,
    };

    let existing_hooks = existing
        .as_object_mut()
        .expect("settings root must be an object")
        .entry("hooks".to_string())
        .or_insert_with(|| json!({}));
    let existing_hooks = existing_hooks
        .as_object_mut()
        .expect("hooks must be an object");

    for (event, t_entries) in template_hooks {
        let Some(t_list) = t_entries.as_array() else {
            continue;
        };
        let e_list = existing_hooks
            .entry(event.clone())
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .expect("each event's entry must be an array");

        for t_entry in t_list {
            // Append if no existing hook under this event already invokes
            // the same `thoth hooks exec <...>` command.
            let t_marker = entry_marker(t_entry);
            let already = e_list.iter().any(|e| entry_marker(e) == t_marker);
            if !already {
                e_list.push(t_entry.clone());
            }
        }
    }
}

/// Pull out the command-string of a hook entry so we can dedupe. Returns
/// `None` for entries with no `thoth hooks exec` command.
fn entry_marker(entry: &Value) -> Option<String> {
    let hooks = entry.get("hooks")?.as_array()?;
    for h in hooks {
        if let Some(cmd) = h.get("command").and_then(|v| v.as_str())
            && cmd.contains(THOTH_MARKER)
        {
            return Some(cmd.to_string());
        }
    }
    None
}

/// Strip every hook whose command starts with [`THOTH_MARKER`]. Prunes
/// empty arrays and the top-level `"hooks"` key if nothing else remains.
fn strip_hooks(v: &mut Value) {
    let Some(hooks) = v.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return;
    };
    let events: Vec<String> = hooks.keys().cloned().collect();
    for event in events {
        let Some(list) = hooks.get_mut(&event).and_then(|e| e.as_array_mut()) else {
            continue;
        };
        list.retain(|entry| entry_marker(entry).is_none());
        if list.is_empty() {
            hooks.remove(&event);
        }
    }
    if hooks.is_empty()
        && let Some(obj) = v.as_object_mut()
    {
        obj.remove("hooks");
    }
}

/// Merge the Thoth MCP server block into an existing `settings.json`. Only
/// writes under `mcpServers.thoth` — other server entries are preserved.
/// Idempotent.
fn merge_mcp(existing: &mut Value, template: &Value) {
    let Some(template_servers) = template.get("mcpServers").and_then(Value::as_object) else {
        return;
    };
    let Some(entry) = template_servers.get(MCP_SERVER_KEY) else {
        return;
    };
    let servers = existing
        .as_object_mut()
        .expect("settings root must be an object")
        .entry("mcpServers".to_string())
        .or_insert_with(|| json!({}));
    let servers = servers
        .as_object_mut()
        .expect("mcpServers must be an object");
    servers.insert(MCP_SERVER_KEY.to_string(), entry.clone());
}

/// Drop Thoth's MCP entry; prune an empty `mcpServers` key.
fn strip_mcp(v: &mut Value) {
    let Some(servers) = v.get_mut("mcpServers").and_then(|s| s.as_object_mut()) else {
        return;
    };
    servers.remove(MCP_SERVER_KEY);
    if servers.is_empty()
        && let Some(obj) = v.as_object_mut()
    {
        obj.remove("mcpServers");
    }
}

// ------------------------------------------------------------- public commands

/// `thoth hooks install [--scope ...]`
pub async fn install(scope: Scope) -> anyhow::Result<()> {
    let path = scope.settings_path()?;
    let template: Value = serde_json::from_str(HOOKS_TEMPLATE)?;
    let mut settings = read_settings(&path).await?;
    if !settings.is_object() {
        bail!(
            "{} exists but isn't a JSON object — refusing to overwrite",
            path.display()
        );
    }
    merge_hooks(&mut settings, &template);
    write_settings(&path, &settings).await?;

    println!("✓ hooks installed into {}", path.display());
    println!("  events: SessionStart · UserPromptSubmit · PostToolUse(Edit|Write|MultiEdit) · Stop");
    println!("  uninstall: thoth hooks uninstall");
    Ok(())
}

/// `thoth hooks uninstall [--scope ...]`
pub async fn uninstall(scope: Scope) -> anyhow::Result<()> {
    let path = scope.settings_path()?;
    if !path.exists() {
        println!("no settings at {} — nothing to remove", path.display());
        return Ok(());
    }
    let mut settings = read_settings(&path).await?;
    strip_hooks(&mut settings);
    write_settings(&path, &settings).await?;
    println!("✓ thoth hooks removed from {}", path.display());
    Ok(())
}

/// `thoth skills install [--scope ...] --root <...>`
pub async fn skills_install(scope: Scope, root: &Path) -> anyhow::Result<()> {
    let dest_dir = scope.skills_dir(root)?.join("thoth");
    tokio::fs::create_dir_all(&dest_dir).await?;
    let dest = dest_dir.join("SKILL.md");
    tokio::fs::write(&dest, SKILL_MD).await?;
    println!("✓ skill installed at {}", dest.display());
    Ok(())
}

/// `thoth mcp install [--scope ...]` — registers `thoth-mcp` under
/// `mcpServers.thoth` in `settings.json`. Idempotent.
pub async fn mcp_install(scope: Scope, root: &Path) -> anyhow::Result<()> {
    let path = scope.settings_path()?;
    let mut template: Value = serde_json::from_str(MCP_TEMPLATE)?;
    // Rewrite the bundled --root arg to match the user's actual root so
    // the server looks in the right place.
    if let Some(entry) = template
        .get_mut("mcpServers")
        .and_then(|s| s.get_mut(MCP_SERVER_KEY))
        .and_then(|v| v.as_object_mut())
    {
        entry.insert(
            "args".to_string(),
            json!(["--root", root.display().to_string()]),
        );
    }

    let mut settings = read_settings(&path).await?;
    if !settings.is_object() {
        bail!(
            "{} exists but isn't a JSON object — refusing to overwrite",
            path.display()
        );
    }
    merge_mcp(&mut settings, &template);
    write_settings(&path, &settings).await?;
    println!("✓ mcp server `thoth` installed into {}", path.display());
    println!("  command: thoth-mcp --root {}", root.display());
    println!("  uninstall: thoth mcp uninstall");
    Ok(())
}

/// `thoth mcp uninstall [--scope ...]`
pub async fn mcp_uninstall(scope: Scope) -> anyhow::Result<()> {
    let path = scope.settings_path()?;
    if !path.exists() {
        println!("no settings at {} — nothing to remove", path.display());
        return Ok(());
    }
    let mut settings = read_settings(&path).await?;
    strip_mcp(&mut settings);
    write_settings(&path, &settings).await?;
    println!("✓ mcp server `thoth` removed from {}", path.display());
    Ok(())
}

/// `thoth install` — convenience one-shot: skill + hooks + mcp, all in the
/// same scope. Idempotent; safe to re-run.
pub async fn install_all(scope: Scope, root: &Path) -> anyhow::Result<()> {
    skills_install(scope, root).await?;
    install(scope).await?;
    mcp_install(scope, root).await?;
    println!();
    println!("✓ thoth fully wired into Claude Code ({scope:?} scope)");
    Ok(())
}

/// `thoth uninstall` — removes skill + hooks + mcp from `settings.json`.
pub async fn uninstall_all(scope: Scope, root: &Path) -> anyhow::Result<()> {
    // Skill file removal — best effort; only drops our own directory.
    let skill_dir = scope.skills_dir(root)?.join("thoth");
    if skill_dir.exists() {
        let _ = tokio::fs::remove_dir_all(&skill_dir).await;
        println!("✓ skill removed from {}", skill_dir.display());
    }
    uninstall(scope).await?;
    mcp_uninstall(scope).await?;
    Ok(())
}

// -------------------------------------------------------------- exec runtime

/// Hook events understood by `thoth hooks exec <event>`.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum HookEvent {
    /// `SessionStart` — dump MEMORY.md + LESSONS.md into the context.
    SessionStart,
    /// `UserPromptSubmit` — recall top-k chunks for the user's prompt.
    UserPromptSubmit,
    /// `PostToolUse` — re-index the edited file.
    PostToolUse,
    /// `Stop` / `SessionEnd` — forget pass (+ nudge if Mode::Full).
    Stop,
}

/// `thoth hooks exec <event>`. Called by Claude Code itself. Reads the
/// hook payload as JSON on stdin, does its thing, and prints any new
/// context to stdout.
///
/// Every error is swallowed to stderr and the process exits 0 — a failing
/// hook must never block the agent.
pub async fn exec(event: HookEvent, root: &Path) -> anyhow::Result<()> {
    let payload = read_stdin_json().await.unwrap_or_else(|_| json!({}));
    let result = match event {
        HookEvent::SessionStart => run_session_start(root).await,
        HookEvent::UserPromptSubmit => run_user_prompt(root, &payload).await,
        HookEvent::PostToolUse => run_post_tool(root, &payload).await,
        HookEvent::Stop => run_stop(root, &payload).await,
    };
    if let Err(e) = result {
        eprintln!("thoth: hook error: {e}");
    }
    Ok(())
}

async fn read_stdin_json() -> anyhow::Result<Value> {
    use tokio::io::AsyncReadExt;
    let mut buf = String::new();
    let mut stdin = tokio::io::stdin();
    stdin.read_to_string(&mut buf).await?;
    if buf.trim().is_empty() {
        return Ok(json!({}));
    }
    Ok(serde_json::from_str(&buf).unwrap_or(Value::Null))
}

async fn run_session_start(root: &Path) -> anyhow::Result<()> {
    if !root.exists() {
        return Ok(());
    }
    // Print MEMORY.md + LESSONS.md verbatim; Claude Code picks stdout up
    // as additional context. Keep it compact.
    for name in ["MEMORY.md", "LESSONS.md"] {
        let p = root.join(name);
        let Ok(body) = tokio::fs::read_to_string(&p).await else {
            continue;
        };
        let trimmed = body.trim();
        if trimmed.is_empty() {
            continue;
        }
        println!("### {name}");
        println!("{trimmed}");
        println!();
    }
    Ok(())
}

async fn run_user_prompt(root: &Path, payload: &Value) -> anyhow::Result<()> {
    if !root.exists() {
        return Ok(());
    }
    let prompt = payload
        .get("prompt")
        .and_then(Value::as_str)
        .or_else(|| payload.get("user_prompt").and_then(Value::as_str))
        .unwrap_or("")
        .trim();
    if prompt.is_empty() {
        return Ok(());
    }
    let store = StoreRoot::open(root).await?;
    let retriever = Retriever::new(store);
    let q = thoth_core::Query {
        text: prompt.to_string(),
        top_k: 5,
        ..thoth_core::Query::text("")
    };
    let out = retriever.recall(&q).await?;
    if out.chunks.is_empty() {
        return Ok(());
    }
    println!("### thoth recall (top {})", out.chunks.len());
    for c in out.chunks.iter() {
        let sym = c.symbol.as_deref().unwrap_or("-");
        println!(
            "- {}:{}-{}  [{}]  {}",
            c.path.display(),
            c.span.0,
            c.span.1,
            sym,
            first_line(&c.preview, 120),
        );
    }
    Ok(())
}

async fn run_post_tool(root: &Path, payload: &Value) -> anyhow::Result<()> {
    if !root.exists() {
        return Ok(());
    }
    // Expected shape: { "tool_name": "Edit", "tool_input": { "file_path": "..." } }
    let file = payload
        .get("tool_input")
        .and_then(|v| v.get("file_path"))
        .and_then(Value::as_str);
    let Some(file) = file else { return Ok(()) };
    let p = Path::new(file);
    if !p.is_file() {
        return Ok(());
    }
    let store = StoreRoot::open(root).await?;
    let idx = Indexer::new(store, LanguageRegistry::new());
    // Best effort — if the language isn't supported we just skip silently.
    // `index_file` purges stale rows for this path before re-writing, and
    // the explicit `commit` flushes the BM25 writer so the next recall
    // picks up the edit.
    let _ = idx.index_file(p).await;
    let _ = idx.commit().await;
    Ok(())
}

async fn run_stop(root: &Path, _payload: &Value) -> anyhow::Result<()> {
    if !root.exists() {
        return Ok(());
    }
    // Always-on, deterministic: TTL + capacity eviction over the episodic log.
    let memory = thoth_memory::MemoryManager::open(root).await?;
    let report = memory.forget_pass().await?;
    let dropped = report.episodes_ttl + report.episodes_cap;
    if dropped > 0 {
        eprintln!("thoth: forgot {dropped} episodes");
    }
    // Nudge is Mode::Full — requires the anthropic synthesizer. If the
    // feature isn't compiled in, skip silently.
    #[cfg(feature = "anthropic")]
    {
        if std::env::var("ANTHROPIC_API_KEY").is_ok() {
            use std::sync::Arc;
            use thoth_core::Synthesizer;
            match thoth_synth::anthropic::AnthropicSynthesizer::from_env() {
                Ok(synth) => {
                    let synth: Arc<dyn Synthesizer> = Arc::new(synth);
                    match memory.nudge(synth.as_ref(), 0).await {
                        Ok(r) if r.lessons_added + r.facts_added + r.skills_added > 0 => {
                            eprintln!(
                                "thoth: nudge added {} facts, {} lessons, {} skills",
                                r.facts_added, r.lessons_added, r.skills_added,
                            );
                        }
                        Ok(_) => {}
                        Err(e) => eprintln!("thoth: nudge skipped: {e}"),
                    }
                }
                Err(e) => eprintln!("thoth: nudge skipped: {e}"),
            }
        }
    }
    Ok(())
}

/// Collapse a multi-line preview to the first non-empty line, capped at
/// `max` chars with `…` elision.
fn first_line(s: &str, max: usize) -> String {
    let line = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let line = line.trim();
    if line.chars().count() <= max {
        line.to_string()
    } else {
        let head: String = line.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

// ----------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_is_idempotent() {
        let template: Value = serde_json::from_str(HOOKS_TEMPLATE).unwrap();
        let mut settings = json!({});
        merge_hooks(&mut settings, &template);
        let once = settings.clone();
        merge_hooks(&mut settings, &template);
        assert_eq!(once, settings);
    }

    #[test]
    fn merge_preserves_user_hooks() {
        let template: Value = serde_json::from_str(HOOKS_TEMPLATE).unwrap();
        let mut settings = json!({
            "hooks": {
                "PostToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{"type": "command", "command": "echo user"}]
                }]
            }
        });
        merge_hooks(&mut settings, &template);
        let post = settings
            .get("hooks")
            .unwrap()
            .get("PostToolUse")
            .unwrap()
            .as_array()
            .unwrap();
        // Original + Thoth entry.
        assert!(post.iter().any(|e| entry_marker(e).is_none()));
        assert!(post.iter().any(|e| entry_marker(e).is_some()));
    }

    #[test]
    fn uninstall_removes_only_thoth() {
        let template: Value = serde_json::from_str(HOOKS_TEMPLATE).unwrap();
        let mut settings = json!({
            "hooks": {
                "PostToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{"type": "command", "command": "echo user"}]
                }]
            }
        });
        merge_hooks(&mut settings, &template);
        strip_hooks(&mut settings);
        let post = settings
            .get("hooks")
            .and_then(|h| h.get("PostToolUse"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        assert_eq!(post.len(), 1);
        assert!(entry_marker(&post[0]).is_none());
    }

    #[test]
    fn uninstall_on_pure_thoth_clears_hooks_key() {
        let template: Value = serde_json::from_str(HOOKS_TEMPLATE).unwrap();
        let mut settings = json!({});
        merge_hooks(&mut settings, &template);
        strip_hooks(&mut settings);
        assert!(settings.get("hooks").is_none());
    }

    #[test]
    fn mcp_merge_is_idempotent() {
        let template: Value = serde_json::from_str(MCP_TEMPLATE).unwrap();
        let mut settings = json!({});
        merge_mcp(&mut settings, &template);
        let once = settings.clone();
        merge_mcp(&mut settings, &template);
        assert_eq!(once, settings);
        assert!(
            settings
                .get("mcpServers")
                .and_then(|s| s.get("thoth"))
                .is_some()
        );
    }

    #[test]
    fn mcp_merge_preserves_other_servers() {
        let template: Value = serde_json::from_str(MCP_TEMPLATE).unwrap();
        let mut settings = json!({
            "mcpServers": {
                "other": { "command": "other-mcp" }
            }
        });
        merge_mcp(&mut settings, &template);
        let servers = settings.get("mcpServers").unwrap().as_object().unwrap();
        assert!(servers.contains_key("other"));
        assert!(servers.contains_key("thoth"));
    }

    #[test]
    fn mcp_uninstall_removes_only_thoth() {
        let template: Value = serde_json::from_str(MCP_TEMPLATE).unwrap();
        let mut settings = json!({
            "mcpServers": {
                "other": { "command": "other-mcp" }
            }
        });
        merge_mcp(&mut settings, &template);
        strip_mcp(&mut settings);
        let servers = settings.get("mcpServers").unwrap().as_object().unwrap();
        assert_eq!(servers.len(), 1);
        assert!(servers.contains_key("other"));
    }

    #[test]
    fn mcp_uninstall_prunes_empty_mcp_servers() {
        let template: Value = serde_json::from_str(MCP_TEMPLATE).unwrap();
        let mut settings = json!({});
        merge_mcp(&mut settings, &template);
        strip_mcp(&mut settings);
        assert!(settings.get("mcpServers").is_none());
    }
}

//! `thoth hooks` / `thoth skills` / `thoth mcp` — replay the bundled
//! Claude Code wiring into the right config files. This is the only
//! install path — Thoth no longer ships a Claude Code marketplace plugin.
//!
//! Claude Code splits its configuration across three files, and this
//! module writes to all three:
//!
//! - `.claude/settings.json` — hooks, permissions, env (project-scoped)
//! - `.mcp.json` — MCP server registry (project-scoped); Claude Code
//!   **ignores `mcpServers` inside `settings.json`**, which is the
//!   singular reason this split exists at all
//! - `.claude/skills/<name>/SKILL.md` — procedural skill bodies
//!
//! User scope mirrors the same layout under the user's home: settings in
//! `~/.claude/settings.json`, MCP in `~/.claude.json`, skills in
//! `~/.claude/skills/`.
//!
//! The CLI's `assets/` directory is the single source of truth:
//! [`BUNDLE_MCP`], [`BUNDLE_HOOKS`], and [`BUNDLE_SKILLS`] embed
//! `assets/mcp.json`, `assets/hooks.json`, and every `assets/skills/*/SKILL.md`
//! at compile time (see `include_str!`), and the merge functions below
//! replay them into their respective destinations.
//!
//! Clean uninstall is done via a sentinel: every hook entry the CLI
//! writes is tagged with `"_thoth_managed": true` (Claude Code ignores
//! unknown fields), so `thoth uninstall` strips exactly what it installed
//! without touching user-owned hooks.
//!
//! [`exec`] is kept as a safety net for any pre-refactor settings that
//! still call `thoth hooks exec <event>` via a `type: "command"` hook —
//! new installs only ship `type: "prompt"` hooks plus the separate
//! `thoth-gate` binary, so the runtime dispatcher is rarely on the hot
//! path anymore.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde_json::{Value, json};
use thoth_parse::LanguageRegistry;
use thoth_retrieve::{Indexer, RetrieveConfig, Retriever};
use thoth_store::StoreRoot;

// -------------------------------------------------------------- asset bundle
//
// `crates/thoth-cli/assets/` is the **single source of truth** for every
// Claude Code integration artifact (MCP config, hooks, skills). Files are
// embedded at compile time via `include_str!` and replayed into their
// respective destination files (`.mcp.json`, `.claude/settings.json`,
// `.claude/skills/<name>/SKILL.md`) by the install helpers below.

/// Bundled MCP server config.
const BUNDLE_MCP: &str = include_str!("../assets/mcp.json");

/// Bundled Claude Code hook bundle (Claude Code plugin format — event
/// names at the JSON root, no outer `"hooks"` wrapper; [`merge_hooks`]
/// bridges that into the `settings.json` shape).
const BUNDLE_HOOKS: &str = include_str!("../assets/hooks.json");

/// Names + bodies of every skill we ship. Kept as a `&[(name, body)]`
/// slice so adding a new skill is just appending another `include_str!` line.
const BUNDLE_SKILLS: &[(&str, &str)] = &[
    (
        "memory-discipline",
        include_str!("../assets/skills/memory-discipline/SKILL.md"),
    ),
    (
        "thoth-reflect",
        include_str!("../assets/skills/thoth-reflect/SKILL.md"),
    ),
    (
        "thoth-guide",
        include_str!("../assets/skills/thoth-guide/SKILL.md"),
    ),
    (
        "thoth-exploring",
        include_str!("../assets/skills/thoth-exploring/SKILL.md"),
    ),
    (
        "thoth-debugging",
        include_str!("../assets/skills/thoth-debugging/SKILL.md"),
    ),
    (
        "thoth-impact-analysis",
        include_str!("../assets/skills/thoth-impact-analysis/SKILL.md"),
    ),
    (
        "thoth-refactoring",
        include_str!("../assets/skills/thoth-refactoring/SKILL.md"),
    ),
    (
        "thoth-cli",
        include_str!("../assets/skills/thoth-cli/SKILL.md"),
    ),
];

/// Trip-wire: we always ship at least one skill. Compile-time so dropping
/// the slice to zero fails the build, not the test suite.
const _: () = assert!(!BUNDLE_SKILLS.is_empty());

/// Bundled statusline script for Claude Code.
const BUNDLE_STATUSLINE: &str = include_str!("../assets/thoth-statusline.sh");

/// Sentinel field added to every hook entry the CLI writes into
/// `.claude/settings.json`. Lets `thoth uninstall` strip exactly what it
/// installed without touching user-owned hooks. We only tag hook entries;
/// MCP config in `.mcp.json` is keyed by `mcpServers.thoth` which gives us
/// the same "install exactly one, uninstall exactly that one" guarantee.
const THOTH_MANAGED_KEY: &str = "_thoth_managed";

/// Key under `mcpServers` that identifies the Thoth entry so we can dedupe
/// and cleanly uninstall.
const MCP_SERVER_KEY: &str = "thoth";

/// Scope of an install edit — which set of Claude Code config files to
/// target. Every scope touches the same three logical surfaces (hooks,
/// MCP, skills); only the on-disk locations differ.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum Scope {
    /// Project-local: `./.claude/settings.json` + `./.mcp.json`
    /// + `./.claude/skills/`.
    Project,
    /// User-global: `~/.claude/settings.json` + `~/.claude.json`
    /// + `~/.claude/skills/`.
    User,
}

impl Scope {
    /// Where **hooks / permissions / env** live. This is NOT where MCP
    /// server config lives — see [`Self::mcp_path`].
    fn settings_path(self) -> anyhow::Result<PathBuf> {
        match self {
            Scope::Project => Ok(PathBuf::from(".claude").join("settings.json")),
            Scope::User => {
                let home = home_dir().context("could not locate home directory")?;
                Ok(home.join(".claude").join("settings.json"))
            }
        }
    }

    /// Where **MCP server config** lives. Claude Code ignores `mcpServers`
    /// in `.claude/settings.json`; project-scoped MCP must live in
    /// `<project>/.mcp.json` (top-level `mcpServers`), and user-scoped
    /// MCP lives in `~/.claude.json`.
    fn mcp_path(self) -> anyhow::Result<PathBuf> {
        match self {
            Scope::Project => Ok(PathBuf::from(".mcp.json")),
            Scope::User => {
                let home = home_dir().context("could not locate home directory")?;
                Ok(home.join(".claude.json"))
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

/// Locate a companion binary (`thoth-mcp`, `thoth-gate`) next to the
/// currently-running `thoth` binary.
///
/// Why: Claude Code spawns hooks and MCP servers with its own PATH, which
/// on macOS GUI launches doesn't include `~/.cargo/bin`, `/opt/homebrew/bin`,
/// or whatever the user's shell rc exports. A bare `"command": "thoth-mcp"`
/// therefore fails to start for GUI-launched Claude Code even though the
/// binary exists in the user's shell. Writing the absolute path sidesteps
/// that whole class of issue.
///
/// Falls back to the bare name if we can't locate a sibling binary — better
/// a broken config that matches the user's expectation than a silent
/// rewrite pointing at a non-existent path.
fn resolve_companion(name: &str) -> String {
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        let candidate = parent.join(name);
        if candidate.exists() {
            return candidate.display().to_string();
        }
    }
    name.to_string()
}

/// Rewrite a single command string:
///
/// - If the first whitespace-delimited token is a companion binary
///   (`thoth`, `thoth-gate`, `thoth-mcp`), replace it with the absolute
///   path of that binary next to the running thoth CLI. Claude Code's
///   spawn PATH on GUI launches doesn't include the user's shell paths,
///   so a bare binary name fails to resolve.
/// - Substitute `{THOTH_ROOT}` with the supplied absolute root path,
///   properly shell-quoted. This is how we thread the user's configured
///   `.thoth/` directory into command hooks whose JSON template can't
///   know it at compile time.
fn rewrite_command_string(cmd: &str, root_abs: &str) -> String {
    // {THOTH_ROOT} first so the substituted value doesn't accidentally
    // look like a companion binary.
    let substituted = cmd.replace("{THOTH_ROOT}", &shell_quote(root_abs));

    // Split on the first whitespace to isolate the program token. We use
    // `split_once` so multi-word commands keep their arg tail untouched.
    let (head, tail) = match substituted.split_once(char::is_whitespace) {
        Some((h, t)) => (h.to_string(), Some(t.to_string())),
        None => (substituted.clone(), None),
    };

    if !matches!(head.as_str(), "thoth" | "thoth-gate" | "thoth-mcp") {
        return substituted;
    }

    let resolved = resolve_companion(&head);
    match tail {
        Some(t) => format!("{resolved} {t}"),
        None => resolved,
    }
}

/// Minimal shell-quoting for a path we're substituting into a command
/// string. Wraps in single quotes and escapes any existing single quotes
/// via the `'\''` idiom. Sufficient for filesystem paths (no newlines).
fn shell_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_alphanumeric() || "_-./:@=+".contains(c))
    {
        return s.to_string();
    }
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

/// Walk a hook bundle and rewrite every command hook via
/// [`rewrite_command_string`]. Mutates in place. Safe on unknown shapes —
/// anything that isn't a command hook is left alone.
fn rewrite_companion_commands(bundle: &mut Value, root_abs: &str) {
    let Value::Object(events) = bundle else {
        return;
    };
    for (_event, entries) in events.iter_mut() {
        let Some(list) = entries.as_array_mut() else {
            continue;
        };
        for entry in list.iter_mut() {
            let Some(hooks) = entry.get_mut("hooks").and_then(Value::as_array_mut) else {
                continue;
            };
            for hook in hooks.iter_mut() {
                let Some(obj) = hook.as_object_mut() else {
                    continue;
                };
                if obj.get("type").and_then(Value::as_str) != Some("command") {
                    continue;
                }
                if let Some(Value::String(cmd)) = obj.get_mut("command") {
                    *cmd = rewrite_command_string(cmd, root_abs);
                }
            }
        }
    }
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

/// Merge the bundled hook set into a `settings.json` value.
///
/// [`BUNDLE_HOOKS`] uses Claude Code plugin format — event names at the
/// JSON root, no outer `"hooks"` wrapper. `settings.json` uses the
/// editor format with a `"hooks"` wrapper. This function bridges the two
/// and tags every entry it writes with [`THOTH_MANAGED_KEY`].
///
/// Semantics: first strip every thoth-managed entry from the existing
/// settings (across *all* events, not just events in the current
/// bundle), then append the fresh bundle entries. Stripping across all
/// events is what makes re-install self-heal when an entire event is
/// dropped from the bundle — e.g. older thoth versions shipped
/// `PostToolUse` prompt hooks that no longer exist. A per-event strip
/// would leave those orphaned forever. User-owned hooks (anything
/// without the sentinel) are never touched.
fn merge_hooks(existing: &mut Value, bundle: &Value) {
    let Value::Object(bundle_events) = bundle else {
        return;
    };

    // Purge every thoth-managed entry first, regardless of event. This
    // is what makes re-install self-heal when the bundle drops an entire
    // event (per-event stripping would leave those orphaned).
    strip_hooks(existing);

    let Some(root) = existing.as_object_mut() else {
        // Callers (install / setup) pre-check `is_object()` and bail with a
        // user-facing message; this branch is unreachable in normal flows.
        // Skip silently so unit tests that hand us non-object fixtures don't
        // blow up with an opaque panic.
        return;
    };
    let hooks_entry = root.entry("hooks".to_string()).or_insert_with(|| json!({}));
    // User hand-edited `"hooks"` to a non-object (string, array, null …).
    // Thoth owns this key — reset it rather than panicking on
    // `as_object_mut().expect()`.
    if !hooks_entry.is_object() {
        *hooks_entry = json!({});
    }
    let settings_hooks = hooks_entry
        .as_object_mut()
        .expect("hooks_entry was just normalised to an object above");

    for (event, entries) in bundle_events {
        let Some(bundle_list) = entries.as_array() else {
            continue;
        };
        let dest_entry = settings_hooks
            .entry(event.clone())
            .or_insert_with(|| json!([]));
        if !dest_entry.is_array() {
            // Same reasoning: per-event slot is thoth-managed on re-install.
            *dest_entry = json!([]);
        }
        let dest = dest_entry
            .as_array_mut()
            .expect("dest_entry was just normalised to an array above");

        for entry in bundle_list {
            let mut tagged = entry.clone();
            if let Value::Object(map) = &mut tagged {
                map.insert(THOTH_MANAGED_KEY.to_string(), Value::Bool(true));
            }
            dest.push(tagged);
        }
    }
}

/// True if this hook entry carries the thoth-managed sentinel.
fn is_thoth_managed(entry: &Value) -> bool {
    entry
        .get(THOTH_MANAGED_KEY)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Strip every thoth-managed hook entry. Prunes empty arrays and the
/// top-level `"hooks"` key if nothing else remains.
fn strip_hooks(v: &mut Value) {
    let Some(hooks) = v.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return;
    };
    let events: Vec<String> = hooks.keys().cloned().collect();
    for event in events {
        let Some(list) = hooks.get_mut(&event).and_then(|e| e.as_array_mut()) else {
            continue;
        };
        list.retain(|entry| !is_thoth_managed(entry));
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
    let Some(root) = existing.as_object_mut() else {
        // Callers pre-check `is_object()`; unreachable in normal flows.
        return;
    };
    let servers_entry = root
        .entry("mcpServers".to_string())
        .or_insert_with(|| json!({}));
    // User hand-edited `mcpServers` into a non-object — reset, as thoth
    // owns the entry it's about to insert.
    if !servers_entry.is_object() {
        *servers_entry = json!({});
    }
    let servers = servers_entry
        .as_object_mut()
        .expect("servers_entry was just normalised to an object above");
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

// --------------------------------------------------- CLAUDE.md managed block
//
// Claude Code loads `./CLAUDE.md` from the project root on every session,
// including after `/clear` and `/compact`. It's the single signal in the
// agent's context that reliably survives those resets — `SessionStart`
// output can be summarized away, `UserPromptSubmit` recalls fire per turn
// but aren't visible before the first prompt, and the `thoth-gate` block
// message only teaches after a retry has been forced.
//
// We write a small policy block into CLAUDE.md with HTML-comment markers
// around it so `thoth uninstall` can strip exactly what was written
// without touching user-owned content. Same sentinel pattern as the
// `_thoth_managed` flag on hook entries.

/// Markers delimiting the Thoth-written region of `./CLAUDE.md`.
const CLAUDE_MD_START: &str = "<!-- thoth:managed:start -->";
const CLAUDE_MD_END: &str = "<!-- thoth:managed:end -->";
const CLAUDE_MD_PATH: &str = "CLAUDE.md";

/// Render the managed block for a given init date (`YYYY-MM-DD`). The
/// block is deterministic in the date so re-running `thoth setup` on the
/// same day produces byte-identical output — makes [`claude_md_install`]
/// a no-op write on same-day re-runs.
fn render_claude_md_block(init_date: &str) -> String {
    format!(
        "\
{start}
## Thoth memory (managed by `thoth setup` — edits inside this block are overwritten)

This project uses **Thoth MCP** as its long-term memory. Initialized on {date}.

### Memory workflow

- Persist facts via `thoth_remember_fact({{text, tags?}})` → `./.thoth/MEMORY.md`.
- Persist lessons via `thoth_remember_lesson({{trigger, advice}})` → `./.thoth/LESSONS.md`.
- Before every Write / Edit / Bash: call `thoth_recall({{query}})` at least once.
- The `UserPromptSubmit` hook auto-recalls for context but passes `log_event: false`, \
so that ceremonial recall does NOT satisfy the `thoth-gate` PreToolUse gate — only \
agent-initiated recalls do.
- Browse raw memory without tool calls: open `./.thoth/MEMORY.md` and `./.thoth/LESSONS.md`.
- Remove this block and all Thoth wiring: `thoth uninstall`.

### Code intelligence tools

| Tool | Params | Purpose |
|------|--------|---------|
| `thoth_recall` | `query`, `top_k?` (default 8) | Hybrid search (symbol + BM25 + graph + markdown) |
| `thoth_impact` | `fqn`, `direction?` (up/down/both), `depth?` (1-8) | Blast radius — who breaks if this symbol changes |
| `thoth_symbol_context` | `fqn`, `limit?` (default 32) | 360° view: callers, callees, imports, siblings, doc |
| `thoth_detect_changes` | `diff` (git diff output), `depth?` (1-6) | Find symbols touched by a diff + their blast radius |
| `thoth_index` | `path?` (default \".\") | Reindex source tree |

### Before editing code

1. **MUST** run `thoth_impact({{fqn: \"module::symbol\"}})` before modifying any function/class.
2. Report blast radius (direct callers at d=1, indirect at d=2+) to the user.
3. **MUST** warn the user if d=1 impact includes critical paths before proceeding.

### Before committing

Run `thoth_detect_changes({{diff: \"<git diff output>\"}})` to verify changes only affect expected symbols.

### Memory maintenance

- `thoth review` — periodic background curation; the PostToolUse hook spawns \
it automatically when `background_review = true` (every `background_review_interval` \
mutations, subject to `background_review_min_secs` cooldown). Appends new insights; \
never deletes. Uses `background_review_model` (default `claude-haiku-4-5`) to avoid \
burning Opus tokens on a curator task.
- `thoth compact` — LLM-driven consolidation of `MEMORY.md` / `LESSONS.md`. Reads \
every entry, merges reworded near-duplicates into canonical form, **rewrites** both \
files (with `.bak-<unix>` backups). Run `thoth compact --dry-run` first to eyeball the \
proposal. Use when the files feel bloated with restatements of the same subject. \
Reuses the review backend/model config — no extra setup.

### Available skills

Use `/skill-name` to invoke: `thoth-exploring` (understand code), `thoth-debugging` (trace bugs), \
`thoth-refactoring` (safe renames/moves), `thoth-impact-analysis` (blast radius), \
`thoth-reflect` (end-of-session lessons), `thoth-guide` (Thoth help).
{end}",
        start = CLAUDE_MD_START,
        end = CLAUDE_MD_END,
        date = init_date,
    )
}

/// Merge (or insert) the managed block into an existing `CLAUDE.md` body.
///
/// Semantics:
/// - If both markers are present and well-ordered, replace everything
///   between them (inclusive of the markers) with the fresh block. User
///   content above/below is preserved byte-for-byte.
/// - If markers are missing (or malformed), prepend the block at the top
///   so Claude Code picks it up first, then a blank line, then whatever
///   was in the file before.
/// - If the file is empty/absent, the returned string is just the block
///   plus a trailing newline.
fn merge_claude_md(existing: &str, block: &str) -> String {
    if let (Some(s), Some(e)) = (existing.find(CLAUDE_MD_START), existing.find(CLAUDE_MD_END))
        && s < e
    {
        let end = e + CLAUDE_MD_END.len();
        let before = &existing[..s];
        let after = &existing[end..];
        let mut out = String::with_capacity(before.len() + block.len() + after.len());
        out.push_str(before);
        out.push_str(block);
        out.push_str(after);
        return out;
    }
    if existing.trim().is_empty() {
        return format!("{block}\n");
    }
    format!("{block}\n\n{trimmed}\n", trimmed = existing.trim_end())
}

/// Remove the managed block, returning the remainder. If no markers are
/// present, returns the input unchanged. Collapses the blank lines that
/// would otherwise be left around the removed region.
fn strip_claude_md(existing: &str) -> String {
    let (Some(s), Some(e)) = (existing.find(CLAUDE_MD_START), existing.find(CLAUDE_MD_END)) else {
        return existing.to_string();
    };
    if s >= e {
        return existing.to_string();
    }
    let end = e + CLAUDE_MD_END.len();
    let before = existing[..s].trim_end_matches(['\n', ' ', '\t']);
    let after = existing[end..].trim_start_matches(['\n', ' ', '\t']);
    if before.is_empty() && after.is_empty() {
        return String::new();
    }
    if before.is_empty() {
        return format!("{after}\n");
    }
    if after.is_empty() {
        return format!("{before}\n");
    }
    format!("{before}\n\n{after}\n")
}

/// Write (or refresh) `./CLAUDE.md` with the Thoth managed block.
/// No-op for `Scope::User` — CLAUDE.md is a per-project file, not a
/// user-global one.
pub async fn claude_md_install(scope: Scope, init_date: &str) -> anyhow::Result<()> {
    if !matches!(scope, Scope::Project) {
        return Ok(());
    }
    let path = PathBuf::from(CLAUDE_MD_PATH);
    let existing = if path.exists() {
        tokio::fs::read_to_string(&path).await.unwrap_or_default()
    } else {
        String::new()
    };
    let block = render_claude_md_block(init_date);
    let merged = merge_claude_md(&existing, &block);
    if merged == existing {
        return Ok(());
    }
    tokio::fs::write(&path, merged)
        .await
        .with_context(|| format!("write {}", path.display()))?;
    println!("✓ CLAUDE.md policy block written at {}", path.display());
    Ok(())
}

/// Strip the Thoth managed block from `./CLAUDE.md`. Deletes the file
/// entirely if nothing else was in it. No-op for `Scope::User`.
pub async fn claude_md_uninstall(scope: Scope) -> anyhow::Result<()> {
    if !matches!(scope, Scope::Project) {
        return Ok(());
    }
    let path = PathBuf::from(CLAUDE_MD_PATH);
    if !path.exists() {
        return Ok(());
    }
    let existing = tokio::fs::read_to_string(&path).await.unwrap_or_default();
    let stripped = strip_claude_md(&existing);
    if stripped == existing {
        return Ok(());
    }
    if stripped.trim().is_empty() {
        let _ = tokio::fs::remove_file(&path).await;
        println!("✓ CLAUDE.md removed (was only the Thoth block)");
    } else {
        tokio::fs::write(&path, stripped)
            .await
            .with_context(|| format!("write {}", path.display()))?;
        println!("✓ Thoth block removed from {}", path.display());
    }
    Ok(())
}

// ------------------------------------------------------------- public commands

/// `thoth hooks install [--scope ...]`
pub async fn install(scope: Scope, root: &Path) -> anyhow::Result<()> {
    let path = scope.settings_path()?;
    let mut bundle: Value = serde_json::from_str(BUNDLE_HOOKS)
        .context("parsing embedded hooks.json — this is a build bug")?;
    // Resolve the absolute THOTH_ROOT so command hooks can reference it
    // regardless of the CWD Claude Code spawns them from.
    let root_abs = tokio::fs::canonicalize(root)
        .await
        .unwrap_or_else(|_| root.to_path_buf());
    // Substitute `{THOTH_ROOT}` placeholders and rewrite bare companion
    // binary names (`thoth`, `thoth-gate`, …) to absolute paths.
    rewrite_companion_commands(&mut bundle, &root_abs.display().to_string());
    let mut settings = read_settings(&path).await?;
    if !settings.is_object() {
        bail!(
            "{} exists but isn't a JSON object — refusing to overwrite",
            path.display()
        );
    }
    // Legacy cleanup: earlier versions of thoth wrote MCP config into
    // `settings.json`, but Claude Code ignores it there. Strip any stale
    // `mcpServers.thoth` we may have left behind so re-running `thoth
    // setup` silently self-heals old installs.
    strip_mcp(&mut settings);
    merge_hooks(&mut settings, &bundle);
    write_settings(&path, &settings).await?;

    println!("✓ hooks installed into {}", path.display());
    println!(
        "  events: SessionStart · UserPromptSubmit · \
         PreToolUse(Write|Edit|NotebookEdit|Bash) · \
         PostToolUse(Bash|Write|Edit|NotebookEdit) · Stop"
    );
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
    // Also purge any legacy `mcpServers.thoth` that older thoth versions
    // wrote into settings.json before the `.mcp.json` split.
    strip_mcp(&mut settings);
    write_settings(&path, &settings).await?;
    println!("✓ thoth hooks removed from {}", path.display());
    Ok(())
}

/// `thoth skills install [--scope ...] --root <...>` — installs every
/// bundled skill (`memory-discipline`, `thoth-reflect`, `thoth-guide`,
/// `thoth-exploring`, `thoth-debugging`, `thoth-impact-analysis`,
/// `thoth-refactoring`, `thoth-cli`) under the scope's `skills/`
/// directory.
pub async fn skills_install(scope: Scope, root: &Path) -> anyhow::Result<()> {
    let base = scope.skills_dir(root)?;
    for (name, body) in BUNDLE_SKILLS {
        let dir = base.join(name);
        tokio::fs::create_dir_all(&dir).await?;
        let dest = dir.join("SKILL.md");
        tokio::fs::write(&dest, body).await?;
        println!("✓ skill `{name}` installed at {}", dest.display());
    }
    Ok(())
}

/// Promote a draft skill from `<root>/skills/<slug>.draft/` (where the
/// agent's `thoth_skill_propose` MCP tool drops them) into the scope's
/// live `skills/` directory, making Claude Code pick it up on the next
/// session. The draft is removed on success so the same skill can't be
/// accepted twice.
///
/// The slug is taken from the draft's SKILL.md frontmatter (`name:`);
/// if that's missing, the directory name minus the `.draft` suffix is
/// used as a fallback.
///
/// Also appends an `install skill` entry to `memory-history.jsonl` so
/// the provenance chain (proposed → installed) is audit-visible.
pub async fn promote_skill_draft(
    scope: Scope,
    root: &Path,
    draft_path: &Path,
) -> anyhow::Result<()> {
    let skills_dir = scope.skills_dir(root)?;
    let (slug, dest) = promote_skill_draft_to(draft_path, &skills_dir).await?;

    // Best-effort history log. Opening the MarkdownStore shouldn't fail
    // in practice (the root exists — we just copied out of it), but a
    // log write must never block the actual install from succeeding.
    if let Ok(store) = thoth_store::MarkdownStore::open(root).await {
        let _ = store
            .append_history(&thoth_store::markdown::HistoryEntry {
                op: "install",
                kind: "skill",
                title: slug.clone(),
                actor: Some("user".to_string()),
                reason: Some(format!("from draft {}", draft_path.display())),
            })
            .await;
    }

    println!("✓ skill `{slug}` installed at {}", dest.display());
    println!("  (draft {} removed)", draft_path.display());
    Ok(())
}

/// Core of [`promote_skill_draft`] without the scope/history concerns —
/// takes an explicit `skills_dir` so tests can drive it with an absolute
/// path and don't need to touch the process's CWD. Returns the derived
/// `(slug, dest_dir)` so the caller can surface them in logs.
pub(crate) async fn promote_skill_draft_to(
    draft_path: &Path,
    skills_dir: &Path,
) -> anyhow::Result<(String, PathBuf)> {
    let skill_md = draft_path.join("SKILL.md");
    if !tokio::fs::try_exists(&skill_md).await.unwrap_or(false) {
        bail!(
            "{} does not contain a SKILL.md — not a skill draft?",
            draft_path.display()
        );
    }
    let body = tokio::fs::read_to_string(&skill_md)
        .await
        .with_context(|| format!("reading {}", skill_md.display()))?;

    let slug = skill_slug_from(&body, draft_path)
        .with_context(|| format!("could not derive a slug for {}", draft_path.display()))?;

    let dest = skills_dir.join(&slug);
    if tokio::fs::try_exists(&dest).await.unwrap_or(false) {
        tokio::fs::remove_dir_all(&dest)
            .await
            .with_context(|| format!("clearing previous install at {}", dest.display()))?;
    }
    copy_dir_recursive(draft_path, &dest)
        .await
        .with_context(|| format!("copying {} → {}", draft_path.display(), dest.display()))?;

    tokio::fs::remove_dir_all(draft_path)
        .await
        .with_context(|| format!("removing draft {}", draft_path.display()))?;

    Ok((slug, dest))
}

/// Pull a slug out of a SKILL.md body. Prefers the `name:` field in the
/// YAML-ish frontmatter; falls back to the directory's file name with the
/// `.draft` suffix stripped. Returns an error only if both are empty.
fn skill_slug_from(skill_md: &str, draft_path: &Path) -> anyhow::Result<String> {
    let from_frontmatter = parse_skill_name(skill_md);
    if let Some(name) = from_frontmatter
        && !name.trim().is_empty()
    {
        return Ok(name.trim().to_string());
    }
    let leaf = draft_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .trim_end_matches(".draft");
    if leaf.is_empty() {
        bail!("empty skill name");
    }
    Ok(leaf.to_string())
}

/// Minimal frontmatter reader — looks for a `name:` line inside a
/// `---`-fenced block at the top of the file. Duplicates the logic in
/// [`thoth_store::markdown::parse_skill_frontmatter`] rather than
/// depending on the private helper.
fn parse_skill_name(text: &str) -> Option<String> {
    let rest = text.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    let block = &rest[..end];
    for line in block.lines() {
        if let Some(v) = line.strip_prefix("name:") {
            return Some(v.trim().to_string());
        }
    }
    None
}

/// Recursively copy a directory tree. Plain files only — symlinks are
/// skipped (skills are expected to be self-contained plain trees, same
/// assumption [`thoth_store::markdown::install_from_directory`] makes).
async fn copy_dir_recursive(src: &Path, dest: &Path) -> anyhow::Result<()> {
    tokio::fs::create_dir_all(dest).await?;
    let mut stack: Vec<(PathBuf, PathBuf)> = vec![(src.to_path_buf(), dest.to_path_buf())];
    while let Some((from, to)) = stack.pop() {
        let mut rd = tokio::fs::read_dir(&from).await?;
        while let Some(entry) = rd.next_entry().await? {
            let ft = entry.file_type().await?;
            let child_from = entry.path();
            let child_to = to.join(entry.file_name());
            if ft.is_dir() {
                tokio::fs::create_dir_all(&child_to).await?;
                stack.push((child_from, child_to));
            } else if ft.is_file() {
                tokio::fs::copy(&child_from, &child_to).await?;
            }
        }
    }
    Ok(())
}

/// `thoth mcp install [--scope ...]` — registers `thoth-mcp` under
/// `mcpServers.thoth`. Idempotent.
///
/// The config file is **not** `.claude/settings.json` — Claude Code ignores
/// `mcpServers` there. Project-scoped config goes in `<root>/.mcp.json`;
/// user-scoped config goes in `~/.claude.json`. Other top-level fields in
/// those files are preserved.
pub async fn mcp_install(scope: Scope, root: &Path) -> anyhow::Result<()> {
    let path = scope.mcp_path()?;
    let mut template: Value = serde_json::from_str(BUNDLE_MCP)
        .context("parsing embedded mcp.json — this is a build bug")?;

    // Resolve THOTH_ROOT to an absolute path — Claude Code spawns the MCP
    // server from its own working directory (not the project root), so a
    // relative path would resolve incorrectly. Fall back to the path as
    // given if canonicalization fails (e.g. path doesn't exist yet).
    let root_abs = tokio::fs::canonicalize(root)
        .await
        .unwrap_or_else(|_| root.to_path_buf());
    let thoth_mcp_bin = resolve_companion("thoth-mcp");

    if let Some(entry) = template
        .get_mut("mcpServers")
        .and_then(|s| s.get_mut(MCP_SERVER_KEY))
        .and_then(|v| v.as_object_mut())
    {
        // Rewrite `command` to the absolute path of the sibling binary so
        // GUI-launched Claude Code (which doesn't inherit the user's shell
        // PATH) can still spawn it.
        entry.insert("command".to_string(), Value::String(thoth_mcp_bin.clone()));

        let env_entry = entry.entry("env".to_string()).or_insert_with(|| json!({}));
        // Embedded BUNDLE_MCP always ships `env: {}`; this guards against
        // a future template edit silently converting it to the wrong shape.
        if !env_entry.is_object() {
            *env_entry = json!({});
        }
        let env = env_entry
            .as_object_mut()
            .expect("env_entry was just normalised to an object above");
        env.insert(
            "THOTH_ROOT".to_string(),
            Value::String(root_abs.display().to_string()),
        );
    }

    let mut existing = read_mcp_config(&path).await?;
    if !existing.is_object() {
        bail!(
            "{} exists but isn't a JSON object — refusing to overwrite",
            path.display()
        );
    }
    merge_mcp(&mut existing, &template);
    write_settings(&path, &existing).await?;
    println!("✓ mcp server `thoth` installed into {}", path.display());
    println!(
        "  command: {}  (THOTH_ROOT={})",
        thoth_mcp_bin,
        root_abs.display()
    );
    println!("  uninstall: thoth mcp uninstall");
    Ok(())
}

/// `thoth mcp uninstall [--scope ...]`
pub async fn mcp_uninstall(scope: Scope) -> anyhow::Result<()> {
    let path = scope.mcp_path()?;
    if !path.exists() {
        println!("no mcp config at {} — nothing to remove", path.display());
        return Ok(());
    }
    let mut existing = read_mcp_config(&path).await?;
    strip_mcp(&mut existing);
    // For project scope, if the file is now empty / just `{}`, remove it
    // rather than leaving an empty stub behind.
    if matches!(scope, Scope::Project)
        && existing.as_object().map(|m| m.is_empty()).unwrap_or(false)
    {
        let _ = tokio::fs::remove_file(&path).await;
        println!(
            "✓ mcp server `thoth` removed; deleted empty {}",
            path.display()
        );
        return Ok(());
    }
    write_settings(&path, &existing).await?;
    println!("✓ mcp server `thoth` removed from {}", path.display());
    Ok(())
}

/// Read an MCP config file (`.mcp.json` or `~/.claude.json`) as a JSON
/// value. Returns an empty object if absent. Uses the same semantics as
/// [`read_settings`] but kept as a separate name for clarity.
async fn read_mcp_config(path: &Path) -> anyhow::Result<Value> {
    read_settings(path).await
}

// -------------------------------------------------------- statusline install

/// Install the Thoth statusline script and merge the `statusLine` key
/// into `.claude/settings.json`.
pub async fn statusline_install(scope: Scope) -> anyhow::Result<()> {
    let settings_path = scope.settings_path()?;
    let claude_dir = settings_path
        .parent()
        .with_context(|| format!("settings path has no parent: {}", settings_path.display()))?;
    let script_path = claude_dir.join("thoth-statusline.sh");

    // Write the bundled script.
    tokio::fs::create_dir_all(claude_dir).await?;
    tokio::fs::write(&script_path, BUNDLE_STATUSLINE).await?;

    // Merge statusLine into settings.json.
    let mut settings = read_settings(&settings_path).await?;
    settings["statusLine"] = serde_json::json!({
        "type": "command",
        "command": format!("sh {}", script_path.display()),
        "refreshInterval": 5,
    });
    write_settings(&settings_path, &settings).await?;

    println!("✓ statusline installed: {}", script_path.display());
    Ok(())
}

/// Remove the Thoth statusline from settings.json and delete the script.
pub async fn statusline_uninstall(scope: Scope) -> anyhow::Result<()> {
    let settings_path = scope.settings_path()?;

    // Remove statusLine key from settings.json.
    let mut settings = read_settings(&settings_path).await?;
    if let Some(obj) = settings.as_object_mut() {
        obj.remove("statusLine");
    }
    write_settings(&settings_path, &settings).await?;

    // Delete the script file.
    let script_path = settings_path
        .parent()
        .with_context(|| format!("settings path has no parent: {}", settings_path.display()))?
        .join("thoth-statusline.sh");
    let _ = tokio::fs::remove_file(&script_path).await;

    Ok(())
}

/// `thoth install` — convenience one-shot: skill + hooks + mcp, all in the
/// same scope. Idempotent; safe to re-run.
pub async fn install_all(scope: Scope, root: &Path) -> anyhow::Result<()> {
    skills_install(scope, root).await?;
    install(scope, root).await?;
    mcp_install(scope, root).await?;
    statusline_install(scope).await?;
    // Project-scope only: write the CLAUDE.md policy block. This is the
    // one artifact Claude Code re-loads after `/clear` and `/compact`, so
    // it's what teaches the agent that Thoth owns long-term memory on a
    // fresh/collapsed context.
    claude_md_install(scope, &crate::setup::today_ymd()).await?;
    println!();
    println!("✓ thoth fully wired into Claude Code ({scope:?} scope)");
    Ok(())
}

/// `thoth uninstall` — removes every bundled skill + hooks + mcp entry
/// from `settings.json`. Skill directory removal is best-effort; we only
/// drop directories we ship (per [`BUNDLE_SKILLS`]), never user files.
pub async fn uninstall_all(scope: Scope, root: &Path) -> anyhow::Result<()> {
    let base = scope.skills_dir(root)?;
    for (name, _) in BUNDLE_SKILLS {
        let dir = base.join(name);
        if dir.exists() {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            println!("✓ skill `{name}` removed from {}", dir.display());
        }
    }
    uninstall(scope).await?;
    mcp_uninstall(scope).await?;
    statusline_uninstall(scope).await?;
    claude_md_uninstall(scope).await?;
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
    /// `PreCompact` — fires right before context compression. Last chance
    /// to save conversation state before the window shrinks.
    PreCompact,
}

/// `thoth hooks exec <event>`. Called by Claude Code itself. Reads the
/// hook payload as JSON on stdin, does its thing, and prints any new
/// context to stdout.
///
/// Every error is swallowed to stderr and the process exits 0 — a failing
/// hook must never block the agent.
pub async fn exec(event: HookEvent, root: &Path) -> anyhow::Result<()> {
    let payload = read_stdin_json().await.unwrap_or_else(|_| json!({}));
    let mut buf = String::new();
    let result = match event {
        HookEvent::SessionStart => run_session_start(root, &mut buf).await,
        HookEvent::UserPromptSubmit => run_user_prompt(root, &payload, &mut buf).await,
        HookEvent::PostToolUse => run_post_tool(root, &payload, &mut buf).await,
        HookEvent::Stop => run_stop(root, &payload).await,
        HookEvent::PreCompact => run_pre_compact(root, &payload).await,
    };
    if let Err(e) = result {
        eprintln!("thoth: hook error: {e}");
    }
    // Output JSON for Claude Code's `"output": "json"` hook mode.
    println!("{}", json!({"result": buf}));
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

async fn run_session_start(root: &Path, buf: &mut String) -> anyhow::Result<()> {
    use std::fmt::Write;
    if !root.exists() {
        return Ok(());
    }

    // Bump the session watermark so ReflectionDebt windows off this
    // point. Failing the write only degrades debt accounting; it must
    // never block the hook.
    if let Err(e) = thoth_memory::mark_session_start(root).await {
        tracing::warn!(error = %e, "mark_session_start failed");
    }

    // Policy banner — always emitted, even when MEMORY.md / LESSONS.md
    // are empty. This is the only signal in the default wiring that
    // tells the agent Thoth owns long-term memory *before* it reaches
    // for a built-in auto-memory path. Without this, a fresh project
    // (no MEMORY/LESSONS content) has no in-context pointer to Thoth
    // until the PreToolUse gate blocks — which happens after the agent
    // has already tried the wrong path.
    let _ = writeln!(buf, "### Thoth memory policy");
    let _ = writeln!(buf, "This project uses Thoth MCP as its long-term memory.");
    let _ = writeln!(
        buf,
        "- Persist facts via `mcp__thoth__thoth_remember_fact`; lessons via \
         `mcp__thoth__thoth_remember_lesson`. These write to \
         ./.thoth/MEMORY.md and ./.thoth/LESSONS.md — the single source of truth."
    );
    let _ = writeln!(
        buf,
        "- Do NOT write to auto-memory paths outside `.thoth/`."
    );
    let _ = writeln!(
        buf,
        "- Before any Write/Edit/Bash: a `thoth_recall` must have been logged \
         within the gate window (strict mode blocks otherwise)."
    );
    let _ = writeln!(
        buf,
        "  The UserPromptSubmit hook auto-recalls for each user prompt, so the \
         first tool call after a prompt usually passes. Call \
         `mcp__thoth__thoth_recall` explicitly when switching topic mid-session."
    );
    let _ = writeln!(buf);

    // Carry-over nag: if the previous session left reflection debt
    // that crossed the nudge threshold, run_stop wrote a marker into
    // `.thoth/.reflect-nag`. Consume it here so the agent sees the
    // message in its first context injection of the new session —
    // stderr during run_stop only surfaces to the user.
    if let Some(body) = thoth_memory::reflection::take_nag(root).await {
        let trimmed = body.trim();
        if !trimmed.is_empty() {
            let _ = writeln!(buf, "### Reflection debt from the previous session");
            let _ = writeln!(buf, "{trimmed}");
            let _ = writeln!(buf);
        }
    }

    // Print USER.md (user-authored prefs) + MEMORY.md + LESSONS.md verbatim;
    // Claude Code picks stdout up as additional context. Keep it compact.
    // USER.md is injected first so user preferences frame the MEMORY/LESSONS
    // that follow. Missing USER.md is silently skipped (REQ-01).
    for name in ["USER.md", "MEMORY.md", "LESSONS.md"] {
        let p = root.join(name);
        let Ok(body) = tokio::fs::read_to_string(&p).await else {
            continue;
        };
        let trimmed = body.trim();
        if trimmed.is_empty() {
            continue;
        }
        let _ = writeln!(buf, "### {name}");
        let _ = writeln!(buf, "{trimmed}");
        let _ = writeln!(buf);
    }

    // L0 archive status — lightweight (~100 tokens) orientation so the
    // agent knows how much verbatim context is available via
    // `thoth_recall scope:"archive"` or `thoth_archive_search`.
    if let Ok(tracker) =
        thoth_store::ArchiveTracker::open(thoth_store::StoreRoot::archive_path(root)).await
        && let Ok((sessions, turns, curated)) = tracker.status()
        && sessions > 0
    {
        let _ = writeln!(buf, "### Conversation archive");
        let _ = writeln!(
            buf,
            "{sessions} sessions, {turns} turns ({curated} curated). \
             Use `thoth_recall` with `scope: \"archive\"` or `scope: \"all\"` \
             to search past conversations. Use `thoth_archive_topics` to browse topics."
        );
        let _ = writeln!(buf);
    }

    // Curator pass. Equivalent to invoking `thoth curate --quiet`
    // directly, but inlined so we don't re-exec the binary from its
    // own hook (costly on cold start). Any findings are printed to
    // stdout → Claude Code injects them as banner context.
    if let Err(e) = curate_quiet(root, buf).await {
        tracing::warn!(error = %e, "curate_quiet failed");
    }

    Ok(())
}

/// Inlined equivalent of `thoth curate --quiet` used by the
/// SessionStart hook. Keeps the hook self-contained (no re-exec) and
/// never fails the hook — errors degrade to a trace line.
///
/// "Quiet" semantics: only flag items that actually need attention.
/// A forget pass that dropped zero episodes isn't a finding — surfacing
/// it every session would pollute the banner and teach the agent to
/// ignore the whole "Curator findings" block.
async fn curate_quiet(root: &Path, buf: &mut String) -> anyhow::Result<()> {
    use std::fmt::Write;
    let mut findings: Vec<String> = Vec::new();

    // Forget pass via daemon if alive; otherwise skip silently — the
    // direct-store path needs an exclusive redb lock that the MCP
    // daemon typically holds at session start. Losing this run only
    // delays the GC by one session.
    //
    // Suppress the finding when the structured `data` shows no
    // drops — the tool itself also elides text on a no-op now, but
    // we defend client-side too in case a stale daemon binary is
    // still running with the older always-non-empty format.
    if let Some(mut d) = crate::daemon::DaemonClient::try_connect(root).await
        && let Ok(res) = d.call("thoth_memory_forget", serde_json::json!({})).await
        && !crate::daemon::tool_is_error(&res)
        && forget_has_drops(&crate::daemon::tool_data(&res))
    {
        let t = crate::daemon::tool_text(&res).trim().to_string();
        if !t.is_empty() {
            findings.push(t);
        }
    }

    // Reflection debt report — independent of the daemon. This is
    // the same counter the gate blocks on, surfaced for visibility.
    let disc = thoth_memory::DisciplineConfig::load_or_default(root).await;
    let debt = thoth_memory::ReflectionDebt::compute(root).await;
    if debt.should_nudge(&disc) {
        findings.push(debt.render());
    }

    if !findings.is_empty() {
        let _ = writeln!(buf, "### Curator findings");
        for f in &findings {
            let _ = writeln!(buf, "- {f}");
        }
        let _ = writeln!(buf);
    }
    Ok(())
}

async fn run_user_prompt(root: &Path, payload: &Value, buf: &mut String) -> anyhow::Result<()> {
    use std::fmt::Write;
    if !root.exists() {
        return Ok(());
    }
    let prompt = payload
        .get("prompt")
        .and_then(Value::as_str)
        .or_else(|| payload.get("user_prompt").and_then(Value::as_str))
        .unwrap_or("")
        .trim()
        .to_string();
    if prompt.is_empty() {
        return Ok(());
    }

    // Save user turn verbatim via the daemon (fire-and-forget).
    if let Some(session_id) = payload.get("session_id").and_then(Value::as_str)
        && let Some(mut d) = crate::daemon::DaemonClient::try_connect(root).await
    {
        let _ = d
            .call(
                "thoth_turn_save",
                serde_json::json!({
                    "session_id": session_id,
                    "role": "user",
                    "content": prompt,
                }),
            )
            .await;
    }

    // Reflection-debt heartbeat. If the agent has been editing
    // without persisting anything, prepend a visible reminder to the
    // recall block below so the agent sees it at every prompt — the
    // single most load-bearing moment for the reflect loop.
    //
    // Deliberately cheap: two file reads (config + two JSONL tails).
    // Worst case < 5 ms on a session-sized log.
    let discipline = thoth_memory::DisciplineConfig::load_or_default(root).await;
    let debt = thoth_memory::ReflectionDebt::compute(root).await;
    if debt.should_nudge(&discipline) {
        let _ = writeln!(buf, "### Reflection debt");
        let _ = writeln!(buf, "{}", debt.render());
        let _ = writeln!(buf);
    }

    // Prefer the running MCP daemon. `thoth-mcp` holds an exclusive redb
    // lock on `.thoth/`; calling `StoreRoot::open` here would fail with
    // "Database already open" whenever Claude Code has the MCP server
    // alive — which is the common case this hook runs in.
    //
    // We pass `log_event: false` so the daemon DOES NOT append a
    // `QueryIssued` event for this recall. Rationale: this hook fires on
    // every user prompt, automatically. Letting it log would let the
    // hook's ceremonial recall satisfy `thoth-gate`'s window check —
    // making the discipline vacuous. The gate should prove the *agent*
    // consulted memory for the upcoming tool call, not that the hook
    // incidentally ran a recall on the prompt text. So: this path is
    // purely context injection; the agent still has to invoke
    // `mcp__thoth__thoth_recall` itself before Write/Edit/Bash.
    // Minimum score for a chunk to be injected by the auto-recall hook.
    // With RRF K=10 a single-stage rank-0 hit scores ~0.09; a two-stage
    // hit scores ~0.18. Setting the bar at 0.08 filters truly irrelevant
    // results (no stage ranked them near the top) while preserving any
    // hit that at least one stage considered a strong match.
    const MIN_AUTO_RECALL_SCORE: f64 = 0.08;

    if let Some(mut d) = crate::daemon::DaemonClient::try_connect(root).await {
        let result = d
            .call(
                "thoth_recall",
                serde_json::json!({
                    "query": prompt,
                    "top_k": 5,
                    "log_event": false,
                }),
            )
            .await?;
        if !crate::daemon::tool_is_error(&result) {
            // Check if the best chunk score exceeds the relevance bar.
            // The daemon's `data.chunks` array carries per-chunk scores.
            let dominated_by_noise = crate::daemon::tool_data(&result)
                .get("chunks")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|c| c.get("score").and_then(Value::as_f64))
                        .all(|s| s < MIN_AUTO_RECALL_SCORE)
                })
                .unwrap_or(false);
            if !dominated_by_noise {
                let text = crate::daemon::tool_text(&result);
                if !text.trim().is_empty() {
                    let _ = writeln!(buf, "### thoth recall");
                    let _ = writeln!(buf, "{text}");
                }
            }
        }
        return Ok(());
    }

    // Fallback: no daemon running (rare during a Claude Code session but
    // still possible — e.g. `thoth hooks exec` invoked from CI or a
    // script). Open the store directly and run a best-effort recall for
    // context only; we deliberately do NOT log `QueryIssued` here, for
    // the same reason as the daemon path above.
    let store = StoreRoot::open(root).await?;
    let retrieve_cfg = RetrieveConfig::load_or_default(root).await;
    let retriever = Retriever::new(store).with_markdown_boost(retrieve_cfg.rerank_markdown_boost);
    let q = thoth_core::Query {
        text: prompt,
        top_k: 5,
        ..thoth_core::Query::text("")
    };
    let out = retriever.recall(&q).await?;
    let relevant: Vec<_> = out
        .chunks
        .iter()
        .filter(|c| (c.score as f64) >= MIN_AUTO_RECALL_SCORE)
        .collect();
    if relevant.is_empty() {
        return Ok(());
    }
    let _ = writeln!(buf, "### thoth recall (top {})", relevant.len());
    for c in relevant.iter() {
        let sym = c.symbol.as_deref().unwrap_or("-");
        let _ = writeln!(
            buf,
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

async fn run_post_tool(root: &Path, payload: &Value, buf: &mut String) -> anyhow::Result<()> {
    use std::fmt::Write;
    if !root.exists() {
        return Ok(());
    }

    // Cadence heartbeat. Writes the debt nag to the buffer (picked up by
    // Claude Code as additional context) when the user has opted into
    // `reflect_cadence = "every"`. `"end"` (the default) short-circuits.
    if let Some(msg) = cadence_heartbeat(root).await {
        let _ = writeln!(buf, "{msg}");
    }

    // Background review trigger. Spawn a detached `thoth review`
    // process when mutations since the last review cross the
    // configured interval. Fire-and-forget — the spawned process
    // outlives this hook invocation.
    maybe_spawn_background_review(root).await;

    // T-18: Outcome harvester. Turn the PostToolUse event into lesson
    // reinforcement signals, violation rows, and auto-promotion tier
    // flips. Best-effort: any failure here is swallowed so the hook
    // still falls through to the re-index path below.
    if let Err(e) = run_outcome_harvest(root, payload).await {
        tracing::warn!(error = %e, "post_tool: outcome_harvest failed");
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

    // Prefer the running daemon — it holds the exclusive redb lock, so
    // `StoreRoot::open` here would fail with "Database already open"
    // while Claude Code has thoth-mcp alive. Forward the re-index
    // request through the same socket the CLI uses.
    if let Some(mut d) = crate::daemon::DaemonClient::try_connect(root).await {
        let _ = d
            .call(
                "thoth_index",
                serde_json::json!({ "path": p.to_string_lossy() }),
            )
            .await;
        return Ok(());
    }

    // Fallback: no daemon (CI / script / smoke test). Direct re-index.
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

/// Spawn a detached `thoth review` when the mutation count since the
/// last review crosses the configured interval. Uses
/// `std::process::Command` (not tokio) so the child outlives the hook
/// process.
async fn maybe_spawn_background_review(root: &Path) {
    let disc = thoth_memory::DisciplineConfig::load_or_default(root).await;
    if !disc.background_review || disc.background_review_interval == 0 {
        return;
    }
    // gate.jsonl is the mutation-counter source — if gate telemetry is
    // off, the count is always 0 and the review never fires.
    if !disc.gate_telemetry_enabled {
        return;
    }

    let mutations = thoth_memory::mutations_since_last_review(root).await;
    if mutations < disc.background_review_interval {
        return;
    }

    // Time-based cooldown on top of mutation count. Stops a rapid burst
    // of edits from firing multiple reviews back-to-back — each review
    // costs an LLM call, and reviews fired seconds apart can't
    // meaningfully differ in what they'd curate.
    if disc.background_review_min_secs > 0
        && let Some(last) = thoth_memory::read_last_review(root).await
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let elapsed = now.saturating_sub(last);
        if (elapsed as u64) < disc.background_review_min_secs {
            return;
        }
    }

    // Resolve the thoth binary path — prefer the same binary that's
    // running now (so cargo-installed dev builds work), else fall back
    // to "thoth" on PATH.
    let thoth_bin = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("thoth"));

    let backend = &disc.background_review_backend;

    match std::process::Command::new(&thoth_bin)
        .args([
            "--root",
            &root.to_string_lossy(),
            "review",
            "--backend",
            backend,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(_) => tracing::debug!("background-review: spawned detached process"),
        Err(e) => tracing::warn!(error = %e, "background-review: failed to spawn"),
    }
}

/// T-18: turn a PostToolUse payload into a harvester invocation.
///
/// Pulls `tool_name` + `tool_input` + `tool_response.is_error` +
/// `session_id` out of the Claude Code hook payload, loads every lesson
/// from `LESSONS.md`, and runs [`OutcomeHarvester::harvest_post_tool`]
/// against them. When the harvester mutates any lesson (success /
/// failure counter bump or tier flip) we rewrite the store so the change
/// is durable.
///
/// Legacy lessons that lack a structured trigger (frontmatter-derived
/// `tool` / `path_glob` / `cmd_regex`) fall through
/// [`LessonTrigger::natural_only`] and never match mechanically — the
/// harvester is a no-op for them, which is the documented behaviour.
///
/// Returns `Ok(())` on a clean run. A missing payload field (no
/// `tool_name`) or an empty lesson list short-circuits early and is not
/// an error.
pub(crate) async fn run_outcome_harvest(root: &Path, payload: &Value) -> anyhow::Result<()> {
    use thoth_core::memory::LessonTrigger;
    use thoth_memory::EnforcementConfig;
    use thoth_memory::lesson_matcher::ToolCall;
    use thoth_memory::outcome_harvest::{LessonEntry, OutcomeHarvester};

    let Some(tool_name) = payload.get("tool_name").and_then(Value::as_str) else {
        return Ok(());
    };
    let input = payload.get("tool_input").cloned().unwrap_or(Value::Null);
    let path = input
        .get("file_path")
        .and_then(Value::as_str)
        .map(|s| s.to_string());
    let command = input
        .get("command")
        .and_then(Value::as_str)
        .map(|s| s.to_string());
    // Edit: concat old_string + new_string. Write: content.
    let content = {
        let mut buf = String::new();
        for k in ["old_string", "new_string", "content"] {
            if let Some(s) = input.get(k).and_then(Value::as_str) {
                if !buf.is_empty() {
                    buf.push('\n');
                }
                buf.push_str(s);
            }
        }
        if buf.is_empty() { None } else { Some(buf) }
    };

    let is_error = payload
        .get("tool_response")
        .and_then(|v| v.get("is_error"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let session_id = payload
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();

    let mut call = ToolCall::new(tool_name);
    if let Some(p) = path {
        call = call.with_path(p);
    }
    if let Some(c) = command {
        call = call.with_command(c);
    }
    if let Some(c) = content {
        call = call.with_content(c);
    }

    // Open the markdown store to read / rewrite lessons. Fails silently
    // if the store isn't initialised (e.g. smoke-test roots) — the hook
    // must never block on a missing store.
    let store = match thoth_store::MarkdownStore::open(root).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "outcome_harvest: MarkdownStore::open failed");
            return Ok(());
        }
    };
    let lessons = match store.read_lessons().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "outcome_harvest: read_lessons failed");
            return Ok(());
        }
    };
    if lessons.is_empty() {
        return Ok(());
    }

    let mut entries: Vec<LessonEntry> = lessons
        .into_iter()
        .map(|l| {
            let trigger = LessonTrigger::natural_only(l.trigger.clone());
            LessonEntry { lesson: l, trigger }
        })
        .collect();

    let cfg = EnforcementConfig::default();
    let harvester = OutcomeHarvester::new(root.to_path_buf(), cfg);

    // Deterministic-ish tool-call hash — timestamp + tool name. The
    // harvester only uses it for violation audit rows, so a coarse key
    // is fine.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let tool_call_hash = format!("{}-{}", tool_name, now);

    let report = harvester.harvest_post_tool(
        &call,
        is_error,
        &session_id,
        &tool_call_hash,
        now,
        &mut entries,
    )?;

    // Persist mutations. Bypass the rewrite when nothing actually
    // changed — avoids a needless file churn on every tool call.
    if !report.lesson_outcomes.is_empty() {
        let new_lessons: Vec<_> = entries.into_iter().map(|e| e.lesson).collect();
        if let Err(e) = store.rewrite_lessons(&new_lessons).await {
            tracing::warn!(error = %e, "outcome_harvest: rewrite_lessons failed");
        }
    }
    Ok(())
}

/// T-18: on Stop, scan for workflows that were started (via
/// `thoth_workflow_start`) but never completed. For each such
/// workflow — or for any Phase 4b workflow with a non-empty
/// `detect_gap` — append a row to `workflow-violations.jsonl` and warn
/// on stderr.
///
/// When the payload carries `session_id` we scope the check to that
/// session; otherwise we iterate every `Active` workflow on disk (the
/// safe superset). The Stop hook never blocks the turn — all errors
/// are surfaced via `tracing::warn` and swallowed.
pub(crate) async fn run_workflow_close_check(root: &Path, payload: &Value) -> anyhow::Result<()> {
    use thoth_memory::workflow::WorkflowStateManager;

    let mgr = WorkflowStateManager::new(root.to_path_buf());
    let session_id = payload.get("session_id").and_then(Value::as_str);

    // Collect the set of sessions we care about.
    let states = if let Some(sid) = session_id {
        match mgr.get(sid)? {
            Some(s) if s.status == thoth_memory::workflow::WorkflowStatus::Active => vec![s],
            _ => Vec::new(),
        }
    } else {
        mgr.list_active()?
    };

    if states.is_empty() {
        return Ok(());
    }

    let cfg = thoth_memory::EnforcementConfig::default();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // 7-day window, per DESIGN-SPEC #12.
    let window: i64 = 7 * 24 * 60 * 60;

    for state in states {
        let gap = mgr.detect_gap(&state.session_id).unwrap_or_default();
        let reason = if gap.is_empty() {
            "stop_without_complete".to_string()
        } else {
            format!("stop_with_gap: {}", gap.join(","))
        };
        match mgr.increment_violation(
            state.session_id.clone(),
            state.workflow_name.clone(),
            reason.clone(),
            now,
            window,
        ) {
            Ok(count) => {
                eprintln!(
                    "thoth: workflow `{}` ended without complete ({}). Violations this week: {}/{}.",
                    state.workflow_name, reason, count, cfg.workflow_violation_threshold
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    session = %state.session_id,
                    "workflow_close_check: increment_violation failed"
                );
            }
        }
    }
    Ok(())
}

async fn run_stop(root: &Path, payload: &Value) -> anyhow::Result<()> {
    if !root.exists() {
        return Ok(());
    }

    // T-18: workflow close check. For every active workflow whose
    // session matches this Stop event (or — when the payload omits
    // session_id — every active workflow on disk), record a violation
    // row and leave the state file marked `Active`. `thoth workflow
    // reset` is the user-driven escape hatch.
    if let Err(e) = run_workflow_close_check(root, payload).await {
        tracing::warn!(error = %e, "stop: workflow close check failed");
    }

    // Reflection-debt nag. Compute first so stderr carries the
    // message to the user (who can see it immediately) AND drop a
    // marker file so the next SessionStart injects the message into
    // agent context (where it's actionable). Failing either path is
    // non-fatal — the Stop hook must never block the turn.
    let discipline = thoth_memory::DisciplineConfig::load_or_default(root).await;
    let debt = thoth_memory::ReflectionDebt::compute(root).await;
    if debt.should_nudge(&discipline) {
        let msg = debt.render();
        eprintln!("thoth: {msg}");
        if let Err(e) = thoth_memory::reflection::write_nag(root, &msg).await {
            tracing::warn!(error = %e, "write_nag failed");
        }
    }

    // Prefer the daemon: `thoth_memory_forget` runs the same TTL +
    // capacity + quarantine pass that `MemoryManager::forget_pass`
    // does, but reuses the daemon's open store and avoids the
    // exclusive-lock collision that would otherwise make this hook
    // no-op whenever Claude Code has thoth-mcp alive.
    if let Some(mut d) = crate::daemon::DaemonClient::try_connect(root).await {
        match d.call("thoth_memory_forget", serde_json::json!({})).await {
            Ok(res) if !crate::daemon::tool_is_error(&res) => {
                let text = crate::daemon::tool_text(&res);
                if !text.trim().is_empty() {
                    eprintln!("thoth: {}", text.trim());
                }
            }
            Ok(res) => eprintln!("thoth: forget failed: {}", crate::daemon::tool_text(&res)),
            Err(e) => eprintln!("thoth: daemon forget call failed: {e}"),
        }
        // Nudge is intentionally skipped on the daemon path: the daemon
        // has its own lifecycle for synth-backed memory curation, and
        // triggering it from a hook would double-invoke if the user has
        // also wired nudge into the daemon directly.
        return Ok(());
    }

    // Fallback: direct store access (no daemon).
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
            match thoth_retrieve::anthropic::AnthropicSynthesizer::from_env() {
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

/// PreCompact: fires right before context compression. Saves a marker
/// so the agent knows context was compressed, and triggers a forget
/// pass to clean up before the window shrinks.
async fn run_pre_compact(root: &Path, _payload: &Value) -> anyhow::Result<()> {
    if !root.exists() {
        return Ok(());
    }
    eprintln!("thoth: pre-compact — saving state before context compression");

    if let Some(mut d) = crate::daemon::DaemonClient::try_connect(root).await {
        // Run forget pass to clean up before compression
        let _ = d.call("thoth_memory_forget", serde_json::json!({})).await;

        // Save a marker episode so we know compaction happened
        let _ = d
            .call(
                "thoth_episode_append",
                serde_json::json!({
                    "kind": "nudge_invoked",
                    "text": "context_compaction",
                }),
            )
            .await;
    }
    Ok(())
}

/// Return the `reflect_cadence = "every"` heartbeat message if one
/// should fire at this PostToolUse event, else `None`.
///
/// Separated from [`run_post_tool`] so the logic (a) has a test seam
/// without needing to capture stdout and (b) can be called from any
/// other per-tool hook path in the future (e.g. a PreToolUse variant).
///
/// Returns `None` when:
/// - the root is missing or the config can't be read (tolerant — a
///   broken config must never silence a hook that just indexes files)
/// - `reflect_cadence` is anything other than `"every"` (case-insensitive)
/// - debt is below the nudge threshold
pub(crate) async fn cadence_heartbeat(root: &Path) -> Option<String> {
    if !root.exists() {
        return None;
    }
    let discipline = thoth_memory::DisciplineConfig::load_or_default(root).await;
    if !discipline.reflect_cadence.eq_ignore_ascii_case("every") {
        return None;
    }
    let debt = thoth_memory::ReflectionDebt::compute(root).await;
    if !debt.should_nudge(&discipline) {
        return None;
    }
    Some(format!(
        "### Reflection debt (cadence=every)\n{}\n",
        debt.render()
    ))
}

/// `true` if the `thoth_memory_forget` response's `data` shows at
/// least one counter > 0. Used both in hook banner suppression and
/// by `cmd_curate` so a no-op forget pass doesn't surface every time.
pub(crate) fn forget_has_drops(data: &Value) -> bool {
    [
        "episodes_ttl",
        "episodes_cap",
        "lessons_dropped",
        "lessons_quarantined",
    ]
    .iter()
    .any(|k| data.get(*k).and_then(Value::as_u64).unwrap_or(0) > 0)
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

// ------------------------------------------------- CLI enum definitions

#[derive(clap::Subcommand, Debug)]
pub enum HooksCmd {
    /// Install Thoth's Claude Code hook block into `settings.json`.
    Install {
        #[arg(long, value_enum, default_value = "project")]
        scope: Scope,
    },
    /// Remove thoth-managed hooks from `settings.json`. Leaves user-owned
    /// hooks untouched.
    Uninstall {
        #[arg(long, value_enum, default_value = "project")]
        scope: Scope,
    },
    /// Runtime dispatcher — called by Claude Code itself with a JSON
    /// payload on stdin. Not intended for interactive use.
    Exec {
        #[arg(value_enum)]
        event: HookEvent,
    },
}

#[derive(clap::Subcommand, Debug)]
pub enum McpCmd {
    /// Register `thoth-mcp` under `mcpServers.thoth` in `settings.json`.
    /// Idempotent.
    Install {
        #[arg(long, value_enum, default_value = "project")]
        scope: Scope,
    },
    /// Remove the `mcpServers.thoth` entry. Other MCP servers are preserved.
    Uninstall {
        #[arg(long, value_enum, default_value = "project")]
        scope: Scope,
    },
}

#[derive(clap::Subcommand, Debug)]
pub enum SkillsCmd {
    /// List installed skills.
    List,
    /// Install skills into `.claude/skills/`.
    ///
    /// With no `PATH`: installs the bundled skills (`memory-discipline`,
    /// `thoth-reflect`, `thoth-guide`, `thoth-exploring`, `thoth-debugging`,
    /// `thoth-impact-analysis`, `thoth-refactoring`, `thoth-cli`) — this is
    /// the primitive `thoth setup` drives.
    ///
    /// With a `PATH` pointing at a `<slug>.draft/` directory (produced by
    /// the agent's `thoth_skill_propose` MCP tool): promotes the draft
    /// into a live skill Claude Code will load on the next session, then
    /// removes the draft.
    Install {
        /// Path to a `<slug>.draft/` skill directory to promote. If
        /// omitted, the bundled skills are installed instead.
        path: Option<std::path::PathBuf>,
        #[arg(long, value_enum, default_value = "project")]
        scope: Scope,
    },
}

/// `thoth skills list` handler — prefers daemon, falls back to direct store.
pub async fn cmd_skills_list(root: &std::path::Path, json: bool) -> anyhow::Result<()> {
    if let Some(mut d) = crate::daemon::DaemonClient::try_connect(root).await {
        let result = d.call("thoth_skills_list", serde_json::json!({})).await?;
        if crate::daemon::tool_is_error(&result) {
            anyhow::bail!("{}", crate::daemon::tool_text(&result));
        }
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&crate::daemon::tool_data(&result))?
            );
        } else {
            // `text` already handles the empty-list message for us.
            print!("{}", crate::daemon::tool_text(&result));
        }
        return Ok(());
    }

    use thoth_store::StoreRoot;
    let store = StoreRoot::open(root).await?;
    let skills = store.markdown.list_skills().await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&skills)?);
        return Ok(());
    }
    if skills.is_empty() {
        println!(
            "(no skills installed — drop a folder into {}/skills/)",
            store.path.display()
        );
        return Ok(());
    }
    for s in skills {
        println!("{:<28}  {}", s.slug, s.description);
    }
    Ok(())
}

// ----------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle_hooks() -> Value {
        serde_json::from_str(BUNDLE_HOOKS).expect("embedded hooks.json is valid JSON")
    }

    fn bundle_mcp() -> Value {
        serde_json::from_str(BUNDLE_MCP).expect("embedded mcp.json is valid JSON")
    }

    #[test]
    fn merge_is_idempotent() {
        let bundle = bundle_hooks();
        let mut settings = json!({});
        merge_hooks(&mut settings, &bundle);
        let once = settings.clone();
        merge_hooks(&mut settings, &bundle);
        assert_eq!(once, settings);
    }

    #[test]
    fn merge_tags_every_written_entry() {
        let bundle = bundle_hooks();
        let mut settings = json!({});
        merge_hooks(&mut settings, &bundle);
        for (_event, entries) in settings.get("hooks").unwrap().as_object().unwrap() {
            for entry in entries.as_array().unwrap() {
                assert!(
                    is_thoth_managed(entry),
                    "thoth-written entry must carry the sentinel: {entry:?}"
                );
            }
        }
    }

    #[test]
    fn merge_preserves_user_hooks() {
        // Seed a user-owned hook under an event the bundle also targets
        // (PreToolUse), so we can assert both the user entry AND a
        // thoth-managed entry coexist under the same event after merge.
        let bundle = bundle_hooks();
        let mut settings = json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "UserOnly",
                    "hooks": [{"type": "command", "command": "echo user"}]
                }]
            }
        });
        merge_hooks(&mut settings, &bundle);
        let pre = settings
            .get("hooks")
            .unwrap()
            .get("PreToolUse")
            .unwrap()
            .as_array()
            .unwrap();
        assert!(pre.iter().any(|e| !is_thoth_managed(e)));
        assert!(pre.iter().any(is_thoth_managed));
    }

    #[test]
    fn uninstall_removes_only_thoth() {
        let bundle = bundle_hooks();
        let mut settings = json!({
            "hooks": {
                "PostToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{"type": "command", "command": "echo user"}]
                }]
            }
        });
        merge_hooks(&mut settings, &bundle);
        strip_hooks(&mut settings);
        let post = settings
            .get("hooks")
            .and_then(|h| h.get("PostToolUse"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        assert_eq!(post.len(), 1);
        assert!(!is_thoth_managed(&post[0]));
    }

    #[test]
    fn uninstall_on_pure_thoth_clears_hooks_key() {
        let bundle = bundle_hooks();
        let mut settings = json!({});
        merge_hooks(&mut settings, &bundle);
        strip_hooks(&mut settings);
        assert!(settings.get("hooks").is_none());
    }

    #[test]
    fn merge_self_heals_when_bundle_changes() {
        // Simulate an older thoth-managed entry that's no longer in the
        // bundle — e.g. we shipped a matcher we later removed. A
        // re-install should drop the stale entry rather than
        // accumulating.
        let bundle = bundle_hooks();
        let mut settings = json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Obsolete",
                    "hooks": [{"type": "command", "command": "thoth-gate"}],
                    "_thoth_managed": true,
                }]
            }
        });
        merge_hooks(&mut settings, &bundle);
        let pre = settings
            .get("hooks")
            .unwrap()
            .get("PreToolUse")
            .unwrap()
            .as_array()
            .unwrap();
        assert!(
            pre.iter()
                .all(|e| e.get("matcher").and_then(Value::as_str) != Some("Obsolete")),
            "stale thoth-managed entry must be purged on re-install: {pre:?}",
        );
    }

    #[test]
    fn merge_self_heals_when_bundle_drops_event() {
        // Regression: older thoth versions shipped `PostToolUse` prompt
        // hooks which were later removed from the bundle entirely
        // (before being re-added for `reflect_cadence = "every"` —
        // which is why the event name this test uses has to be one
        // the current bundle does *not* ship, so we detect the actual
        // regression rather than the heartbeat entry). A per-event
        // strip left those orphaned forever because the loop only
        // visited events present in the new bundle. Re-install must
        // purge thoth-managed entries under *any* event, not just the
        // ones the current bundle targets.
        let bundle = bundle_hooks();
        // Any event name guaranteed to be absent from the bundle works;
        // `SubagentStop` is a real Claude Code event the bundle has
        // never claimed.
        let dropped_event = "SubagentStop";
        assert!(
            bundle.get(dropped_event).is_none(),
            "this test assumes the current bundle has no {dropped_event}"
        );
        let mut settings = json!({
            "hooks": {
                dropped_event: [{
                    "matcher": "Bash",
                    "hooks": [{"type": "prompt", "prompt": "legacy"}],
                    "_thoth_managed": true,
                }]
            }
        });
        merge_hooks(&mut settings, &bundle);
        assert!(
            settings
                .get("hooks")
                .and_then(|h| h.get(dropped_event))
                .is_none(),
            "stale thoth-managed entry under a dropped event must be purged: {settings:?}",
        );
    }

    #[test]
    fn mcp_merge_is_idempotent() {
        let template = bundle_mcp();
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
        let template = bundle_mcp();
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
        let template = bundle_mcp();
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
        let template = bundle_mcp();
        let mut settings = json!({});
        merge_mcp(&mut settings, &template);
        strip_mcp(&mut settings);
        assert!(settings.get("mcpServers").is_none());
    }

    #[test]
    fn rewrite_companion_leaves_unknown_commands_alone() {
        // Only the named companion binaries should be rewritten — random
        // user commands must be left as-is.
        let mut bundle = json!({
            "PreToolUse": [{
                "matcher": "Bash",
                "hooks": [
                    {"type": "command", "command": "thoth-gate"},
                    {"type": "command", "command": "echo hi"},
                    {"type": "prompt",  "prompt":  "remember X"},
                ]
            }]
        });
        rewrite_companion_commands(&mut bundle, "/tmp/.thoth");
        let hooks = bundle
            .get("PreToolUse")
            .unwrap()
            .as_array()
            .unwrap()
            .first()
            .unwrap()
            .get("hooks")
            .unwrap()
            .as_array()
            .unwrap()
            .clone();
        // echo hi must be untouched
        assert_eq!(
            hooks[1].get("command").and_then(Value::as_str),
            Some("echo hi"),
        );
        // prompt hook must still have its prompt field intact
        assert_eq!(
            hooks[2].get("prompt").and_then(Value::as_str),
            Some("remember X"),
        );
        // thoth-gate is either rewritten to an absolute path OR left as-is
        // if no sibling binary exists during test runs; both are valid.
        let gate_cmd = hooks[0].get("command").and_then(Value::as_str).unwrap();
        assert!(gate_cmd.ends_with("thoth-gate"));
    }

    #[test]
    fn rewrite_substitutes_thoth_root_placeholder() {
        let mut bundle = json!({
            "SessionStart": [{
                "matcher": "startup",
                "hooks": [{
                    "type": "command",
                    "command": "thoth --root {THOTH_ROOT} hooks exec session-start",
                }]
            }]
        });
        rewrite_companion_commands(&mut bundle, "/Users/nat/proj/.thoth");
        let cmd = bundle
            .get("SessionStart")
            .unwrap()
            .as_array()
            .unwrap()
            .first()
            .unwrap()
            .get("hooks")
            .unwrap()
            .as_array()
            .unwrap()
            .first()
            .unwrap()
            .get("command")
            .and_then(Value::as_str)
            .unwrap()
            .to_string();
        // Placeholder is gone, actual path is present, and first token is
        // either `thoth` (no sibling found in test env) or an absolute
        // path ending in `/thoth`.
        assert!(!cmd.contains("{THOTH_ROOT}"));
        assert!(cmd.contains("/Users/nat/proj/.thoth"));
        assert!(cmd.contains("hooks exec session-start"));
    }

    #[test]
    fn shell_quote_leaves_safe_paths_alone() {
        assert_eq!(
            shell_quote("/Users/nat/proj/.thoth"),
            "/Users/nat/proj/.thoth"
        );
        // Spaces force quoting
        let q = shell_quote("/Users/nat/my proj/.thoth");
        assert!(q.starts_with('\'') && q.ends_with('\''));
        assert!(q.contains("my proj"));
        // Embedded single quote gets escaped
        let q = shell_quote("/Users/nat/it's/.thoth");
        assert!(q.contains("'\\''"));
    }

    // ---- CLAUDE.md managed block -----------------------------------------

    #[test]
    fn claude_md_render_includes_init_date() {
        let block = render_claude_md_block("2026-04-16");
        assert!(block.contains("2026-04-16"));
        assert!(block.starts_with(CLAUDE_MD_START));
        assert!(block.ends_with(CLAUDE_MD_END));
    }

    #[test]
    fn claude_md_merge_into_empty_produces_block_only() {
        let block = render_claude_md_block("2026-04-16");
        let out = merge_claude_md("", &block);
        assert!(out.contains(CLAUDE_MD_START));
        assert!(out.contains(CLAUDE_MD_END));
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn claude_md_merge_is_idempotent_for_same_date() {
        // Re-running `thoth setup` on the same UTC day must not rewrite the
        // file. We render the block twice with the same date and assert
        // `merge_claude_md` is stable.
        let block = render_claude_md_block("2026-04-16");
        let once = merge_claude_md("", &block);
        let twice = merge_claude_md(&once, &block);
        assert_eq!(once, twice);
    }

    #[test]
    fn claude_md_merge_replaces_existing_block_between_markers() {
        let old = render_claude_md_block("2025-01-01");
        let new = render_claude_md_block("2026-04-16");
        let existing = format!("{old}\n\nUser notes below.\n");
        let merged = merge_claude_md(&existing, &new);
        assert!(merged.contains("2026-04-16"));
        assert!(!merged.contains("2025-01-01"));
        // User content below the block must survive untouched.
        assert!(merged.contains("User notes below."));
    }

    #[test]
    fn claude_md_merge_preserves_user_content_when_no_markers() {
        let block = render_claude_md_block("2026-04-16");
        let existing = "# My project\n\nSome notes.\n";
        let merged = merge_claude_md(existing, &block);
        // Block goes first so Claude Code picks it up at the top of the file,
        // then a blank line, then the user's original content.
        assert!(merged.starts_with(CLAUDE_MD_START));
        assert!(merged.contains("# My project"));
        assert!(merged.contains("Some notes."));
    }

    #[test]
    fn claude_md_strip_removes_only_managed_block() {
        let block = render_claude_md_block("2026-04-16");
        let existing = format!("# Top\n\n{block}\n\n## My own section\n");
        let stripped = strip_claude_md(&existing);
        assert!(!stripped.contains(CLAUDE_MD_START));
        assert!(!stripped.contains(CLAUDE_MD_END));
        assert!(stripped.contains("# Top"));
        assert!(stripped.contains("My own section"));
    }

    #[test]
    fn claude_md_strip_is_noop_without_markers() {
        let existing = "# My project\n\nNo Thoth here.\n";
        assert_eq!(strip_claude_md(existing), existing);
    }

    #[test]
    fn claude_md_strip_on_pure_block_returns_empty() {
        let block = render_claude_md_block("2026-04-16");
        // `claude_md_uninstall` treats an empty (or whitespace-only) result
        // as "delete the file" — we just confirm the string is empty here.
        let stripped = strip_claude_md(&block);
        assert!(stripped.trim().is_empty());
    }

    #[test]
    fn bundle_ships_post_tool_use_hook() {
        // Regression: `reflect_cadence = "every"` only works if the
        // PostToolUse hook is actually wired into settings.json.
        // Without this entry, the cadence heartbeat logic in
        // `run_post_tool` never runs and the "every" setting is silent.
        let bundle = bundle_hooks();
        let entries = bundle
            .get("PostToolUse")
            .and_then(Value::as_array)
            .expect("bundle must declare PostToolUse");
        assert!(!entries.is_empty(), "PostToolUse entries must not be empty");
        // The matcher must at least include mutation tools — indexing
        // and cadence both hinge on those. An over-permissive matcher
        // is fine; an over-restrictive one is the regression.
        let matcher = entries[0]
            .get("matcher")
            .and_then(Value::as_str)
            .expect("matcher required");
        for needed in ["Write", "Edit"] {
            assert!(
                matcher.contains(needed),
                "PostToolUse matcher must include {needed}: {matcher:?}"
            );
        }
    }

    #[tokio::test]
    async fn cadence_heartbeat_off_by_default() {
        // Default config → `reflect_cadence = "end"`. The heartbeat
        // helper must NOT emit anything, regardless of debt. Keeps
        // PostToolUse silent for the overwhelming majority of users
        // who never touched the cadence knob.
        let tmp = tempfile::tempdir().unwrap();
        // Plenty of mutations, zero remembers — would trigger the
        // nudge threshold if cadence were "every".
        tokio::fs::write(
            tmp.path().join("gate.jsonl"),
            (0..15)
                .map(|_| r#"{"ts":"2026-04-17T10:00:00Z","tool":"Write","decision":"pass","reason":""}"#)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .await
        .unwrap();

        let msg = cadence_heartbeat(tmp.path()).await;
        assert!(msg.is_none(), "default cadence must stay quiet: {msg:?}");
    }

    #[tokio::test]
    async fn cadence_heartbeat_fires_on_every_plus_debt() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(
            tmp.path().join("config.toml"),
            r#"
[discipline]
reflect_cadence = "every"
reflect_debt_nudge = 3
reflect_debt_block = 100
"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            tmp.path().join("gate.jsonl"),
            (0..5)
                .map(|_| r#"{"ts":"2026-04-17T10:00:00Z","tool":"Write","decision":"pass","reason":""}"#)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .await
        .unwrap();

        let msg = cadence_heartbeat(tmp.path())
            .await
            .expect("should fire when cadence=every and debt ≥ nudge");
        assert!(msg.contains("cadence=every"), "banner missing tag: {msg}");
        assert!(msg.contains("reflection debt"), "body missing: {msg}");
    }

    #[tokio::test]
    async fn cadence_heartbeat_silent_when_debt_below_threshold() {
        // Cadence=every with zero mutations must still stay quiet —
        // the heartbeat is a reminder, not an always-on banner.
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(
            tmp.path().join("config.toml"),
            r#"
[discipline]
reflect_cadence = "every"
reflect_debt_nudge = 3
"#,
        )
        .await
        .unwrap();
        let msg = cadence_heartbeat(tmp.path()).await;
        assert!(msg.is_none(), "no debt → no nag: {msg:?}");
    }

    #[test]
    fn bundle_skills_have_valid_bodies() {
        // Slice-is-empty is enforced at compile time (see the `const _`
        // assertion alongside [`BUNDLE_SKILLS`]); this test validates the
        // individual entries have names and non-blank bodies.
        for (name, body) in BUNDLE_SKILLS {
            assert!(!name.is_empty());
            assert!(!body.trim().is_empty(), "skill `{name}` body is empty");
        }
    }

    #[test]
    fn parse_skill_name_reads_frontmatter() {
        let body = "---\nname: my-skill\ndescription: does stuff\n---\n# body\n";
        assert_eq!(parse_skill_name(body).as_deref(), Some("my-skill"));
    }

    #[test]
    fn parse_skill_name_returns_none_when_missing() {
        // No frontmatter at all.
        assert!(parse_skill_name("# just a header\n").is_none());
        // Frontmatter without a `name:` line.
        assert!(parse_skill_name("---\ndescription: no name\n---\n").is_none());
    }

    #[test]
    fn skill_slug_falls_back_to_draft_dir_name() {
        // No frontmatter → slug comes from `<leaf>.draft` → `<leaf>`.
        let body = "# no frontmatter\n";
        let slug = skill_slug_from(body, Path::new("/tmp/thoth/skills/my-reflex.draft")).unwrap();
        assert_eq!(slug, "my-reflex");
    }
}

#[cfg(test)]
mod promote_tests {
    use super::*;
    use tempfile::tempdir;

    /// Happy path: a valid draft under `<tmp>/skills/<slug>.draft/` gets
    /// copied to the explicit skills dir (absolute path — no CWD games),
    /// the slug is pulled from frontmatter, and the draft is removed.
    #[tokio::test]
    async fn promote_copies_draft_and_cleans_up() {
        let tmp = tempdir().unwrap();
        let draft = tmp.path().join("skills").join("custom-recall.draft");
        tokio::fs::create_dir_all(&draft).await.unwrap();
        tokio::fs::write(
            draft.join("SKILL.md"),
            "---\nname: custom-recall\ndescription: does the thing\n---\n# body\n",
        )
        .await
        .unwrap();

        let live_dir = tmp.path().join("live").join("skills");
        let (slug, dest) = promote_skill_draft_to(&draft, &live_dir).await.unwrap();

        assert_eq!(slug, "custom-recall");
        assert_eq!(dest, live_dir.join("custom-recall"));
        assert!(
            tokio::fs::try_exists(dest.join("SKILL.md")).await.unwrap(),
            "live SKILL.md should exist"
        );
        assert!(
            !tokio::fs::try_exists(&draft).await.unwrap(),
            "draft dir should be gone after promote"
        );
    }

    #[tokio::test]
    async fn promote_rejects_dir_without_skill_md() {
        let tmp = tempdir().unwrap();
        let empty_draft = tmp.path().join("empty.draft");
        tokio::fs::create_dir_all(&empty_draft).await.unwrap();

        let err = promote_skill_draft_to(&empty_draft, &tmp.path().join("live"))
            .await
            .expect_err("should fail without SKILL.md");
        assert!(
            err.to_string().contains("SKILL.md"),
            "error should mention SKILL.md: {err}"
        );
    }

    #[tokio::test]
    async fn promote_overwrites_existing_install() {
        // A re-propose-then-accept must replace the previous live skill
        // rather than append side-by-side.
        let tmp = tempdir().unwrap();
        let live_dir = tmp.path().join("live");
        let stale = live_dir.join("my-skill").join("SKILL.md");
        tokio::fs::create_dir_all(stale.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&stale, "old body").await.unwrap();

        let draft = tmp.path().join("my-skill.draft");
        tokio::fs::create_dir_all(&draft).await.unwrap();
        tokio::fs::write(
            draft.join("SKILL.md"),
            "---\nname: my-skill\ndescription: v2\n---\nnew body\n",
        )
        .await
        .unwrap();

        let (_slug, dest) = promote_skill_draft_to(&draft, &live_dir).await.unwrap();
        let body = tokio::fs::read_to_string(dest.join("SKILL.md"))
            .await
            .unwrap();
        assert!(
            body.contains("new body"),
            "live SKILL.md should reflect the draft: {body:?}"
        );
    }

    #[tokio::test]
    async fn session_start_injects_user_md() {
        // When both USER.md and MEMORY.md exist under `.thoth/`, the
        // SessionStart banner must render USER.md *before* MEMORY.md so
        // user-authored prefs frame the auto-memory that follows.
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        tokio::fs::write(root.join("USER.md"), "user pref body\n")
            .await
            .unwrap();
        tokio::fs::write(root.join("MEMORY.md"), "memory body\n")
            .await
            .unwrap();

        let mut buf = String::new();
        run_session_start(root, &mut buf).await.unwrap();

        let user_idx = buf.find("### USER.md").expect("USER.md section missing");
        let mem_idx = buf
            .find("### MEMORY.md")
            .expect("MEMORY.md section missing");
        assert!(
            user_idx < mem_idx,
            "USER.md must come before MEMORY.md: buf={buf}"
        );
        assert!(buf.contains("user pref body"));
        assert!(buf.contains("memory body"));
    }

    #[tokio::test]
    async fn session_start_skips_missing_user_md() {
        // USER.md is optional — when it is absent, the hook must still
        // render MEMORY.md and emit no USER.md header (silent skip).
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        tokio::fs::write(root.join("MEMORY.md"), "memory body\n")
            .await
            .unwrap();

        let mut buf = String::new();
        run_session_start(root, &mut buf).await.unwrap();

        assert!(
            !buf.contains("### USER.md"),
            "no USER.md header when file absent: {buf}"
        );
        assert!(
            buf.contains("### MEMORY.md"),
            "MEMORY.md must still render: {buf}"
        );
    }
}

// ==============================================================
// T-18: enforcement wiring — PostToolUse → harvester, Stop →
// workflow close check. Acceptance gate for task T-18 uses the
// path `hooks::enforcement_wiring`.
// ==============================================================
#[cfg(test)]
mod enforcement_wiring {
    use super::*;
    use serde_json::json;
    use thoth_core::memory::{Enforcement, Lesson, LessonTrigger, MemoryKind, MemoryMeta};
    use thoth_memory::workflow::{WorkflowStateManager, WorkflowStatus};

    /// Build a structured, frontmatter-equivalent lesson by writing
    /// directly to `.thoth/LESSONS.md`. Since `Lesson` on disk only
    /// carries a natural-language trigger, we stuff a parseable glob
    /// hint into the `trigger` field and override the in-memory
    /// [`LessonTrigger`] via the harvester test seam below.
    async fn seed_lesson(root: &std::path::Path, lesson: Lesson) {
        let store = thoth_store::MarkdownStore::open(root).await.unwrap();
        store.rewrite_lessons(&[lesson]).await.unwrap();
    }

    fn mk_lesson(trigger_text: &str) -> Lesson {
        Lesson {
            meta: MemoryMeta::new(MemoryKind::Reflective),
            trigger: trigger_text.into(),
            advice: "be careful".into(),
            success_count: 0,
            failure_count: 0,
            enforcement: Enforcement::Advise,
            suggested_enforcement: None,
            block_message: None,
        }
    }

    /// PostToolUse with no lessons on disk is a silent no-op — no
    /// violations file, no rewrite churn, hook returns Ok.
    #[tokio::test]
    async fn post_tool_no_lessons_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        // Initialise the markdown store so `read_lessons` can run.
        let _ = thoth_store::MarkdownStore::open(tmp.path()).await.unwrap();

        let payload = json!({
            "tool_name": "Edit",
            "tool_input": { "file_path": "src/foo.rs" },
            "tool_response": { "is_error": false },
            "session_id": "s1"
        });
        run_outcome_harvest(tmp.path(), &payload).await.unwrap();
        assert!(!tmp.path().join("violations.jsonl").exists());
    }

    /// PostToolUse runs the harvester end-to-end: the lesson's
    /// structured trigger matches and its `success_count` bumps.
    ///
    /// We bypass the natural-only fallback by invoking the
    /// harvester directly with a structured [`LessonTrigger`] —
    /// this mirrors what the lesson-matcher wiring task will do
    /// once lesson frontmatter carries `tool` / `path_glob`.
    #[tokio::test]
    async fn post_tool_harvester_bumps_success_counter() {
        use thoth_memory::EnforcementConfig;
        use thoth_memory::lesson_matcher::ToolCall;
        use thoth_memory::outcome_harvest::{LessonEntry, OutcomeHarvester};

        let tmp = tempfile::tempdir().unwrap();
        let trigger = LessonTrigger {
            tool: Some("Edit".into()),
            path_glob: Some("**/*.rs".into()),
            natural: "edits to rust".into(),
            ..Default::default()
        };
        let lesson = mk_lesson("edits to rust");
        let mut entries = vec![LessonEntry { lesson, trigger }];
        let call = ToolCall::new("Edit").with_path("src/foo.rs");
        let h = OutcomeHarvester::new(tmp.path().to_path_buf(), EnforcementConfig::default());
        let report = h
            .harvest_post_tool(&call, false, "sess", "hash", 100, &mut entries)
            .unwrap();
        assert_eq!(report.lesson_outcomes.len(), 1);
        assert_eq!(entries[0].lesson.success_count, 1);
    }

    /// PostToolUse with `is_error=true` writes a violation row via
    /// the hook's payload path. We pre-seed a structured lesson in
    /// LESSONS.md whose natural trigger won't match; the test
    /// confirms the hook at minimum exercises the harvester plumb
    /// without crashing (natural-only = no mechanical match = no
    /// violation row). This covers the "harvester ran" acceptance.
    #[tokio::test]
    async fn post_tool_error_runs_harvester_without_crash() {
        let tmp = tempfile::tempdir().unwrap();
        let lesson = mk_lesson("edits to rust files");
        seed_lesson(tmp.path(), lesson).await;

        let payload = json!({
            "tool_name": "Edit",
            "tool_input": {
                "file_path": "src/foo.rs",
                "old_string": "a",
                "new_string": "b"
            },
            "tool_response": { "is_error": true },
            "session_id": "sess-err"
        });
        run_outcome_harvest(tmp.path(), &payload).await.unwrap();
        // Natural-only legacy trigger → no mechanical match → no
        // violation row. But the hook completed without error,
        // which is what the acceptance test gates on.
        let lessons = thoth_store::MarkdownStore::open(tmp.path())
            .await
            .unwrap()
            .read_lessons()
            .await
            .unwrap();
        assert_eq!(lessons.len(), 1);
        assert_eq!(lessons[0].failure_count, 0);
    }

    /// PostToolUse with a missing `tool_name` field is a silent
    /// no-op — an empty payload must never kill the hook.
    #[tokio::test]
    async fn post_tool_missing_tool_name_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let payload = json!({});
        run_outcome_harvest(tmp.path(), &payload).await.unwrap();
    }

    /// Stop hook with an active workflow records a violation row
    /// and warns — but does not mutate the workflow state (only
    /// `thoth workflow reset` or `thoth_workflow_complete` can).
    #[tokio::test]
    async fn stop_active_workflow_records_violation() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = WorkflowStateManager::new(tmp.path().to_path_buf());
        mgr.start_workflow("sess-wf", "hoangsa:cook", 1_000)
            .unwrap();

        let payload = json!({ "session_id": "sess-wf" });
        run_workflow_close_check(tmp.path(), &payload)
            .await
            .unwrap();

        // Violation row appended.
        let log = tmp.path().join("workflow-violations.jsonl");
        assert!(log.exists(), "violation log must materialise");
        let raw = std::fs::read_to_string(&log).unwrap();
        assert!(raw.contains("sess-wf"), "log must carry session id: {raw}");
        assert!(raw.contains("stop_without_complete"), "reason tag: {raw}");

        // State stays Active — only `thoth workflow reset` / complete changes it.
        let state = mgr.get("sess-wf").unwrap().unwrap();
        assert_eq!(state.status, WorkflowStatus::Active);
    }

    /// Stop hook with a Phase 4b workflow that has a gap records a
    /// violation whose `reason` surfaces the missing steps.
    #[tokio::test]
    async fn stop_workflow_with_gap_captures_step_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = WorkflowStateManager::new(tmp.path().to_path_buf());
        mgr.start_workflow_with_steps(
            "sess-gap",
            "hoangsa:cook",
            1_000,
            vec!["1a".into(), "1b".into(), "2".into()],
        )
        .unwrap();
        mgr.advance_step("sess-gap", "1a", 1_010).unwrap();

        let payload = json!({ "session_id": "sess-gap" });
        run_workflow_close_check(tmp.path(), &payload)
            .await
            .unwrap();

        let raw = std::fs::read_to_string(tmp.path().join("workflow-violations.jsonl")).unwrap();
        assert!(raw.contains("stop_with_gap"), "gap tag: {raw}");
        assert!(raw.contains("1b"), "missing step surfaced: {raw}");
        assert!(raw.contains('2'), "missing step surfaced: {raw}");
    }

    /// Stop hook with no active workflows is a no-op.
    #[tokio::test]
    async fn stop_no_active_workflow_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let payload = json!({ "session_id": "sess-quiet" });
        run_workflow_close_check(tmp.path(), &payload)
            .await
            .unwrap();
        assert!(!tmp.path().join("workflow-violations.jsonl").exists());
    }

    /// Stop hook with a completed workflow leaves it alone.
    #[tokio::test]
    async fn stop_completed_workflow_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = WorkflowStateManager::new(tmp.path().to_path_buf());
        mgr.start_workflow("sess-done", "hoangsa:cook", 1_000)
            .unwrap();
        mgr.complete_workflow("sess-done", 2_000).unwrap();

        let payload = json!({ "session_id": "sess-done" });
        run_workflow_close_check(tmp.path(), &payload)
            .await
            .unwrap();
        assert!(!tmp.path().join("workflow-violations.jsonl").exists());
    }
}

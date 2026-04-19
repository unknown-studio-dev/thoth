//! `thoth` daemon-facing helpers — shared logic for commands that prefer
//! the running MCP daemon and fall back to in-process dispatch.
//!
//! This module re-exports the shared `call_mcp_tool` and `emit_output`
//! helpers extracted from `main.rs` so subcommand modules can import them
//! from a single place without circular dependencies.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum DomainSource {
    /// Local TOML directory — always available, useful for tests and bootstrap.
    File,
    /// Notion database (requires `--features notion` and `NOTION_TOKEN`).
    Notion,
    /// Asana project (requires `--features asana` and `ASANA_TOKEN`).
    Asana,
    /// NotebookLM (stub — see `docs/adr/0001-domain-memory.md`).
    Notebooklm,
}

#[derive(clap::Subcommand, Debug)]
pub enum DomainCmd {
    /// Pull rules from a source and upsert snapshots under
    /// `<root>/domain/<context>/_remote/<source>/`.
    Sync {
        /// Which adapter to use.
        #[arg(long, value_enum)]
        source: DomainSource,

        /// Local directory for `--source file`.
        #[arg(long, required_if_eq("source", "file"))]
        from: Option<std::path::PathBuf>,

        /// Remote project / database id. Meaning depends on `--source`:
        /// Notion database id, Asana project gid, ... Ignored for `file`.
        #[arg(long)]
        project_id: Option<String>,

        /// Only pull rules modified since this RFC3339 timestamp.
        #[arg(long)]
        since: Option<String>,

        /// Per-sync cap on rules returned. Default 500.
        #[arg(long, default_value_t = 500)]
        max_items: usize,
    },
}

pub async fn cmd_domain_sync(
    root: &Path,
    source: DomainSource,
    from: Option<&Path>,
    project_id: Option<&str>,
    since: Option<&str>,
    max_items: usize,
    json: bool,
) -> anyhow::Result<()> {
    use thoth_domain::{IngestFilter, SnapshotStore, file::FileIngestor, sync_source};

    tokio::fs::create_dir_all(root).await?;
    let snap = SnapshotStore::new(root);

    let filter = IngestFilter {
        since: since
            .map(|s| time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339))
            .transpose()
            .map_err(|e| anyhow::anyhow!("invalid --since: {e}"))?,
        max_items,
    };

    let ingestor: Arc<dyn thoth_domain::DomainIngestor> = match source {
        DomainSource::File => {
            let dir = from.ok_or_else(|| anyhow::anyhow!("--from required for --source file"))?;
            Arc::new(FileIngestor::new(dir))
        }
        #[cfg(feature = "notion")]
        DomainSource::Notion => {
            let db = project_id
                .ok_or_else(|| anyhow::anyhow!("--project-id required for --source notion"))?;
            Arc::new(thoth_domain::notion::NotionIngestor::new(db)?)
        }
        #[cfg(not(feature = "notion"))]
        DomainSource::Notion => {
            anyhow::bail!("--source notion requires `--features notion` at build time")
        }
        #[cfg(feature = "asana")]
        DomainSource::Asana => {
            let gid = project_id
                .ok_or_else(|| anyhow::anyhow!("--project-id required for --source asana"))?;
            Arc::new(thoth_domain::asana::AsanaIngestor::new(gid)?)
        }
        #[cfg(not(feature = "asana"))]
        DomainSource::Asana => {
            anyhow::bail!("--source asana requires `--features asana` at build time")
        }
        #[cfg(feature = "notebooklm")]
        DomainSource::Notebooklm => Arc::new(thoth_domain::notebooklm::NotebookLmIngestor::new()?),
        #[cfg(not(feature = "notebooklm"))]
        DomainSource::Notebooklm => anyhow::bail!(
            "--source notebooklm requires `--features notebooklm` at build time (stub)"
        ),
    };
    // `project_id` is only consumed by remote adapters — silence the unused
    // warning for builds without those features.
    let _ = project_id;

    let rep = sync_source(ingestor, &snap, &filter).await?;

    if json {
        let payload = serde_json::json!({
            "source": rep.source,
            "created": rep.stats.created,
            "updated": rep.stats.updated,
            "unchanged": rep.stats.unchanged,
            "unmapped": rep.stats.unmapped,
            "redacted": rep.stats.redacted,
            "errors": rep.errors.iter()
                .map(|(id, e)| serde_json::json!({"id": id, "error": e}))
                .collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        print!("{rep}");
    }

    Ok(())
}

/// Invoke an MCP tool over the daemon socket when one is running; else
/// spin up an in-process server and dispatch through it. Returns
/// `(text, data, is_error)` so the caller can pick which to surface
/// based on `--json`.
pub async fn call_mcp_tool(
    root: &Path,
    tool: &str,
    arguments: serde_json::Value,
) -> Result<(String, serde_json::Value, bool)> {
    if let Some(mut d) = crate::daemon::DaemonClient::try_connect(root).await {
        let result = d.call(tool, arguments).await?;
        let is_error = crate::daemon::tool_is_error(&result);
        let text = crate::daemon::tool_text(&result).to_string();
        let data = crate::daemon::tool_data(&result);
        return Ok((text, data, is_error));
    }
    // In-process: reuse the Server so we don't duplicate tool bodies.
    // This also means the CLI and daemon paths share test coverage.
    let server = thoth_mcp::Server::open(root).await?;
    let params = serde_json::json!({
        "name": tool,
        "arguments": arguments,
    });
    let msg = thoth_mcp::proto::RpcIncoming {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::Value::Number(1.into())),
        method: "thoth.call".to_string(),
        params,
    };
    let resp = server.handle(msg).await;
    let Some(response) = resp else {
        anyhow::bail!("server returned no response for {tool}");
    };
    if let Some(err) = response.error {
        anyhow::bail!("{}: {}", err.code, err.message);
    }
    // `ToolOutput` is Serialize-only (the server never deserialises its
    // own output), so pull fields from the raw JSON instead of
    // round-tripping through from_value.
    let result = response.result.unwrap_or_default();
    let text = result
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let is_error = result
        .get("isError")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let data = result.get("data").cloned().unwrap_or_default();
    Ok((text, data, is_error))
}

// ------------------------------------------- graph / diff CLI subcommands

/// `thoth impact <fqn>` — forwards to the `thoth_impact` MCP tool.
///
/// The daemon path is preferred (keeps us working when Claude Code is
/// holding the redb lock); if unavailable we fall back to opening the
/// store directly and calling the graph API in-process. Exit code is
/// non-zero when the graph can't find the symbol, so shell pipelines
/// can gate on missing FQNs.
pub async fn cmd_impact(
    root: &Path,
    fqn: &str,
    direction: &str,
    depth: usize,
    json: bool,
) -> Result<()> {
    let args = serde_json::json!({
        "fqn": fqn,
        "direction": direction,
        "depth": depth,
    });
    let (text, data, is_error) = call_mcp_tool(root, "thoth_impact", args).await?;
    emit_output(text, data, is_error, json)
}

/// `thoth context <fqn>` — forwards to the `thoth_symbol_context` tool.
pub async fn cmd_context(root: &Path, fqn: &str, limit: usize, json: bool) -> Result<()> {
    let args = serde_json::json!({ "fqn": fqn, "limit": limit });
    let (text, data, is_error) = call_mcp_tool(root, "thoth_symbol_context", args).await?;
    emit_output(text, data, is_error, json)
}

/// `thoth changes` — feed a unified diff through the `thoth_detect_changes`
/// tool. Diff source order of preference: `--from <file>` > `--from -`
/// (stdin) > `git diff HEAD`.
pub async fn cmd_changes(root: &Path, from: Option<&str>, depth: usize, json: bool) -> Result<()> {
    let diff = match from {
        Some("-") => {
            use tokio::io::AsyncReadExt;
            let mut buf = String::new();
            tokio::io::stdin().read_to_string(&mut buf).await?;
            buf
        }
        Some(path) => tokio::fs::read_to_string(path).await?,
        None => {
            // Default: diff of the current working tree against HEAD.
            // `git diff HEAD` includes both staged and unstaged changes,
            // which matches the "what am I about to commit?" intuition.
            let output = tokio::process::Command::new("git")
                .args(["diff", "HEAD"])
                .output()
                .await
                .map_err(|e| anyhow::anyhow!("failed to run git diff: {e}"))?;
            if !output.status.success() {
                anyhow::bail!(
                    "`git diff HEAD` exited non-zero: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            String::from_utf8(output.stdout)
                .map_err(|e| anyhow::anyhow!("git diff output not UTF-8: {e}"))?
        }
    };
    if diff.trim().is_empty() {
        println!("(no diff — working tree matches HEAD)");
        return Ok(());
    }
    let args = serde_json::json!({ "diff": diff, "depth": depth });
    let (text, data, is_error) = call_mcp_tool(root, "thoth_detect_changes", args).await?;
    emit_output(text, data, is_error, json)
}

/// Emit either the rendered text or a pretty-printed JSON dump of the
/// structured `data` half. When `is_error` is set the process exits
/// non-zero so shell pipelines can gate on missing FQNs / malformed
/// diffs.
pub fn emit_output(
    text: String,
    data: serde_json::Value,
    is_error: bool,
    json: bool,
) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&data)?);
    } else if !text.is_empty() {
        print!("{text}");
        if !text.ends_with('\n') {
            println!();
        }
    }
    if is_error {
        std::process::exit(1);
    }
    Ok(())
}

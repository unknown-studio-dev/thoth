//! `thoth-mcp` — an MCP (Model Context Protocol) stdio server exposing
//! Thoth's recall/remember/index capabilities to any MCP-aware client
//! (Claude Agent SDK, Claude Code, Cowork, Cursor, Zed, ...).
//!
//! See the [crate-level docs](thoth_mcp) for the wire protocol details and
//! the tool catalog.
//!
//! # Usage
//!
//! ```text
//! thoth-mcp                   # serve on stdio; log to stderr
//! THOTH_ROOT=/path/.thoth thoth-mcp
//! ```

use std::path::PathBuf;

use thoth_mcp::{Server, run_stdio};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Logs must go to stderr; stdout is reserved for the JSON-RPC transport.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let root = std::env::var("THOTH_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(".thoth"));

    tracing::info!(root = %root.display(), "thoth-mcp starting");

    let server = Server::open(&root).await?;
    run_stdio(server).await?;

    tracing::info!("thoth-mcp exiting");
    Ok(())
}

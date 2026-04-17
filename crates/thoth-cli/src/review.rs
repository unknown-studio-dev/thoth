//! Background review runner.
//!
//! Orchestrates the full background review lifecycle:
//!
//! 1. Build context from on-disk state (episodes, gate log, memory).
//! 2. Enrich with `git diff --stat`.
//! 3. Select backend: `claude` CLI (subscription) or Anthropic API.
//! 4. Send a single prompt → parse structured JSON response.
//! 5. Persist facts/lessons/skills via [`background_review::persist_review`].
//! 6. Bump the `.last-review` watermark.

use std::path::Path;

use anyhow::{Context, bail};
use thoth_memory::background_review::{
    ReviewReport, build_review_context, parse_review_response, persist_review, render_prompt,
};

/// Run the background review end-to-end.
///
/// `backend` is one of `"auto"`, `"cli"`, or `"api"`. On `"auto"` the
/// function checks `ANTHROPIC_API_KEY` and falls back to `claude` CLI.
pub async fn run_review(root: &Path, backend: &str) -> anyhow::Result<ReviewReport> {
    // 1. Build context.
    let mut ctx = build_review_context(root)
        .await
        .context("failed to build review context")?;

    // 2. Enrich with git diff --stat (best-effort).
    ctx.git_stat = git_diff_stat().await.unwrap_or_default();

    // 3. Render prompt.
    let prompt = render_prompt(&ctx);

    // 4. Call LLM.
    let response = match resolve_backend(backend) {
        #[cfg(feature = "anthropic")]
        Backend::Api(key) => call_api(&prompt, &key).await?,
        Backend::Cli => call_cli(&prompt).await?,
    };

    // 5. Parse response.
    let output =
        parse_review_response(&response).map_err(|e| anyhow::anyhow!("{e}"))?;

    // 6. Persist.
    let report = persist_review(root, output)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // 7. Bump watermark.
    if let Err(e) = thoth_memory::mark_last_review(root).await {
        tracing::warn!(error = %e, "background-review: failed to bump watermark");
    }

    Ok(report)
}

// ------------------------------------------------------------------ backend

enum Backend {
    Cli,
    #[cfg(feature = "anthropic")]
    Api(String),
}

fn resolve_backend(requested: &str) -> Backend {
    match requested {
        #[cfg(feature = "anthropic")]
        "api" => match std::env::var("ANTHROPIC_API_KEY") {
            Ok(k) if !k.is_empty() => Backend::Api(k),
            _ => {
                tracing::warn!("background-review: api backend requested but ANTHROPIC_API_KEY not set, falling back to cli");
                Backend::Cli
            }
        },
        #[cfg(feature = "anthropic")]
        "auto" => match std::env::var("ANTHROPIC_API_KEY") {
            Ok(k) if !k.is_empty() => Backend::Api(k),
            _ => Backend::Cli,
        },
        _ => Backend::Cli,
    }
}

// --------------------------------------------------------- backend: claude CLI

async fn call_cli(prompt: &str) -> anyhow::Result<String> {
    use tokio::io::AsyncWriteExt;

    // Pipe the prompt via stdin instead of a CLI arg — prompts can
    // exceed the OS argument-length limit (macOS ARG_MAX ≈ 256 KiB).
    // `--bare` skips hooks/LSP/CLAUDE.md so the review doesn't
    // recurse into Thoth's own discipline loop.
    let mut child = tokio::process::Command::new("claude")
        .args(["--print", "--output-format", "text", "--dangerously-skip-permissions"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn `claude` CLI — is it installed and in PATH?")?;

    // Write prompt to stdin, then close it so claude starts processing.
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(prompt.as_bytes()).await?;
        // drop closes the pipe
    }

    let output = child
        .wait_with_output()
        .await
        .context("claude CLI process failed")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("claude CLI exited with {}: {stderr}", output.status);
    }

    String::from_utf8(output.stdout)
        .context("claude CLI output is not valid UTF-8")
}

// ------------------------------------------------------- backend: Anthropic API

#[cfg(feature = "anthropic")]
async fn call_api(prompt: &str, api_key: &str) -> anyhow::Result<String> {
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "model": "claude-haiku-4-5-20251001",
        "max_tokens": 1024,
        "messages": [
            { "role": "user", "content": prompt }
        ]
    });

    let resp = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .context("Anthropic API request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("Anthropic API returned {status}: {text}");
    }

    let json: serde_json::Value =
        resp.json().await.context("failed to parse API response")?;

    json.get("content")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first())
        .and_then(|block| block.get("text"))
        .and_then(|t| t.as_str())
        .map(String::from)
        .ok_or_else(|| anyhow::anyhow!("unexpected API response shape"))
}

// ------------------------------------------------------------------ helpers

async fn git_diff_stat() -> anyhow::Result<String> {
    let output = tokio::process::Command::new("git")
        .args(["diff", "--stat", "HEAD"])
        .output()
        .await?;

    if !output.status.success() {
        return Ok(String::new());
    }

    String::from_utf8(output.stdout).context("git output not UTF-8")
}

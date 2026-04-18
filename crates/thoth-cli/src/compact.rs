//! `thoth compact` — LLM-driven rewrite of MEMORY.md / LESSONS.md.
//!
//! Unlike `thoth review` (which *appends* new insights from the current
//! session), compact reads the **entire** MEMORY.md + LESSONS.md, asks
//! an LLM to consolidate reworded near-duplicates into a smaller set of
//! canonical entries, and **rewrites** both files in place.
//!
//! Architecturally this shares the backend dispatch with
//! [`crate::review::call_backend`] — same `background_review_backend`
//! and `background_review_model` config, same Haiku-default, same
//! stdin-piped `claude --print --model` subprocess with
//! `--dangerously-skip-permissions`. Only the prompt and response
//! handling differ.
//!
//! Safety: the original files are copied to timestamped `.bak-<unix>`
//! siblings before the rewrite. `--dry-run` skips the write entirely
//! and prints the LLM-proposed output instead, so the user can eyeball
//! it first.

use std::path::Path;

use anyhow::{Context, bail};
use thoth_core::{Fact, Lesson, MemoryKind, MemoryMeta};
use thoth_store::markdown::MarkdownStore;

use crate::review::call_backend;

/// Parsed LLM output: a *full replacement* list for each file. Anything
/// the model omits is effectively dropped.
#[derive(Debug, Default, serde::Deserialize)]
pub struct CompactOutput {
    /// Consolidated facts that should replace MEMORY.md entirely.
    #[serde(default)]
    pub facts: Vec<CompactFact>,
    /// Consolidated lessons that should replace LESSONS.md entirely.
    #[serde(default)]
    pub lessons: Vec<CompactLesson>,
}

/// A single compacted fact.
#[derive(Debug, serde::Deserialize)]
pub struct CompactFact {
    /// Fact body. First line becomes the heading (same convention as
    /// `Fact::text` / `append_fact`).
    pub text: String,
    /// Optional tags, merged from source entries.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// A single compacted lesson.
#[derive(Debug, serde::Deserialize)]
pub struct CompactLesson {
    /// Condition that should trigger the lesson.
    pub trigger: String,
    /// Advice body.
    pub advice: String,
}

/// Summary of what compact did.
#[derive(Debug, Default)]
pub struct CompactReport {
    /// Fact count before.
    pub facts_before: usize,
    /// Fact count after compaction (or proposed, in dry-run).
    pub facts_after: usize,
    /// Lesson count before.
    pub lessons_before: usize,
    /// Lesson count after compaction (or proposed, in dry-run).
    pub lessons_after: usize,
    /// Path of the MEMORY.md backup (empty in dry-run).
    pub memory_backup: String,
    /// Path of the LESSONS.md backup (empty in dry-run).
    pub lessons_backup: String,
}

/// Run the compaction end-to-end.
///
/// `backend` and `model` follow the same fallback semantics as
/// [`crate::review::run_review`]: empty → use `background_review_*`
/// from config.toml.
pub async fn run_compact(
    root: &Path,
    backend: &str,
    model: &str,
    dry_run: bool,
) -> anyhow::Result<CompactReport> {
    let md = MarkdownStore::open(root)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let facts = md.read_facts().await.map_err(|e| anyhow::anyhow!("{e}"))?;
    let lessons = md
        .read_lessons()
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    if facts.is_empty() && lessons.is_empty() {
        bail!("MEMORY.md and LESSONS.md are both empty — nothing to compact");
    }

    let prompt = render_prompt(&facts, &lessons);
    let response = call_backend(&prompt, backend, model)
        .await
        .context("compact: LLM call failed")?;
    let out = parse_response(&response).context("compact: response parse failed")?;

    // Sanity: refuse to wipe the store if the LLM returned a trivially
    // empty or suspiciously small result. `persist_review` doesn't need
    // this check because it's append-only; compact *overwrites*.
    let shrink_factor_facts = safe_ratio(out.facts.len(), facts.len());
    let shrink_factor_lessons = safe_ratio(out.lessons.len(), lessons.len());
    if out.facts.is_empty() && !facts.is_empty() {
        bail!(
            "compact: LLM returned 0 facts but MEMORY.md had {}. Refusing to wipe. \
             Re-run with a different --model or inspect the response manually.",
            facts.len()
        );
    }
    if out.lessons.is_empty() && !lessons.is_empty() {
        bail!(
            "compact: LLM returned 0 lessons but LESSONS.md had {}. Refusing to wipe.",
            lessons.len()
        );
    }
    if shrink_factor_facts < 0.05 || shrink_factor_lessons < 0.05 {
        bail!(
            "compact: proposed output shrinks >95% (facts {}→{}, lessons {}→{}). \
             This is almost certainly a bad response — refusing to overwrite. \
             Re-run with --dry-run to inspect.",
            facts.len(),
            out.facts.len(),
            lessons.len(),
            out.lessons.len()
        );
    }

    let mut report = CompactReport {
        facts_before: facts.len(),
        facts_after: out.facts.len(),
        lessons_before: lessons.len(),
        lessons_after: out.lessons.len(),
        ..Default::default()
    };

    if dry_run {
        print_dry_run(&out);
        return Ok(report);
    }

    // Back up originals. Same timestamp for both so the user can pair
    // them when rolling back.
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    report.memory_backup = backup(root, "MEMORY.md", ts).await?;
    report.lessons_backup = backup(root, "LESSONS.md", ts).await?;

    // Prune old backups so `.thoth/` doesn't accumulate one pair per
    // compact run. `compact_backup_keep = 0` disables pruning (user
    // opted to retain everything). The count includes the backup we
    // just created, so `keep = 2` keeps the 2 most recent pairs.
    let disc = thoth_memory::DisciplineConfig::load_or_default(root).await;
    if disc.compact_backup_keep > 0 {
        prune_backups(root, "MEMORY.md", disc.compact_backup_keep as usize).await;
        prune_backups(root, "LESSONS.md", disc.compact_backup_keep as usize).await;
    }

    let new_facts: Vec<Fact> = out
        .facts
        .into_iter()
        .map(|f| Fact {
            meta: MemoryMeta::new(MemoryKind::Semantic),
            text: f.text,
            tags: f.tags,
        })
        .collect();
    let new_lessons: Vec<Lesson> = out
        .lessons
        .into_iter()
        .map(|l| Lesson {
            meta: MemoryMeta::new(MemoryKind::Reflective),
            trigger: l.trigger,
            advice: l.advice,
            success_count: 0,
            failure_count: 0,
        })
        .collect();

    md.rewrite_facts(&new_facts)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    md.rewrite_lessons(&new_lessons)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    Ok(report)
}

// -------------------------------------------------------------- helpers

fn safe_ratio(num: usize, den: usize) -> f32 {
    if den == 0 {
        1.0
    } else {
        num as f32 / den as f32
    }
}

async fn backup(root: &Path, filename: &str, ts: u64) -> anyhow::Result<String> {
    let src = root.join(filename);
    if !src.exists() {
        return Ok(String::new());
    }
    let dst = root.join(format!("{filename}.bak-{ts}"));
    tokio::fs::copy(&src, &dst)
        .await
        .with_context(|| format!("failed to back up {filename}"))?;
    Ok(dst.display().to_string())
}

/// Delete `<root>/<filename>.bak-<ts>` files beyond the `keep` most
/// recent (by the numeric `<ts>` suffix, not filesystem mtime — the
/// suffix is set atomically when the backup is created and can't drift
/// from `touch`). Best-effort: any I/O error is logged but doesn't
/// fail the surrounding compact.
async fn prune_backups(root: &Path, filename: &str, keep: usize) {
    let prefix = format!("{filename}.bak-");
    let mut entries = match tokio::fs::read_dir(root).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "compact: prune read_dir failed");
            return;
        }
    };
    let mut candidates: Vec<(u64, std::path::PathBuf)> = Vec::new();
    loop {
        match entries.next_entry().await {
            Ok(Some(e)) => {
                let p = e.path();
                let Some(name) = p.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                let Some(rest) = name.strip_prefix(&prefix) else {
                    continue;
                };
                if let Ok(ts) = rest.parse::<u64>() {
                    candidates.push((ts, p));
                }
            }
            Ok(None) => break,
            Err(e) => {
                tracing::warn!(error = %e, "compact: prune iter failed");
                break;
            }
        }
    }
    // Newest first; keep the first `keep`, delete the rest.
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    for (_ts, path) in candidates.into_iter().skip(keep) {
        if let Err(e) = tokio::fs::remove_file(&path).await {
            tracing::warn!(error = %e, path = %path.display(), "compact: prune remove failed");
        }
    }
}

fn print_dry_run(out: &CompactOutput) {
    println!("## Proposed facts ({})\n", out.facts.len());
    for (i, f) in out.facts.iter().enumerate() {
        let head = f.text.lines().next().unwrap_or("").trim();
        println!("{:>3}. {head}", i + 1);
    }
    println!("\n## Proposed lessons ({})\n", out.lessons.len());
    for (i, l) in out.lessons.iter().enumerate() {
        println!("{:>3}. when {}", i + 1, l.trigger.trim());
    }
}

// ------------------------------------------------------------- prompt/parse

fn render_prompt(facts: &[Fact], lessons: &[Lesson]) -> String {
    // Serialise every fact/lesson into a numbered list the LLM can
    // cite. We don't truncate — compact's whole point is for the model
    // to see every entry so it can merge across the full set. The 158F
    // + 87L pathological case is ~20k input tokens, well within Haiku's
    // 200k context.
    let mut facts_block = String::new();
    for (i, f) in facts.iter().enumerate() {
        facts_block.push_str(&format!("{}. {}\n", i + 1, f.text.trim()));
        if !f.tags.is_empty() {
            facts_block.push_str(&format!("   tags: {}\n", f.tags.join(", ")));
        }
    }

    let mut lessons_block = String::new();
    for (i, l) in lessons.iter().enumerate() {
        lessons_block.push_str(&format!(
            "{}. when {}\n   advice: {}\n",
            i + 1,
            l.trigger.trim(),
            l.advice.trim()
        ));
    }

    format!(
        r#"You are consolidating a project's long-term memory. The facts and lessons below accumulated over many sessions and contain heavy redundancy — multiple rewordings of the same underlying point, often with slightly different wording or detail level.

Your job: produce a **replacement** list — fewer entries, each one a merged canonical version. This overwrites the current files, so an entry you omit is GONE.

## Facts ({n_facts} total)
{facts_block}

## Lessons ({n_lessons} total)
{lessons_block}

## Rules
1. Preserve every distinct insight. Near-duplicates (same subject, different wording) merge into ONE entry that keeps the best detail from each source (file paths, commit hashes, dates, numeric constants — all of these matter and must survive).
2. Drop entries that are pure restatements with no added information.
3. Keep entries terse but specific. Prefer the longer/more-specific wording when merging.
4. Do NOT invent facts. Every output entry must trace to at least one input entry.
5. Target 15-30% of the original size (aim for high compression, but never drop unique info).
6. Keep tags from source entries; merge tag lists when merging entries.
7. Preserve absolute dates (e.g. "2026-04-17"). Never rewrite dates to relative form.

## Output
Return ONLY valid JSON (no markdown fences, no commentary) matching this schema:
{{"facts":[{{"text":"...","tags":["..."]}}, ...],"lessons":[{{"trigger":"...","advice":"..."}}, ...]}}

Remember: anything you omit is permanently deleted from memory. Lean toward keeping rather than dropping when uncertain.
"#,
        n_facts = facts.len(),
        facts_block = facts_block,
        n_lessons = lessons.len(),
        lessons_block = lessons_block,
    )
}

fn parse_response(text: &str) -> anyhow::Result<CompactOutput> {
    let trimmed = text.trim();
    // Strip markdown fences if the model slipped one in.
    let json_str = if trimmed.starts_with("```") {
        let start = trimmed.find('{').unwrap_or(0);
        let end = trimmed.rfind('}').map(|i| i + 1).unwrap_or(trimmed.len());
        &trimmed[start..end]
    } else {
        trimmed
    };
    serde_json::from_str::<CompactOutput>(json_str).context("compact response not valid JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_handles_plain_json() {
        let raw =
            r#"{"facts":[{"text":"hello","tags":["a"]}],"lessons":[{"trigger":"x","advice":"y"}]}"#;
        let out = parse_response(raw).unwrap();
        assert_eq!(out.facts.len(), 1);
        assert_eq!(out.lessons[0].trigger, "x");
    }

    #[test]
    fn parse_strips_markdown_fences() {
        let raw = "```json\n{\"facts\":[],\"lessons\":[]}\n```";
        let out = parse_response(raw).unwrap();
        assert!(out.facts.is_empty());
    }

    #[test]
    fn safe_ratio_handles_zero_denominator() {
        assert!((safe_ratio(0, 0) - 1.0).abs() < 1e-6);
        assert!((safe_ratio(5, 10) - 0.5).abs() < 1e-6);
    }
}

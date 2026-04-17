//! Background review: context assembly, prompt rendering, and response
//! parsing for the Hermes-style background memory curation.
//!
//! This module is intentionally IO-light — it reads local files and
//! formats strings. The actual LLM call and process spawning live in
//! the CLI crate (`thoth-cli/src/review.rs`).

use std::path::Path;

use thoth_core::Result;
use thoth_store::episodes::EpisodeLog;
use thoth_store::markdown::MarkdownStore;

// ------------------------------------------------------------------ types

/// Assembled context for a background review prompt.
#[derive(Debug)]
pub struct ReviewContext {
    /// Current contents of MEMORY.md (truncated).
    pub memory_md: String,
    /// Current contents of LESSONS.md (truncated).
    pub lessons_md: String,
    /// Human-readable summaries of recent episodes.
    pub recent_events: Vec<String>,
    /// File paths touched in this session (from gate.jsonl).
    pub files_changed: Vec<String>,
    /// Output of `git diff --stat` (injected by the CLI caller).
    pub git_stat: String,
}

/// A single fact extracted from the review response.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct FactEntry {
    /// Fact body text. First line becomes the heading in MEMORY.md.
    pub text: String,
    /// Optional tags for filtering.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// A single lesson extracted from the review response.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct LessonEntry {
    /// Situation that should trigger this lesson.
    pub trigger: String,
    /// What to do (or avoid) in that situation.
    pub advice: String,
}

/// A skill proposal extracted from the review response.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SkillEntry {
    /// URL-safe slug for the skill directory.
    pub slug: String,
    /// Full SKILL.md body (with frontmatter).
    pub body: String,
    /// Lesson triggers that motivated this skill.
    #[serde(default)]
    pub source_triggers: Vec<String>,
}

/// Parsed review output — zero or more of each entry type.
#[derive(Debug, Default, serde::Deserialize)]
pub struct ReviewOutput {
    /// Facts to persist.
    #[serde(default)]
    pub facts: Vec<FactEntry>,
    /// Lessons to persist.
    #[serde(default)]
    pub lessons: Vec<LessonEntry>,
    /// Skills to propose as drafts.
    #[serde(default)]
    pub skills: Vec<SkillEntry>,
}

/// Report of what was actually persisted (after dedup).
#[derive(Debug, Default)]
pub struct ReviewReport {
    /// Number of new facts appended to MEMORY.md.
    pub facts_added: usize,
    /// Number of new lessons appended to LESSONS.md.
    pub lessons_added: usize,
    /// Number of skill drafts written.
    pub skills_proposed: usize,
}

// -------------------------------------------------------------- context build

const MEMORY_TRUNCATE: usize = 2000;
const LESSONS_TRUNCATE: usize = 2000;
const MAX_RECENT_EVENTS: usize = 20;

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        // Don't split a multi-byte char.
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

/// Build review context from on-disk state. The `git_stat` field is
/// left empty — the CLI caller fills it in via `git diff --stat`.
pub async fn build_review_context(root: &Path) -> Result<ReviewContext> {
    let md = MarkdownStore::open(root).await?;
    let memory_raw = md.read_facts().await.unwrap_or_default();
    let lessons_raw = md.read_lessons().await.unwrap_or_default();

    // Format facts as bullet points for the prompt.
    let memory_md = {
        let mut buf = String::new();
        for f in &memory_raw {
            buf.push_str(&format!("- {}\n", f.text.lines().next().unwrap_or("")));
        }
        truncate(&buf, MEMORY_TRUNCATE)
    };

    let lessons_md = {
        let mut buf = String::new();
        for l in &lessons_raw {
            buf.push_str(&format!("- when {}: {}\n", l.trigger, l.advice));
        }
        truncate(&buf, LESSONS_TRUNCATE)
    };

    // Recent episodes from the SQLite log.
    let episodes = EpisodeLog::open(thoth_store::StoreRoot::episodes_path(root)).await?;
    let recent = episodes.recent(MAX_RECENT_EVENTS).await?;
    let recent_events: Vec<String> = recent
        .iter()
        .map(|h| {
            let kind = h.event.kind_str();
            let summary = h.event.one_line_summary();
            format!("[{kind}] {summary}")
        })
        .collect();

    // Files changed from gate.jsonl — extract unique paths.
    let files_changed = extract_changed_files(root).await;

    Ok(ReviewContext {
        memory_md,
        lessons_md,
        recent_events,
        files_changed,
        git_stat: String::new(),
    })
}

/// Scan gate.jsonl for mutation entries and extract unique file paths.
async fn extract_changed_files(root: &Path) -> Vec<String> {
    use tokio::io::AsyncBufReadExt;

    let path = root.join("gate.jsonl");
    let file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };

    let reader = tokio::io::BufReader::new(file);
    let mut lines = reader.lines();
    let mut paths = std::collections::BTreeSet::new();

    while let Ok(Some(line)) = lines.next_line().await {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) {
            let decision = val.get("decision").and_then(|v| v.as_str()).unwrap_or("");
            if !matches!(decision, "pass" | "nudge") {
                continue;
            }
            if let Some(p) = val.get("path").and_then(|v| v.as_str())
                && !p.is_empty()
            {
                paths.insert(p.to_string());
            }
        }
    }

    paths.into_iter().collect()
}

// ------------------------------------------------------------- prompt render

/// Render the review prompt. Designed for ~800-1200 input tokens.
pub fn render_prompt(ctx: &ReviewContext) -> String {
    let events_section = if ctx.recent_events.is_empty() {
        "(no events recorded)".to_string()
    } else {
        ctx.recent_events.join("\n")
    };

    let files_section = if ctx.files_changed.is_empty() {
        "(none)".to_string()
    } else {
        ctx.files_changed.join("\n")
    };

    let git_section = if ctx.git_stat.is_empty() {
        "(unavailable)".to_string()
    } else {
        ctx.git_stat.clone()
    };

    format!(
        r#"You are a memory curator for a coding session. Analyze the activity below and extract durable knowledge worth remembering across future sessions.

## Current Memory
{memory}

## Current Lessons
{lessons}

## Session Activity
{events}

## Files Changed
{files}

## Git Diff Summary
{git}

## Instructions
Return ONLY valid JSON (no markdown fences, no commentary):
{{"facts":[{{"text":"...","tags":["..."]}}],"lessons":[{{"trigger":"...","advice":"..."}}],"skills":[{{"slug":"...","body":"...","source_triggers":["..."]}}]}}

Quality gates — only include entries that:
- Save a future session at least one round-trip (a recall that would have failed, a mistake that would repeat)
- Encode a decision, convention, or non-obvious pattern — not a raw file path or symbol name
- Are NOT already present in the current memory/lessons above
- Are NOT obvious from a 5-minute README scan

If nothing is worth saving, return: {{"facts":[],"lessons":[],"skills":[]}}"#,
        memory = ctx.memory_md,
        lessons = ctx.lessons_md,
        events = events_section,
        files = files_section,
        git = git_section,
    )
}

// ------------------------------------------------------------ response parse

/// Parse the LLM response into structured review output. Tolerant of
/// markdown fences and leading/trailing whitespace.
pub fn parse_review_response(text: &str) -> Result<ReviewOutput> {
    let trimmed = text.trim();

    // Strip markdown code fences if present.
    let json_str = if trimmed.starts_with("```") {
        let start = trimmed.find('{').unwrap_or(0);
        let end = trimmed.rfind('}').map(|i| i + 1).unwrap_or(trimmed.len());
        &trimmed[start..end]
    } else {
        trimmed
    };

    Ok(serde_json::from_str::<ReviewOutput>(json_str)?)
}

// ---------------------------------------------------------------- persist

/// Persist review output into the markdown store, deduplicating against
/// existing entries. Returns a report of what was actually added.
pub async fn persist_review(root: &Path, output: ReviewOutput) -> Result<ReviewReport> {
    let md = MarkdownStore::open(root).await?;
    let mut report = ReviewReport::default();

    // Dedup facts.
    let existing_facts: std::collections::HashSet<String> = md
        .read_facts()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|f| f.text.lines().next().unwrap_or("").trim().to_ascii_lowercase())
        .collect();

    for entry in output.facts {
        let key = entry
            .text
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if key.is_empty() || existing_facts.contains(&key) {
            continue;
        }
        let fact = thoth_core::Fact {
            meta: thoth_core::MemoryMeta::new(thoth_core::MemoryKind::Semantic),
            text: entry.text,
            tags: entry.tags,
        };
        if let Err(e) = md.append_fact(&fact).await {
            tracing::warn!(error = %e, "background-review: failed to append fact");
            continue;
        }
        report.facts_added += 1;
    }

    // Dedup lessons.
    let existing_triggers: std::collections::HashSet<String> = md
        .read_lessons()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|l| l.trigger.trim().to_ascii_lowercase())
        .collect();

    for entry in output.lessons {
        let key = entry.trigger.trim().to_ascii_lowercase();
        if key.is_empty() || existing_triggers.contains(&key) {
            continue;
        }
        let lesson = thoth_core::Lesson {
            meta: thoth_core::MemoryMeta::new(thoth_core::MemoryKind::Reflective),
            trigger: entry.trigger,
            advice: entry.advice,
            success_count: 0,
            failure_count: 0,
        };
        if let Err(e) = md.append_lesson(&lesson).await {
            tracing::warn!(error = %e, "background-review: failed to append lesson");
            continue;
        }
        report.lessons_added += 1;
    }

    // Skills → propose as drafts (same flow as thoth_skill_propose).
    for entry in output.skills {
        let slug = entry
            .slug
            .trim()
            .to_ascii_lowercase()
            .replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', "-");
        if slug.is_empty() {
            continue;
        }
        let draft_dir = root.join("skills").join(format!("{slug}.draft"));
        if let Err(e) = tokio::fs::create_dir_all(&draft_dir).await {
            tracing::warn!(error = %e, slug = %slug, "background-review: mkdir failed");
            continue;
        }
        if let Err(e) = tokio::fs::write(draft_dir.join("SKILL.md"), &entry.body).await {
            tracing::warn!(error = %e, slug = %slug, "background-review: write failed");
            continue;
        }
        report.skills_proposed += 1;
    }

    Ok(report)
}

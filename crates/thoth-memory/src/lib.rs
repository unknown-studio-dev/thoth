//! # thoth-memory
//!
//! The memory lifecycle layer — the "policy core" of Thoth.
//!
//! It owns:
//!
//! - The markdown source of truth (`MEMORY.md`, `LESSONS.md`, `skills/*/`)
//! - TTL-based forgetting for episodic memory
//! - Confidence evolution for lessons (reinforcement from outcomes)
//! - The **nudge** flow in Mode::Full: at session end, ask the
//!   [`Synthesizer`](thoth_core::Synthesizer) whether any new fact,
//!   lesson, or skill should be persisted
//!
//! Design goals (per `DESIGN.md` §5 and §9):
//! - Deterministic in Mode::Zero (TTL + hard delete only).
//! - LLM-curated in Mode::Full (nudge instead of algorithmic salience
//!   scoring — see Hermes).
//! - Markdown files remain first-class so a human can review in git.

#![deny(rust_2018_idioms)]
#![warn(missing_docs)]

use std::path::Path;
use thoth_core::{Event, Result, Synthesizer};
use thoth_store::episodes::EpisodeLog;
use thoth_store::markdown::MarkdownStore;
use time::{Duration, OffsetDateTime};

/// Config controlling the memory lifecycle.
#[derive(Debug, Clone)]
pub struct MemoryConfig {
    /// Episodic TTL in days. Default 30.
    pub episodic_ttl_days: u32,
    /// Max number of episodes retained before capacity-based eviction.
    pub max_episodes: usize,
    /// Lesson confidence floor below which lessons are forgotten.
    pub lesson_floor: f32,
    /// Whether to invoke the LLM nudge at session end (Mode::Full only).
    pub enable_nudge: bool,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            episodic_ttl_days: 30,
            max_episodes: 50_000,
            lesson_floor: 0.2,
            enable_nudge: true,
        }
    }
}

/// Top-level memory manager.
pub struct MemoryManager {
    /// Markdown-backed source of truth.
    pub md: MarkdownStore,
    /// Episodic log — owns TTL / capacity eviction.
    pub episodes: EpisodeLog,
    /// Active configuration.
    pub config: MemoryConfig,
}

impl MemoryManager {
    /// Open the memory manager against `<root>/`.
    ///
    /// The episodic log lives at `<root>/index/episodes.sqlite`.
    pub async fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let md = MarkdownStore::open(root).await?;
        let episodes = EpisodeLog::open(root.join("index").join("episodes.sqlite")).await?;
        Ok(Self {
            md,
            episodes,
            config: MemoryConfig::default(),
        })
    }

    /// Open with a caller-supplied [`EpisodeLog`] — lets callers share one
    /// log across the indexer, retriever, and memory manager.
    pub async fn open_with(root: impl AsRef<Path>, episodes: EpisodeLog) -> Result<Self> {
        let md = MarkdownStore::open(root).await?;
        Ok(Self {
            md,
            episodes,
            config: MemoryConfig::default(),
        })
    }

    /// Run the scheduled forgetting pass.
    ///
    /// In Mode::Zero this is a pair of deterministic SQL deletes over the
    /// episodic log:
    ///
    /// 1. Drop every event older than [`MemoryConfig::episodic_ttl_days`].
    /// 2. Then cap the log at [`MemoryConfig::max_episodes`] rows,
    ///    keeping the newest.
    ///
    /// Lesson-confidence eviction is deferred to the Mode::Full nudge flow
    /// (which is where confidence signals actually come from), so
    /// [`ForgetReport::lessons_dropped`] is always `0` here.
    pub async fn forget_pass(&self) -> Result<ForgetReport> {
        let ttl_days = self.config.episodic_ttl_days as i64;
        let cutoff = OffsetDateTime::now_utc() - Duration::days(ttl_days);
        let cutoff_ns_i128 = cutoff.unix_timestamp_nanos();
        // Clamp to i64 since that's what the store uses.
        let cutoff_ns = cutoff_ns_i128
            .clamp(i64::MIN as i128, i64::MAX as i128) as i64;

        let episodes_ttl = self.episodes.delete_older_than(cutoff_ns).await?;
        let episodes_cap = self.episodes.trim_to_capacity(self.config.max_episodes).await?;

        tracing::info!(
            ttl_days,
            episodes_ttl,
            episodes_cap,
            "memory: forget pass complete"
        );

        Ok(ForgetReport {
            episodes_ttl,
            episodes_cap,
            lessons_dropped: 0,
        })
    }

    /// Run the Mode::Full nudge (invokes `Synthesizer::critique`).
    ///
    /// Walks the most recent outcome events, asks the synthesizer to
    /// critique each one, and appends any proposed [`thoth_core::Lesson`]s
    /// to `LESSONS.md`. Duplicate lessons (by trigger, case-insensitive) are
    /// silently skipped so the nudge is idempotent across sessions.
    ///
    /// `window` bounds how many recent episodes are scanned. A value of
    /// `0` means "use the default of 64".
    pub async fn nudge(
        &self,
        synth: &dyn Synthesizer,
        window: usize,
    ) -> Result<NudgeReport> {
        if !self.config.enable_nudge {
            return Ok(NudgeReport::default());
        }
        let window = if window == 0 { 64 } else { window };

        // Pull recent episodes, filter to outcome events.
        let recent = self.episodes.recent(window).await?;
        let outcomes: Vec<_> = recent
            .into_iter()
            .filter_map(|hit| match hit.event {
                Event::OutcomeObserved { outcome, .. } => Some(outcome),
                _ => None,
            })
            .collect();
        if outcomes.is_empty() {
            return Ok(NudgeReport::default());
        }

        // Known triggers so we don't re-append the same lesson every session.
        let existing: std::collections::HashSet<String> = self
            .md
            .read_lessons()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|l| l.trigger.trim().to_ascii_lowercase())
            .collect();

        let mut report = NudgeReport::default();
        for o in &outcomes {
            match synth.critique(o).await {
                Ok(Some(lesson)) => {
                    let key = lesson.trigger.trim().to_ascii_lowercase();
                    if key.is_empty() || existing.contains(&key) {
                        continue;
                    }
                    if let Err(e) = self.md.append_lesson(&lesson).await {
                        tracing::warn!(error = %e, "nudge: failed to append lesson");
                        continue;
                    }
                    report.lessons_added += 1;
                }
                Ok(None) => {}
                Err(e) => {
                    // Don't abort the whole pass on a single provider error.
                    tracing::warn!(error = %e, "nudge: critique failed for an outcome");
                }
            }
        }

        tracing::info!(
            outcomes = outcomes.len(),
            lessons_added = report.lessons_added,
            "memory: nudge pass complete"
        );
        Ok(report)
    }
}



/// Stats produced by a forgetting pass.
#[derive(Debug, Clone, Default)]
pub struct ForgetReport {
    /// How many episodes were dropped for TTL.
    pub episodes_ttl: u64,
    /// How many episodes were dropped for capacity.
    pub episodes_cap: u64,
    /// How many lessons were dropped for low confidence.
    pub lessons_dropped: u64,
}

/// Stats produced by a nudge pass.
#[derive(Debug, Clone, Default)]
pub struct NudgeReport {
    /// Facts proposed by the LLM and accepted.
    pub facts_added: u64,
    /// Lessons proposed by the LLM and accepted.
    pub lessons_added: u64,
    /// Skills proposed by the LLM and accepted.
    pub skills_added: u64,
}

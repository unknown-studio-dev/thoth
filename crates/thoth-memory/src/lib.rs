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

pub mod working;
pub use working::{WorkingMemory, WorkingNote};

use std::path::Path;
use thoth_core::{Event, Result, Synthesizer};
use thoth_store::episodes::EpisodeLog;
use thoth_store::markdown::MarkdownStore;
use thoth_store::StoreRoot;
use time::{Duration, OffsetDateTime};

/// Config controlling the memory lifecycle.
///
/// Loaded from `<root>/config.toml` via [`MemoryConfig::load_or_default`].
/// Unknown keys are ignored and missing keys fall back to the compiled
/// defaults (equivalent to [`MemoryConfig::default`]).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryConfig {
    /// Episodic TTL in days. Default 30.
    pub episodic_ttl_days: u32,
    /// Max number of episodes retained before capacity-based eviction.
    pub max_episodes: usize,
    /// Lesson confidence floor. Below this ratio (success / (success +
    /// failure + 1)) a lesson is considered harmful and dropped — but only
    /// once it has [`MemoryConfig::lesson_min_attempts`] attempts on record.
    pub lesson_floor: f32,
    /// Minimum number of success+failure attempts before a lesson can be
    /// dropped for low confidence. Prevents a single unlucky pass from
    /// killing a freshly-minted lesson.
    pub lesson_min_attempts: u32,
    /// Exponential decay rate per day (DESIGN §9).
    /// `effective = salience · exp(-λ·days_idle) · ln(e + access_count)`.
    /// At the default `λ=0.02` a never-retrieved memory decays to ~0.67
    /// of its original salience after 30 days, ~0.45 after 60 days.
    pub decay_lambda: f32,
    /// Retention floor for the decay formula. Memories whose effective
    /// score falls below this are dropped by the forget pass. A value of
    /// `0.0` disables decay-based eviction.
    pub decay_floor: f32,
    /// Whether to invoke the LLM nudge at session end (Mode::Full only).
    pub enable_nudge: bool,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            episodic_ttl_days: 30,
            max_episodes: 50_000,
            lesson_floor: 0.2,
            lesson_min_attempts: 3,
            decay_lambda: 0.02,
            decay_floor: 0.05,
            enable_nudge: true,
        }
    }
}

/// TOML file schema — mirrors the `[memory]` table in `<root>/config.toml`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ConfigFile {
    memory: MemoryConfig,
}

impl MemoryConfig {
    /// Load `<root>/config.toml` if it exists, otherwise fall back to
    /// [`MemoryConfig::default`]. Malformed files emit a `warn!` and still
    /// fall back — the user's memory must not become unusable because they
    /// mistyped a key.
    pub async fn load_or_default(root: &Path) -> Self {
        let path = root.join("config.toml");
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(),
                    "memory: could not read config.toml, using defaults");
                return Self::default();
            }
        };
        match toml::from_str::<ConfigFile>(&text) {
            Ok(cf) => cf.memory,
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(),
                    "memory: config.toml parse error, using defaults");
                Self::default()
            }
        }
    }
}

/// Effective retention score per DESIGN §9:
/// `salience · exp(-λ·days_idle) · ln(e + access_count)`.
///
/// Using `ln(e + access_count)` instead of the literal `ln(1 + …)` from
/// the design text so that a fresh item (access_count = 0) evaluates to
/// `salience · decay`, i.e. time-scaled salience — not zero. Retrieved
/// items then earn a multiplicative boost of `ln(e + N) ≥ 1`.
pub fn effective_retention_score(
    salience: f32,
    access_count: u64,
    last_accessed_at: OffsetDateTime,
    now: OffsetDateTime,
    lambda: f32,
) -> f32 {
    let days_idle = ((now - last_accessed_at).whole_seconds() as f64 / 86_400.0).max(0.0);
    let decay = (-(lambda as f64) * days_idle).exp() as f32;
    let usage = (std::f64::consts::E + access_count as f64).ln() as f32;
    salience * decay * usage
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
    /// The episodic log lives at `<root>/episodes.db` (per DESIGN §7).
    /// Any legacy `<root>/index/episodes.sqlite` is migrated in-place by
    /// [`StoreRoot::open`] — a bare `MemoryManager::open` that runs
    /// *without* also opening a [`StoreRoot`] will skip the migration and
    /// create a fresh log.
    pub async fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let md = MarkdownStore::open(root).await?;
        let episodes = EpisodeLog::open(StoreRoot::episodes_path(root)).await?;
        Ok(Self {
            md,
            episodes,
            config: MemoryConfig::load_or_default(root).await,
        })
    }

    /// Open with a caller-supplied [`EpisodeLog`] — lets callers share one
    /// log across the indexer, retriever, and memory manager.
    pub async fn open_with(root: impl AsRef<Path>, episodes: EpisodeLog) -> Result<Self> {
        let root = root.as_ref();
        let md = MarkdownStore::open(root).await?;
        Ok(Self {
            md,
            episodes,
            config: MemoryConfig::load_or_default(root).await,
        })
    }

    /// Run the scheduled forgetting pass (DESIGN §9).
    ///
    /// Four deterministic steps, in order:
    ///
    /// 1. **TTL** — drop every episode older than
    ///    [`MemoryConfig::episodic_ttl_days`].
    /// 2. **Capacity** — cap the log at [`MemoryConfig::max_episodes`] rows,
    ///    keeping the newest.
    /// 3. **Decay** — for every surviving episode, compute
    ///    [`effective_retention_score`] from its stored `salience`,
    ///    `access_count`, and last-accessed timestamp. Any row whose
    ///    score falls below [`MemoryConfig::decay_floor`] is dropped.
    ///    Setting `decay_floor` to `0.0` disables this step, so
    ///    deterministic-only (Mode::Zero) deployments can opt out.
    /// 4. **Lesson confidence** — drop any lesson whose
    ///    `success / (success + failure + 1)` ratio is below
    ///    [`MemoryConfig::lesson_floor`] once it has accumulated at least
    ///    [`MemoryConfig::lesson_min_attempts`] attempts. Lessons with
    ///    fewer attempts are left alone so a single unlucky run doesn't
    ///    kill a newborn rule.
    pub async fn forget_pass(&self) -> Result<ForgetReport> {
        let ttl_days = self.config.episodic_ttl_days as i64;
        let now = OffsetDateTime::now_utc();
        let cutoff = now - Duration::days(ttl_days);
        let cutoff_ns_i128 = cutoff.unix_timestamp_nanos();
        // Clamp to i64 since that's what the store uses.
        let cutoff_ns = cutoff_ns_i128
            .clamp(i64::MIN as i128, i64::MAX as i128) as i64;

        let episodes_ttl = self.episodes.delete_older_than(cutoff_ns).await?;
        let episodes_cap = self
            .episodes
            .trim_to_capacity(self.config.max_episodes)
            .await?;

        let episodes_decayed = self.decay_evict_episodes(now).await?;

        let lessons_dropped = self.drop_low_confidence_lessons().await?;

        tracing::info!(
            ttl_days,
            episodes_ttl,
            episodes_cap,
            episodes_decayed,
            lessons_dropped,
            "memory: forget pass complete"
        );

        Ok(ForgetReport {
            episodes_ttl,
            episodes_cap,
            episodes_decayed,
            lessons_dropped,
        })
    }

    /// Compute [`effective_retention_score`] for every surviving episode
    /// and delete any whose score dips below [`MemoryConfig::decay_floor`].
    /// Returns the number of rows dropped.
    async fn decay_evict_episodes(&self, now: OffsetDateTime) -> Result<u64> {
        let floor = self.config.decay_floor;
        if floor <= 0.0 {
            return Ok(0);
        }
        let lambda = self.config.decay_lambda;
        let rows = self.episodes.iter_with_decay_meta().await?;
        let mut to_drop: Vec<i64> = Vec::new();
        for (id, salience, access_count, last_ns) in rows {
            let last = OffsetDateTime::from_unix_timestamp_nanos(last_ns as i128)
                .unwrap_or(OffsetDateTime::UNIX_EPOCH);
            let score = effective_retention_score(salience, access_count, last, now, lambda);
            if score < floor {
                to_drop.push(id);
            }
        }
        if to_drop.is_empty() {
            return Ok(0);
        }
        self.episodes.delete_by_ids(&to_drop).await
    }

    /// Drop any lesson that has had at least
    /// [`MemoryConfig::lesson_min_attempts`] retrievals and whose confidence
    /// ratio is below [`MemoryConfig::lesson_floor`].
    async fn drop_low_confidence_lessons(&self) -> Result<u64> {
        let floor = self.config.lesson_floor;
        let min_attempts = self.config.lesson_min_attempts as u64;
        // `floor <= 0` or `min_attempts == 0` are both "never drop".
        if floor <= 0.0 || min_attempts == 0 {
            return Ok(0);
        }
        let lessons = self.md.read_lessons().await?;
        let before = lessons.len();
        let kept: Vec<_> = lessons
            .into_iter()
            .filter(|l| {
                let attempts = l.success_count + l.failure_count;
                if attempts < min_attempts {
                    return true;
                }
                // +1 in the denominator matches MemoryConfig::lesson_floor's docs.
                let ratio = l.success_count as f32 / (attempts as f32 + 1.0);
                ratio >= floor
            })
            .collect();
        let dropped = (before - kept.len()) as u64;
        if dropped > 0 {
            self.md.rewrite_lessons(&kept).await?;
        }
        Ok(dropped)
    }

    /// Run the Mode::Full nudge (invokes `Synthesizer::critique` +
    /// `Synthesizer::propose_session_memory`).
    ///
    /// Two passes against the synthesizer:
    ///
    /// 1. **Per-outcome critique** — every recent `OutcomeObserved` event
    ///    is handed to `critique`, which may return a [`Lesson`].
    /// 2. **Session-level proposal** — the full recent event window is
    ///    handed to `propose_session_memory`, which may return a bundle of
    ///    [`Fact`]s, [`Lesson`]s, and [`Skill`]s.
    ///
    /// Results from both passes are de-duplicated against what's already
    /// on disk (facts by normalized text, lessons by trigger, skills by
    /// slug) so the nudge is idempotent across sessions.
    ///
    /// `window` bounds how many recent episodes are scanned. `0` means
    /// "use the default of 64".
    pub async fn nudge(
        &self,
        synth: &dyn Synthesizer,
        window: usize,
    ) -> Result<NudgeReport> {
        if !self.config.enable_nudge {
            return Ok(NudgeReport::default());
        }
        let window = if window == 0 { 64 } else { window };

        let recent = self.episodes.recent(window).await?;
        let events: Vec<Event> = recent.iter().map(|h| h.event.clone()).collect();
        let outcomes: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::OutcomeObserved { outcome, .. } => Some(outcome.clone()),
                _ => None,
            })
            .collect();

        let mut report = NudgeReport::default();

        // -- pass 1: per-outcome critique -----------------------------------
        let mut existing_triggers: std::collections::HashSet<String> = self
            .md
            .read_lessons()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|l| l.trigger.trim().to_ascii_lowercase())
            .collect();

        for o in &outcomes {
            match synth.critique(o).await {
                Ok(Some(lesson)) => {
                    let key = lesson.trigger.trim().to_ascii_lowercase();
                    if key.is_empty() || existing_triggers.contains(&key) {
                        continue;
                    }
                    if let Err(e) = self.md.append_lesson(&lesson).await {
                        tracing::warn!(error = %e, "nudge: failed to append lesson");
                        continue;
                    }
                    existing_triggers.insert(key);
                    report.lessons_added += 1;
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "nudge: critique failed for an outcome");
                }
            }
        }

        // -- pass 2: session-level proposal ---------------------------------
        if !events.is_empty() {
            match synth.propose_session_memory(&events).await {
                Ok(proposal) => {
                    // Facts — dedup by case-insensitive first-line title.
                    let mut existing_facts: std::collections::HashSet<String> = self
                        .md
                        .read_facts()
                        .await
                        .unwrap_or_default()
                        .into_iter()
                        .map(|f| fact_key(&f.text))
                        .collect();
                    for fact in proposal.facts {
                        let key = fact_key(&fact.text);
                        if key.is_empty() || existing_facts.contains(&key) {
                            continue;
                        }
                        if let Err(e) = self.md.append_fact(&fact).await {
                            tracing::warn!(error = %e, "nudge: failed to append fact");
                            continue;
                        }
                        existing_facts.insert(key);
                        report.facts_added += 1;
                    }

                    // Lessons from the session view (same dedup set as pass 1).
                    for lesson in proposal.lessons {
                        let key = lesson.trigger.trim().to_ascii_lowercase();
                        if key.is_empty() || existing_triggers.contains(&key) {
                            continue;
                        }
                        if let Err(e) = self.md.append_lesson(&lesson).await {
                            tracing::warn!(error = %e, "nudge: failed to append lesson");
                            continue;
                        }
                        existing_triggers.insert(key);
                        report.lessons_added += 1;
                    }

                    // Skills — dedup by slug. `path` should point at a
                    // source directory with a SKILL.md; we copy it in.
                    let mut existing_slugs: std::collections::HashSet<String> = self
                        .md
                        .list_skills()
                        .await
                        .unwrap_or_default()
                        .into_iter()
                        .map(|s| s.slug.trim().to_ascii_lowercase())
                        .collect();
                    for skill in proposal.skills {
                        let slug_key = skill.slug.trim().to_ascii_lowercase();
                        if !slug_key.is_empty() && existing_slugs.contains(&slug_key) {
                            continue;
                        }
                        if skill.path.as_os_str().is_empty() {
                            tracing::warn!(
                                slug = %skill.slug,
                                "nudge: skill proposal has empty path, skipped"
                            );
                            continue;
                        }
                        match self.md.install_from_directory(&skill.path).await {
                            Ok(installed) => {
                                existing_slugs.insert(installed.slug.to_ascii_lowercase());
                                report.skills_added += 1;
                            }
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    slug = %skill.slug,
                                    "nudge: failed to install skill"
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "nudge: session-level proposal failed");
                }
            }
        }

        tracing::info!(
            outcomes = outcomes.len(),
            facts_added = report.facts_added,
            lessons_added = report.lessons_added,
            skills_added = report.skills_added,
            "memory: nudge pass complete"
        );
        Ok(report)
    }
}

/// Normalize a fact body to its case-folded first line — used as the
/// dedup key when the synthesizer proposes a fact that's already on disk.
fn fact_key(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

#[cfg(test)]
mod decay_tests {
    use super::*;

    #[test]
    fn fresh_item_decay_matches_salience_when_just_accessed() {
        let now = OffsetDateTime::now_utc();
        // 0 days idle, 0 accesses → decay = 1.0, usage = ln(e) = 1.0.
        let s = effective_retention_score(0.7, 0, now, now, 0.02);
        assert!((s - 0.7).abs() < 1e-5, "expected 0.7, got {s}");
    }

    #[test]
    fn idle_items_decay_below_floor() {
        let now = OffsetDateTime::now_utc();
        let long_ago = now - Duration::days(365);
        // λ=0.02, 365 days → exp(-7.3) ≈ 0.00067 → well below a 0.05 floor.
        let s = effective_retention_score(1.0, 0, long_ago, now, 0.02);
        assert!(s < 0.05, "expected decay below floor; got {s}");
    }

    #[test]
    fn access_count_boosts_retention() {
        let now = OffsetDateTime::now_utc();
        let idle = now - Duration::days(30);
        let cold = effective_retention_score(0.5, 0, idle, now, 0.02);
        let warm = effective_retention_score(0.5, 100, idle, now, 0.02);
        assert!(warm > cold, "accesses should increase retention");
    }
}



/// Stats produced by a forgetting pass.
#[derive(Debug, Clone, Default)]
pub struct ForgetReport {
    /// How many episodes were dropped for TTL.
    pub episodes_ttl: u64,
    /// How many episodes were dropped for capacity.
    pub episodes_cap: u64,
    /// How many episodes were dropped because their decayed retention
    /// score fell below [`MemoryConfig::decay_floor`].
    pub episodes_decayed: u64,
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

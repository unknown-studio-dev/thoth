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

pub mod reflection;
pub mod working;
pub use reflection::{ReflectionDebt, mark_session_start};
pub use working::{WorkingMemory, WorkingNote};

use std::path::Path;
use thoth_core::{Event, Result, Synthesizer};
use thoth_store::StoreRoot;
use thoth_store::episodes::EpisodeLog;
use thoth_store::markdown::MarkdownStore;
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

/// TOML file schema — mirrors the `[memory]` and `[discipline]` tables in
/// `<root>/config.toml`. We deliberately do NOT `deny_unknown_fields` at
/// the top level because the same file also hosts `[index]`,
/// `[output]`, and other per-crate tables owned by other loaders.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct ConfigFile {
    memory: MemoryConfig,
    #[serde(default)]
    discipline: DisciplineConfig,
}

/// Enforcement policy for the memory-discipline loop.
///
/// Read by hook runners and by the plugin skills; the MCP server itself
/// doesn't enforce anything — it just exposes memory tools. What the
/// `discipline` block controls is **how loud** the plugin skills get when
/// they detect that a lesson was violated or a reflect-step was skipped.
///
/// Two modes:
///
/// - `soft` (default): the skill reminds the agent but never blocks.
/// - `strict`: the skill returns a hard `deny` to Claude Code hooks, which
///   aborts the tool call until the agent re-plans with lessons in hand.
///
/// `global_fallback = true` (default) lets the plugin fall back to the
/// user-level `~/.thoth/` memory when no project-local `.thoth/` exists,
/// so lessons travel across checkouts of scratch repos.
// NOTE: no `deny_unknown_fields` here — the project-wide
// `config.toml` also hosts gate-v2 keys (`gate_window_*`,
// `gate_relevance_threshold`, `gate_telemetry_enabled`, …) that are
// owned by `thoth-mcp/bin/gate.rs`'s own `DisciplineFile` struct. If
// we enforced unknown-field rejection here, every project with a
// normal config would silently fall back to hard-coded defaults —
// masking the very thresholds (`reflect_debt_*`) this struct adds.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default)]
pub struct DisciplineConfig {
    /// `"soft"` (warn only) or `"strict"` (deny on violation). Default
    /// `"soft"` — match the principle of "nudge, don't yoke".
    pub mode: String,
    /// Fall back to `~/.thoth/` memory when the current project has no
    /// `.thoth/` directory. Default `true`.
    pub global_fallback: bool,
    /// Ask for a `thoth.reflect` pass after every tool call (`"every"`) or
    /// only at session end (`"end"`). Default `"end"` — avoid thrash on
    /// trivial edits.
    pub reflect_cadence: String,
    /// Ask for a `thoth.nudge` pass before destructive actions. Default
    /// `true`.
    pub nudge_before_write: bool,
    /// Ask for a `thoth.grounding_check` on any load-bearing factual claim
    /// in the assistant's response. Default `false` (opt-in — it's the
    /// slowest of the three).
    pub grounding_check: bool,
    /// How new facts and lessons land in memory:
    ///
    /// - `"auto"` (default) — `thoth_remember_fact` and
    ///   `thoth_remember_lesson` write straight to `MEMORY.md` / `LESSONS.md`.
    /// - `"review"` — writes land in `MEMORY.pending.md` / `LESSONS.pending.md`
    ///   and a human must run `thoth_memory_promote` (or the CLI equivalent)
    ///   to accept them. Rejected entries are archived with a reason.
    ///
    /// Teams that want hard curation should switch to `"review"`; teams that
    /// trust the agent can stay on `"auto"` and rely on the forget pass +
    /// confidence counters to prune bad memory later.
    pub memory_mode: String,
    /// In `strict` mode, the gate also requires a `nudge_invoked` event
    /// within [`Self::gate_window_secs`] before a `Write`/`Edit`/`Bash`
    /// tool call. This forces the agent to actually expand `thoth.nudge`
    /// (not just run a no-op `thoth_recall`). Default `false`.
    pub gate_require_nudge: bool,
    /// Lessons whose `failure_count / (success_count + failure_count)`
    /// exceeds this ratio (once they have at least
    /// [`Self::quarantine_min_attempts`] attempts) are moved from
    /// `LESSONS.md` to `LESSONS.quarantined.md` during the forget pass.
    /// Default `0.66` — i.e. twice as many failures as successes.
    pub quarantine_failure_ratio: f32,
    /// Minimum `success_count + failure_count` before a lesson is eligible
    /// for quarantine. Default `5` — a freshly minted lesson with one
    /// failure shouldn't get yanked.
    pub quarantine_min_attempts: u32,
    /// Reflection debt — number of **mutations** (successful Write/Edit/
    /// NotebookEdit tool calls, derived from `gate.jsonl`) since the last
    /// `thoth_remember_fact` / `thoth_remember_lesson` call (derived from
    /// `memory-history.jsonl`). Above [`Self::reflect_debt_nudge`] the
    /// hooks surface a soft reminder; above [`Self::reflect_debt_block`]
    /// the gate hard-blocks new mutations until the agent reflects.
    ///
    /// Rationale: pre-action recall is enforced by the gate, but
    /// post-action reflection was previously a prompt contract only —
    /// agents drift. This turns reflection into an enforced loop with
    /// the same mechanism (hook injection + PreToolUse block) that
    /// proved effective for recall.
    ///
    /// Default `10`. Set to `0` to disable the soft reminder.
    pub reflect_debt_nudge: u32,
    /// Reflection debt that triggers a hard gate block on mutations.
    /// Set `THOTH_DEFER_REFLECT=1` to bypass one session when the user
    /// genuinely wants to land a batch before reflecting.
    ///
    /// Default `20`. Set to `0` to disable the hard block.
    pub reflect_debt_block: u32,
}

impl Default for DisciplineConfig {
    fn default() -> Self {
        Self {
            mode: "soft".to_string(),
            global_fallback: true,
            reflect_cadence: "end".to_string(),
            nudge_before_write: true,
            grounding_check: false,
            memory_mode: "auto".to_string(),
            gate_require_nudge: false,
            quarantine_failure_ratio: 0.66,
            quarantine_min_attempts: 5,
            reflect_debt_nudge: 10,
            reflect_debt_block: 20,
        }
    }
}

impl DisciplineConfig {
    /// `true` if new memory should be staged (pending) instead of
    /// auto-committed.
    pub fn requires_review(&self) -> bool {
        self.memory_mode.eq_ignore_ascii_case("review")
    }
}

impl DisciplineConfig {
    /// Load `<root>/config.toml` if it exists, else return defaults.
    ///
    /// Same tolerant behaviour as [`MemoryConfig::load_or_default`]: missing
    /// file → defaults, malformed file → warn + defaults.
    pub async fn load_or_default(root: &Path) -> Self {
        let path = root.join("config.toml");
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(),
                    "discipline: could not read config.toml, using defaults");
                return Self::default();
            }
        };
        Self::parse_or_default(&text, &path)
    }

    /// Sync twin of [`Self::load_or_default`] for callers that can't
    /// spin a tokio runtime (the `thoth-gate` hook binary).
    pub fn load_or_default_sync(root: &Path) -> Self {
        let path = root.join("config.toml");
        let text = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(),
                    "discipline: could not read config.toml, using defaults");
                return Self::default();
            }
        };
        Self::parse_or_default(&text, &path)
    }

    fn parse_or_default(text: &str, path: &Path) -> Self {
        match toml::from_str::<ConfigFile>(text) {
            Ok(cf) => cf.discipline,
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(),
                    "discipline: config.toml parse error, using defaults");
                Self::default()
            }
        }
    }

    /// `true` if mode is `"strict"`.
    pub fn is_strict(&self) -> bool {
        self.mode.eq_ignore_ascii_case("strict")
    }
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
        let cutoff_ns = cutoff_ns_i128.clamp(i64::MIN as i128, i64::MAX as i128) as i64;

        let episodes_ttl = self.episodes.delete_older_than(cutoff_ns).await?;
        let episodes_cap = self
            .episodes
            .trim_to_capacity(self.config.max_episodes)
            .await?;

        let episodes_decayed = self.decay_evict_episodes(now).await?;

        let lessons_dropped = self.drop_low_confidence_lessons().await?;

        // Auto-quarantine: lessons whose failure ratio blew past the
        // discipline threshold. Different from `drop_low_confidence_lessons`
        // in two ways: it's opt-in via `DisciplineConfig`, and it preserves
        // the offending lesson in `LESSONS.quarantined.md` so a human can
        // review and restore it later.
        let lessons_quarantined = self.auto_quarantine_lessons().await?;

        tracing::info!(
            ttl_days,
            episodes_ttl,
            episodes_cap,
            episodes_decayed,
            lessons_dropped,
            lessons_quarantined,
            "memory: forget pass complete"
        );

        Ok(ForgetReport {
            episodes_ttl,
            episodes_cap,
            episodes_decayed,
            lessons_dropped,
            lessons_quarantined,
        })
    }

    /// Move every lesson whose failure ratio has tripped the configured
    /// threshold into `LESSONS.quarantined.md`. Threshold knobs live in
    /// [`DisciplineConfig::quarantine_failure_ratio`] and
    /// [`DisciplineConfig::quarantine_min_attempts`].
    async fn auto_quarantine_lessons(&self) -> Result<u64> {
        let root = self.md.root.clone();
        let dcfg = DisciplineConfig::load_or_default(&root).await;
        if dcfg.quarantine_failure_ratio <= 0.0 || dcfg.quarantine_min_attempts == 0 {
            return Ok(0);
        }
        let min_attempts = dcfg.quarantine_min_attempts as u64;
        let ratio_threshold = dcfg.quarantine_failure_ratio;

        let lessons = self.md.read_lessons().await?;
        let mut to_quarantine: Vec<String> = Vec::new();
        for l in &lessons {
            let attempts = l.success_count + l.failure_count;
            if attempts < min_attempts {
                continue;
            }
            let ratio = l.failure_count as f32 / attempts as f32;
            if ratio >= ratio_threshold {
                to_quarantine.push(l.trigger.trim().to_string());
            }
        }
        if to_quarantine.is_empty() {
            return Ok(0);
        }
        self.md.quarantine_lessons(&to_quarantine).await
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
    pub async fn nudge(&self, synth: &dyn Synthesizer, window: usize) -> Result<NudgeReport> {
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
    /// How many lessons were moved to `LESSONS.quarantined.md` because
    /// their failure ratio exceeded the configured threshold.
    pub lessons_quarantined: u64,
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

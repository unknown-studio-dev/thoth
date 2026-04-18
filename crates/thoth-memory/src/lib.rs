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

pub mod background_review;
pub mod lesson_clusters;
pub mod reflection;
pub mod text_sim;
pub mod working;
pub use lesson_clusters::{
    DEFAULT_CLUSTER_JACCARD, DEFAULT_CLUSTER_MIN_SIZE, LessonCluster, detect_clusters,
};
pub use reflection::{
    ReflectionDebt, mark_last_review, mark_session_start, mutations_since_last_review,
    read_last_review,
};
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
    /// Hard cap for `MEMORY.md` in bytes. Default 3072 (DESIGN-SPEC REQ-02).
    /// A `thoth_remember_fact` that would push the file above this cap
    /// returns a structured [`CapExceededError`] instead of silently
    /// appending — the agent must call `thoth_memory_replace` or
    /// `thoth_memory_remove` first.
    #[serde(default = "default_cap_memory_bytes")]
    pub cap_memory_bytes: usize,
    /// Hard cap for `USER.md` in bytes. Default 1536 (DESIGN-SPEC REQ-02).
    #[serde(default = "default_cap_user_bytes")]
    pub cap_user_bytes: usize,
    /// Hard cap for `LESSONS.md` in bytes. Default 5120 (DESIGN-SPEC REQ-02).
    #[serde(default = "default_cap_lessons_bytes")]
    pub cap_lessons_bytes: usize,
    /// FLEXIBLE content policy (DESIGN-SPEC REQ-12). When `false` (default)
    /// MCP tool handlers only log a warning if a `remember_*` payload looks
    /// like a bare commit sha / ISO date / file path with no invariant.
    /// When `true`, such payloads are rejected with a structured error.
    #[serde(default)]
    pub strict_content_policy: bool,
}

fn default_cap_memory_bytes() -> usize {
    3072
}

fn default_cap_user_bytes() -> usize {
    1536
}

fn default_cap_lessons_bytes() -> usize {
    5120
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
            cap_memory_bytes: default_cap_memory_bytes(),
            cap_user_bytes: default_cap_user_bytes(),
            cap_lessons_bytes: default_cap_lessons_bytes(),
            strict_content_policy: false,
        }
    }
}

/// Which markdown file a verb targets.
///
/// Distinct from [`thoth_core::MemoryKind`] (the five-class taxonomy) —
/// this one only covers the three markdown surfaces exposed by the
/// `thoth_memory_replace` / `thoth_memory_remove` / `thoth_remember_*`
/// verbs introduced in DESIGN-SPEC REQ-04/05/06.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// `MEMORY.md` — project facts.
    Fact,
    /// `LESSONS.md` — lessons learned.
    Lesson,
    /// `USER.md` — user preferences.
    Preference,
}

/// Structured error surfaced via MCP when a write would exceed a hard cap
/// (DESIGN-SPEC REQ-03). The `entries` field lets the agent decide which
/// record to `replace` / `remove` before retrying.
#[derive(Debug, serde::Serialize)]
pub struct CapExceededError {
    /// Which markdown surface was over cap.
    pub kind: MemoryKind,
    /// Size of the file *before* the attempted write, in bytes.
    pub current_bytes: usize,
    /// Configured hard cap, in bytes.
    pub cap_bytes: usize,
    /// Size the file *would have reached* after the attempted write.
    pub attempted_bytes: usize,
    /// Snapshot of current entries so the agent can choose what to drop.
    pub entries: Vec<MemoryEntryPreview>,
    /// Suggested next verb — e.g. "Call thoth_memory_replace or thoth_memory_remove.".
    pub hint: String,
}

/// Preview row describing one entry in a markdown memory file. Used inside
/// [`CapExceededError::entries`] and by the read-side `preview` API.
#[derive(Debug, serde::Serialize)]
pub struct MemoryEntryPreview {
    /// Zero-based index of the entry within the file (top → bottom).
    pub index: usize,
    /// First non-empty line of the entry, truncated to 120 chars.
    pub first_line: String,
    /// Byte size of the full entry (including its trailing newline).
    pub bytes: usize,
    /// Tags parsed off the entry's leading `#tag` markers, if any.
    pub tags: Vec<String>,
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
// NOTE: `deny_unknown_fields` stays off. All gate-v2 keys
// (`gate_window_*`, `gate_relevance_threshold`, `gate_telemetry_enabled`,
// `gate_bash_readonly_prefixes`, `[[discipline.policies]]`) now live on
// this struct — the previous two-loader-one-file split in 2026-04-17's
// `thoth-mcp/bin/gate.rs::DisciplineFile` was the hazard that masked
// `reflect_debt_*` when strictness was briefly enabled. Consolidating
// here means the gate binary reads exactly what the rest of the world
// sees; drift between the two is no longer possible.
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

    // ------------------------------------------------------------------
    // Gate-v2 keys. Consumed by `thoth-mcp/bin/gate.rs` to decide
    // pass / nudge / block on mutation tool calls. Owned here so the
    // project's `config.toml` has a single parser; see the struct-level
    // NOTE above for the hazard consolidation fixes.
    // ------------------------------------------------------------------
    /// Recency shortcut window (seconds). Any `query_issued` event
    /// within this window passes the gate without a relevance check.
    /// Accepts the legacy key name `gate_window_secs` via serde alias
    /// so existing v1-era configs continue to parse. Default `60`.
    #[serde(alias = "gate_window_secs")]
    pub gate_window_short_secs: u64,
    /// Relevance pool window (seconds). Past recalls up to this age
    /// are scored by token overlap with the incoming edit; best score
    /// ≥ `gate_relevance_threshold` passes. Default `1800` (30 min).
    pub gate_window_long_secs: u64,
    /// Containment ratio threshold in `[0.0, 1.0]`. `0.0` disables the
    /// relevance check (recency-only behaviour). Default `0.30`.
    pub gate_relevance_threshold: f64,
    /// Append every gate decision to `<root>/gate.jsonl` when `true`.
    /// Off by default — telemetry is opt-in. Consumed by the gate
    /// binary and by `ReflectionDebt::compute` (for mutation counts).
    pub gate_telemetry_enabled: bool,
    /// Additional Bash command prefixes that bypass the gate.
    /// Additive to the hard-coded built-ins (`cargo test`, `git status`,
    /// `thoth curate`, …) — entries here *extend* the whitelist, they
    /// don't replace it. Default empty.
    pub gate_bash_readonly_prefixes: Vec<String>,
    /// Actor-specific overrides. First entry whose `actor` glob matches
    /// the `THOTH_ACTOR` env var wins; the default policy applies when
    /// none match. Default empty.
    pub policies: Vec<ActorPolicyConfig>,

    // ------------------------------------------------------------------
    // Background review. A lightweight fork of the Hermes-style
    // "background review" that spawns `claude -p` (subscription) or
    // calls the Anthropic API directly to auto-persist facts/lessons
    // mid-session.
    // ------------------------------------------------------------------
    /// Enable periodic background reviews. When `true`, the PostToolUse
    /// hook spawns a detached `thoth review` process every
    /// [`Self::background_review_interval`] mutations. Default `false`
    /// (opt-in).
    pub background_review: bool,
    /// Number of mutations (Write/Edit/NotebookEdit) between background
    /// reviews. Default `50`. Was `10` pre-2026-04-18 but that fires so
    /// often a 50-mutation session can spawn 5+ reviews, each running
    /// through the whole MEMORY.md/LESSONS.md and tending to snowball
    /// reworded near-duplicates.
    pub background_review_interval: u32,
    /// Minimum seconds between two background reviews, regardless of
    /// mutation count. A hard floor on spawn rate so a rapid burst of
    /// edits can't fire back-to-back reviews. Default `600` (10 min).
    pub background_review_min_secs: u64,
    /// Backend for the review LLM call. `"auto"` checks
    /// `ANTHROPIC_API_KEY` → API, else `claude` CLI (subscription).
    /// `"cli"` forces the CLI path. `"api"` forces the API path.
    /// Default `"auto"`.
    pub background_review_backend: String,
    /// Model name passed to the backend. For the CLI backend this
    /// becomes `claude --model <name>`; for the API backend it's the
    /// `model` field in the request body. Default
    /// `"claude-haiku-4-5"` — Haiku is plenty for memory curation, and
    /// leaving this unset on the CLI path previously meant reviews ran
    /// under whatever default the user's interactive session uses
    /// (often Opus), burning tokens.
    pub background_review_model: String,

    /// How many `MEMORY.md.bak-*` / `LESSONS.md.bak-*` files `thoth
    /// compact` keeps around after a successful rewrite. Older
    /// backups (by filename timestamp) are deleted. `0` disables
    /// pruning (keep every backup forever). Default `2`.
    pub compact_backup_keep: u32,
}

/// Per-actor gate policy override. Missing fields inherit from the
/// top-level [`DisciplineConfig`] defaults. Mirrors
/// `[[discipline.policies]]` in `config.toml`.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct ActorPolicyConfig {
    /// Glob matching the `THOTH_ACTOR` env var (e.g. `"hoangsa/*"`,
    /// `"ci-*"`, `"*"`). Empty string matches nothing.
    pub actor: String,
    /// Gate mode for this actor. `"off"` / `"nudge"` / `"strict"`. When
    /// `None`, inherits [`DisciplineConfig::mode`] / the gate's default.
    pub mode: Option<String>,
    /// Recency window override in seconds. Inherits when `None`.
    pub window_short_secs: Option<u64>,
    /// Relevance window override in seconds. Inherits when `None`.
    pub window_long_secs: Option<u64>,
    /// Relevance threshold override in `[0.0, 1.0]`. Inherits when `None`.
    pub relevance_threshold: Option<f64>,
}

/// Gate-v2 defaults, exposed so the gate binary and the config layer
/// agree on concrete numbers without duplicating literals.
pub mod gate_defaults {
    /// Default recency shortcut window in seconds.
    pub const WINDOW_SHORT_SECS: u64 = 60;
    /// Default relevance pool window in seconds.
    pub const WINDOW_LONG_SECS: u64 = 1800;
    /// Default containment-ratio threshold.
    pub const RELEVANCE_THRESHOLD: f64 = 0.30;
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
            gate_window_short_secs: gate_defaults::WINDOW_SHORT_SECS,
            gate_window_long_secs: gate_defaults::WINDOW_LONG_SECS,
            gate_relevance_threshold: gate_defaults::RELEVANCE_THRESHOLD,
            gate_telemetry_enabled: false,
            gate_bash_readonly_prefixes: Vec::new(),
            policies: Vec::new(),
            background_review: false,
            background_review_interval: 50,
            background_review_min_secs: 600,
            background_review_backend: "auto".to_string(),
            background_review_model: "claude-haiku-4-5".to_string(),
            compact_backup_keep: 2,
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

// ---------------------------------------------------------------------------
// DESIGN-SPEC §162-215: cap-aware verbs on the markdown surface.
//
// These live in `thoth-memory` (not `thoth-store`) because the concept of
// a byte cap is a *policy* decision driven by `MemoryConfig`, not a raw
// storage primitive. `MarkdownStoreMemoryExt` is an extension trait so the
// existing `MarkdownStore` in `thoth-store` stays free of policy code while
// the MCP / CLI layers get a single uniform entrypoint for replace, remove,
// preview, preference append, and cap-enforced append.
// ---------------------------------------------------------------------------

const USER_MD: &str = "USER.md";
const MEMORY_MD: &str = "MEMORY.md";
const LESSONS_MD: &str = "LESSONS.md";

/// The `bak-<unix_ts>` suffix attached to a snapshot written *before* any
/// mutating `replace` / `remove` call. Matches `thoth compact`'s convention
/// so downstream pruners can sweep both sources uniformly.
fn backup_suffix(now_unix: i64) -> String {
    format!("bak-{now_unix}")
}

/// Write `<path>.bak-<unix>` iff `path` currently exists. A missing source
/// file is not an error — fresh stores have nothing to preserve.
async fn write_backup(path: &Path) -> Result<()> {
    if !tokio::fs::try_exists(path).await.unwrap_or(false) {
        return Ok(());
    }
    let body = tokio::fs::read(path).await?;
    let ts = OffsetDateTime::now_utc().unix_timestamp();
    let bak = path.with_extension(backup_suffix(ts));
    tokio::fs::write(&bak, body).await?;
    Ok(())
}

/// Path for the given markdown surface inside the store root.
fn md_path(root: &Path, kind: MemoryKind) -> std::path::PathBuf {
    match kind {
        MemoryKind::Fact => root.join(MEMORY_MD),
        MemoryKind::Lesson => root.join(LESSONS_MD),
        MemoryKind::Preference => root.join(USER_MD),
    }
}

/// Map the three-surface `MemoryKind` onto the `kind` string field of
/// `memory-history.jsonl` so replace/remove ops are discoverable by
/// [`reflection::count_remembers`].
fn history_kind(kind: MemoryKind) -> &'static str {
    match kind {
        MemoryKind::Fact => "fact",
        MemoryKind::Lesson => "lesson",
        MemoryKind::Preference => "preference",
    }
}

/// Split a markdown file into entry blocks on `### ` level-3 headings.
///
/// Every block includes its own heading line and every following line until
/// the next `### ` heading (or EOF). The file preamble (anything before the
/// first heading — typically a `# TITLE\n` line) is returned separately so
/// callers can re-emit it verbatim when rewriting. Trailing blank lines on
/// each block are preserved so round-tripping is byte-identical.
fn split_entries(text: &str) -> (String, Vec<String>) {
    let mut preamble = String::new();
    let mut entries: Vec<String> = Vec::new();
    let mut current: Option<String> = None;

    for line in text.split_inclusive('\n') {
        if line.starts_with("### ") {
            if let Some(buf) = current.take() {
                entries.push(buf);
            }
            current = Some(String::from(line));
        } else if let Some(buf) = current.as_mut() {
            buf.push_str(line);
        } else {
            preamble.push_str(line);
        }
    }
    if let Some(buf) = current.take() {
        entries.push(buf);
    }
    (preamble, entries)
}

/// Re-assemble a file body from its preamble + entries. Guarantees a
/// trailing newline so downstream appends compose cleanly.
fn join_entries(preamble: &str, entries: &[String]) -> String {
    let mut out = String::from(preamble);
    for e in entries {
        out.push_str(e);
    }
    out
}

/// Extract the first non-empty line of an entry, minus the leading `### `
/// heading marker, truncated to 120 chars.
fn entry_first_line(entry: &str) -> String {
    for line in entry.lines() {
        let l = line.trim_start_matches("### ").trim();
        if !l.is_empty() {
            return l.chars().take(120).collect();
        }
    }
    String::new()
}

/// Extract the `tags: a, b, c` line inside an entry, if any.
fn entry_tags(entry: &str) -> Vec<String> {
    for line in entry.lines() {
        if let Some(rest) = line.trim().strip_prefix("tags:") {
            return rest
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
        }
    }
    Vec::new()
}

/// Render a preference entry. Mirrors `render_fact` in `thoth-store` so
/// USER.md parses with the same `### heading / body / tags:` shape — the
/// MarkdownStore in thoth-store is re-used without a bespoke parser.
fn render_preference(text: &str, tags: &[String]) -> String {
    let mut lines = text.lines();
    let title = lines.next().unwrap_or("").trim();
    let body: Vec<&str> = lines.collect();
    let mut out = String::from("### ");
    out.push_str(title);
    out.push('\n');
    let body_joined = body.join("\n");
    if !body_joined.trim().is_empty() {
        out.push_str(body_joined.trim_end());
        out.push('\n');
    }
    if !tags.is_empty() {
        out.push_str("tags: ");
        out.push_str(&tags.join(", "));
        out.push('\n');
    }
    out.push('\n');
    out
}

/// Collect `MemoryEntryPreview` rows for the given markdown file. Missing
/// file yields an empty list — not an error — so callers can chain this
/// into `CapExceededError::entries` without an extra guard.
async fn collect_previews(path: &Path) -> Result<Vec<MemoryEntryPreview>> {
    let text = match tokio::fs::read_to_string(path).await {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let (_, entries) = split_entries(&text);
    Ok(entries
        .into_iter()
        .enumerate()
        .map(|(index, e)| MemoryEntryPreview {
            index,
            first_line: entry_first_line(&e),
            bytes: e.len(),
            tags: entry_tags(&e),
        })
        .collect())
}

/// Pick the single matching entry index given a `query`:
///
/// 1. Case-insensitive substring match on the first line and tags.
/// 2. If exactly one hit → return it.
/// 3. If multiple hits → fall back to Jaccard similarity over `text_sim`
///    tokens; only accept the top match when it's ≥ 0.6 AND strictly
///    greater than every other candidate.
/// 4. Otherwise → `Error::Other` listing all ambiguous candidates so the
///    caller (MCP tool handler) can surface them to the agent.
fn pick_entry(entries: &[String], query: &str) -> Result<usize> {
    let needle = query.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return Err(thoth_core::Error::Store(
            "empty match_substring".to_string(),
        ));
    }
    let mut substring_hits: Vec<usize> = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        let first_lc = entry_first_line(e).to_ascii_lowercase();
        let tag_match = entry_tags(e)
            .iter()
            .any(|t| t.to_ascii_lowercase().contains(&needle));
        if first_lc.contains(&needle) || tag_match {
            substring_hits.push(i);
        }
    }

    match substring_hits.len() {
        0 => Err(thoth_core::Error::Store(format!(
            "no entry matches query {query:?}"
        ))),
        1 => Ok(substring_hits[0]),
        _ => {
            let q_tokens = text_sim::tokens(query);
            let mut scored: Vec<(usize, f32)> = substring_hits
                .iter()
                .map(|&i| {
                    let t = text_sim::tokens(&entry_first_line(&entries[i]));
                    (i, text_sim::jaccard(&q_tokens, &t))
                })
                .collect();
            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            let (best_idx, best_score) = scored[0];
            let second = scored.get(1).map(|(_, s)| *s).unwrap_or(0.0);
            if best_score >= 0.6 && best_score > second {
                Ok(best_idx)
            } else {
                let titles: Vec<String> = substring_hits
                    .iter()
                    .map(|&i| entry_first_line(&entries[i]))
                    .collect();
                Err(thoth_core::Error::Store(format!(
                    "ambiguous match for {query:?}: {} candidates — {}",
                    substring_hits.len(),
                    titles.join(" | ")
                )))
            }
        }
    }
}

/// Build a [`CapExceededError`] snapshotting the current file. Used by
/// both cap-checking append paths.
async fn build_cap_error(
    kind: MemoryKind,
    path: &Path,
    current_bytes: usize,
    cap_bytes: usize,
    attempted_bytes: usize,
) -> CapExceededError {
    let entries = collect_previews(path).await.unwrap_or_default();
    CapExceededError {
        kind,
        current_bytes,
        cap_bytes,
        attempted_bytes,
        entries,
        hint: "Call thoth_memory_replace or thoth_memory_remove to free space, then retry."
            .to_string(),
    }
}

/// Policy-layer extension on [`MarkdownStore`]: cap-aware appends, single-
/// entry replace/remove with backup, and a uniform preview/size API across
/// the three markdown surfaces (`MEMORY.md` / `LESSONS.md` / `USER.md`).
///
/// This is defined here rather than in `thoth-store` so the raw storage
/// crate stays policy-free (its `append_fact` / `append_lesson` don't know
/// about caps). The MCP `thoth_memory_replace` / `thoth_memory_remove` /
/// `thoth_remember_preference` handlers call through this trait.
#[allow(async_fn_in_trait)]
pub trait MarkdownStoreMemoryExt {
    /// Append a user preference to `USER.md`, enforcing `cap_user_bytes`.
    ///
    /// Returns [`CapExceededError`] (with a preview snapshot) when the
    /// resulting file would exceed `cap`. Caller is expected to feed that
    /// list back to the agent so it can pick an entry to replace/remove.
    async fn append_preference(
        &self,
        text: &str,
        tags: &[String],
        cap: usize,
    ) -> std::result::Result<(), CapExceededError>;

    /// Wrapper around [`MarkdownStore::append_fact`] that refuses the write
    /// when `MEMORY.md` would grow past `cap` bytes.
    async fn append_fact_capped(
        &self,
        f: &thoth_core::Fact,
        cap: usize,
    ) -> std::result::Result<(), CapExceededError>;

    /// Wrapper around [`MarkdownStore::append_lesson`] that refuses the
    /// write when `LESSONS.md` would grow past `cap` bytes.
    async fn append_lesson_capped(
        &self,
        l: &thoth_core::Lesson,
        cap: usize,
    ) -> std::result::Result<(), CapExceededError>;

    /// Replace the single entry matching `query` with `new_text`. Returns
    /// the index of the entry that was replaced. Writes a `.bak-<unix>`
    /// snapshot before mutating.
    async fn replace(&self, kind: MemoryKind, query: &str, new_text: &str) -> Result<usize>;

    /// Remove the single entry matching `query`. Returns the index that
    /// was removed. Writes a `.bak-<unix>` snapshot before mutating.
    async fn remove(&self, kind: MemoryKind, query: &str) -> Result<usize>;

    /// Snapshot all entries in the given markdown surface.
    async fn preview(&self, kind: MemoryKind) -> Result<Vec<MemoryEntryPreview>>;

    /// Current size of the given markdown surface, in bytes. Missing file
    /// reports `0` — no error.
    async fn size_bytes(&self, kind: MemoryKind) -> Result<u64>;
}

impl MarkdownStoreMemoryExt for MarkdownStore {
    async fn append_preference(
        &self,
        text: &str,
        tags: &[String],
        cap: usize,
    ) -> std::result::Result<(), CapExceededError> {
        let path = md_path(&self.root, MemoryKind::Preference);
        let rendered = render_preference(text, tags);
        let current_bytes = tokio::fs::metadata(&path)
            .await
            .map(|m| m.len() as usize)
            .unwrap_or(0);
        let attempted_bytes = current_bytes + rendered.len();
        if attempted_bytes > cap {
            return Err(
                build_cap_error(MemoryKind::Preference, &path, current_bytes, cap, attempted_bytes)
                    .await,
            );
        }
        // Lazy init: write a header line if the file is missing so USER.md
        // matches the shape of MEMORY.md / LESSONS.md.
        if current_bytes == 0 {
            let header = "# USER.md\n";
            if let Err(e) = tokio::fs::write(&path, format!("{header}{rendered}")).await {
                tracing::warn!(error = %e, "append_preference: failed to create USER.md");
                return Err(CapExceededError {
                    kind: MemoryKind::Preference,
                    current_bytes,
                    cap_bytes: cap,
                    attempted_bytes,
                    entries: Vec::new(),
                    hint: format!("io error: {e}"),
                });
            }
            return Ok(());
        }
        let mut f = match tokio::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, "append_preference: open failed");
                return Err(CapExceededError {
                    kind: MemoryKind::Preference,
                    current_bytes,
                    cap_bytes: cap,
                    attempted_bytes,
                    entries: Vec::new(),
                    hint: format!("io error: {e}"),
                });
            }
        };
        use tokio::io::AsyncWriteExt;
        if let Err(e) = f.write_all(rendered.as_bytes()).await {
            tracing::warn!(error = %e, "append_preference: write failed");
            return Err(CapExceededError {
                kind: MemoryKind::Preference,
                current_bytes,
                cap_bytes: cap,
                attempted_bytes,
                entries: Vec::new(),
                hint: format!("io error: {e}"),
            });
        }
        Ok(())
    }

    async fn append_fact_capped(
        &self,
        f: &thoth_core::Fact,
        cap: usize,
    ) -> std::result::Result<(), CapExceededError> {
        let path = md_path(&self.root, MemoryKind::Fact);
        let current_bytes = tokio::fs::metadata(&path)
            .await
            .map(|m| m.len() as usize)
            .unwrap_or(0);
        // Approximate rendered size: first_line + body + tags + framing (~6 bytes of `### \n\n`).
        let mut approx = 4 + f.text.len() + 2;
        if !f.tags.is_empty() {
            approx += 6 + f.tags.iter().map(|t| t.len() + 2).sum::<usize>();
        }
        let attempted_bytes = current_bytes + approx;
        if attempted_bytes > cap {
            return Err(
                build_cap_error(MemoryKind::Fact, &path, current_bytes, cap, attempted_bytes).await,
            );
        }
        if let Err(e) = self.append_fact(f).await {
            tracing::warn!(error = %e, "append_fact_capped: underlying append failed");
            return Err(CapExceededError {
                kind: MemoryKind::Fact,
                current_bytes,
                cap_bytes: cap,
                attempted_bytes,
                entries: Vec::new(),
                hint: format!("io error: {e}"),
            });
        }
        Ok(())
    }

    async fn append_lesson_capped(
        &self,
        l: &thoth_core::Lesson,
        cap: usize,
    ) -> std::result::Result<(), CapExceededError> {
        let path = md_path(&self.root, MemoryKind::Lesson);
        let current_bytes = tokio::fs::metadata(&path)
            .await
            .map(|m| m.len() as usize)
            .unwrap_or(0);
        let mut approx = 4 + l.trigger.len() + l.advice.len() + 3;
        if l.success_count > 0 || l.failure_count > 0 {
            approx += 40;
        }
        let attempted_bytes = current_bytes + approx;
        if attempted_bytes > cap {
            return Err(
                build_cap_error(MemoryKind::Lesson, &path, current_bytes, cap, attempted_bytes)
                    .await,
            );
        }
        if let Err(e) = self.append_lesson(l).await {
            tracing::warn!(error = %e, "append_lesson_capped: underlying append failed");
            return Err(CapExceededError {
                kind: MemoryKind::Lesson,
                current_bytes,
                cap_bytes: cap,
                attempted_bytes,
                entries: Vec::new(),
                hint: format!("io error: {e}"),
            });
        }
        Ok(())
    }

    async fn replace(&self, kind: MemoryKind, query: &str, new_text: &str) -> Result<usize> {
        let path = md_path(&self.root, kind);
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e.into()),
        };
        let (preamble, mut entries) = split_entries(&text);
        let idx = pick_entry(&entries, query)?;
        // Preserve original tags on the entry when the caller didn't supply
        // a new `tags:` line — keeps `replace` focused on swapping the body.
        let tags = entry_tags(&entries[idx]);
        let rendered = render_preference(new_text, &tags);
        write_backup(&path).await?;
        entries[idx] = rendered;
        let header = match kind {
            MemoryKind::Fact => "# MEMORY.md\n",
            MemoryKind::Lesson => "# LESSONS.md\n",
            MemoryKind::Preference => "# USER.md\n",
        };
        let preamble = if preamble.trim().is_empty() {
            header.to_string()
        } else {
            preamble
        };
        let body = join_entries(&preamble, &entries);
        tokio::fs::write(&path, body).await?;
        // REQ-07: log `op=replace` so `reflection::count_remembers`
        // decrements debt. Errors are non-fatal — the replace succeeded
        // on disk, history is best-effort.
        let _ = self
            .append_history(&thoth_store::markdown::HistoryEntry {
                op: "replace",
                kind: history_kind(kind),
                title: new_text.lines().next().unwrap_or(new_text).to_string(),
                actor: None,
                reason: None,
            })
            .await;
        Ok(idx)
    }

    async fn remove(&self, kind: MemoryKind, query: &str) -> Result<usize> {
        let path = md_path(&self.root, kind);
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e.into()),
        };
        let (preamble, mut entries) = split_entries(&text);
        let idx = pick_entry(&entries, query)?;
        write_backup(&path).await?;
        let removed = entries.remove(idx);
        let header = match kind {
            MemoryKind::Fact => "# MEMORY.md\n",
            MemoryKind::Lesson => "# LESSONS.md\n",
            MemoryKind::Preference => "# USER.md\n",
        };
        let preamble = if preamble.trim().is_empty() {
            header.to_string()
        } else {
            preamble
        };
        let body = join_entries(&preamble, &entries);
        tokio::fs::write(&path, body).await?;
        // REQ-07: log `op=remove` so `reflection::count_remembers`
        // decrements debt. Title is the first line of the dropped
        // entry for audit.
        let title = removed
            .lines()
            .map(str::trim_start)
            .map(|l| l.trim_start_matches('#').trim())
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .to_string();
        let _ = self
            .append_history(&thoth_store::markdown::HistoryEntry {
                op: "remove",
                kind: history_kind(kind),
                title,
                actor: None,
                reason: None,
            })
            .await;
        Ok(idx)
    }

    async fn preview(&self, kind: MemoryKind) -> Result<Vec<MemoryEntryPreview>> {
        let path = md_path(&self.root, kind);
        collect_previews(&path).await
    }

    async fn size_bytes(&self, kind: MemoryKind) -> Result<u64> {
        let path = md_path(&self.root, kind);
        match tokio::fs::metadata(&path).await {
            Ok(m) => Ok(m.len()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod cap_enforcement_tests {
    use super::*;
    use tempfile::tempdir;
    use thoth_core::{Fact, MemoryKind as CoreKind, MemoryMeta};

    fn fact(text: &str) -> Fact {
        Fact {
            meta: MemoryMeta::new(CoreKind::Semantic),
            text: text.to_string(),
            tags: Vec::new(),
        }
    }

    #[tokio::test]
    async fn append_fact_errors_when_cap_exceeded() {
        let dir = tempdir().unwrap();
        let store = MarkdownStore::open(dir.path()).await.unwrap();
        // Seed with one large fact so the next append tips us over.
        let big = "x".repeat(200);
        store.append_fact(&fact(&big)).await.unwrap();
        // Cap well below current size.
        let err = store
            .append_fact_capped(&fact("another"), 50)
            .await
            .expect_err("expected CapExceededError");
        assert!(matches!(err.kind, MemoryKind::Fact));
        assert!(err.attempted_bytes > err.cap_bytes);
        assert!(!err.entries.is_empty(), "preview entries must be populated");
    }

    #[tokio::test]
    async fn replace_updates_single_match() {
        let dir = tempdir().unwrap();
        let store = MarkdownStore::open(dir.path()).await.unwrap();
        store.append_fact(&fact("alpha fact")).await.unwrap();
        store.append_fact(&fact("beta fact")).await.unwrap();
        let idx = store
            .replace(MemoryKind::Fact, "alpha", "alpha fact v2")
            .await
            .expect("single match replace");
        assert_eq!(idx, 0);
        let facts = store.read_facts().await.unwrap();
        assert_eq!(facts.len(), 2);
        assert!(facts[0].text.contains("v2"));
        assert!(facts[1].text.contains("beta"));
    }

    #[tokio::test]
    async fn remove_errors_on_ambiguous_match() {
        let dir = tempdir().unwrap();
        let store = MarkdownStore::open(dir.path()).await.unwrap();
        store.append_fact(&fact("shared token here")).await.unwrap();
        store.append_fact(&fact("shared token there")).await.unwrap();
        // "shared" substring-matches both; Jaccard tie (both share only
        // "shared" with the query) so pick_entry must error.
        let err = store
            .remove(MemoryKind::Fact, "shared")
            .await
            .expect_err("ambiguous should error");
        let msg = format!("{err}");
        assert!(msg.contains("ambiguous"), "unexpected error: {msg}");
        // Neither entry should have been removed.
        assert_eq!(store.read_facts().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn append_preference_writes_user_md() {
        let dir = tempdir().unwrap();
        let store = MarkdownStore::open(dir.path()).await.unwrap();
        store
            .append_preference("prefers dark mode", &["ui".to_string()], 1536)
            .await
            .expect("append_preference");
        let body =
            tokio::fs::read_to_string(dir.path().join("USER.md")).await.unwrap();
        assert!(body.contains("### prefers dark mode"));
        assert!(body.contains("tags: ui"));
        let size = store.size_bytes(MemoryKind::Preference).await.unwrap();
        assert!(size > 0);
    }

    #[tokio::test]
    async fn replace_creates_backup_file() {
        let dir = tempdir().unwrap();
        let store = MarkdownStore::open(dir.path()).await.unwrap();
        store.append_fact(&fact("original entry")).await.unwrap();
        store
            .replace(MemoryKind::Fact, "original", "original entry v2")
            .await
            .expect("replace");
        // Look for any MEMORY.bak-* sibling.
        let mut found = false;
        let mut rd = tokio::fs::read_dir(dir.path()).await.unwrap();
        while let Some(ent) = rd.next_entry().await.unwrap() {
            let name = ent.file_name().to_string_lossy().to_string();
            if name.starts_with("MEMORY.bak-") {
                found = true;
                break;
            }
        }
        assert!(found, "expected MEMORY.bak-<unix> backup to exist");
    }
}

#[cfg(test)]
mod cap_tests {
    use super::*;

    /// DESIGN-SPEC REQ-02: default caps for `MEMORY.md` / `USER.md` /
    /// `LESSONS.md` must land on the 3072 / 1536 / 5120 byte budget and
    /// `strict_content_policy` defaults off (REQ-12 is warn-only by default).
    #[test]
    fn memory_config_caps_default() {
        let cfg = MemoryConfig::default();
        assert_eq!(cfg.cap_memory_bytes, 3072);
        assert_eq!(cfg.cap_user_bytes, 1536);
        assert_eq!(cfg.cap_lessons_bytes, 5120);
        assert!(!cfg.strict_content_policy);
    }
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

/// REQ-04: verify that `append_fact` and `append_lesson` write an `op =
/// "append"` entry to `memory-history.jsonl` so the reflection-debt counter
/// in `thoth-memory` can count remembers correctly.
#[cfg(test)]
mod history_tests {
    use tempfile::tempdir;
    use thoth_core::{Fact, Lesson, MemoryKind, MemoryMeta};
    use thoth_store::markdown::MarkdownStore;

    fn fact(text: &str) -> Fact {
        Fact {
            meta: MemoryMeta::new(MemoryKind::Semantic),
            text: text.to_string(),
            tags: Vec::new(),
        }
    }

    fn lesson(trigger: &str, advice: &str) -> Lesson {
        Lesson {
            meta: MemoryMeta::new(MemoryKind::Reflective),
            trigger: trigger.to_string(),
            advice: advice.to_string(),
            success_count: 0,
            failure_count: 0,
        }
    }

    /// `append_fact` must emit an `op="append", kind="fact"` entry in
    /// `memory-history.jsonl`.
    #[tokio::test]
    async fn append_fact_writes_history_entry() {
        let dir = tempdir().unwrap();
        let store = MarkdownStore::open(dir.path()).await.unwrap();
        store.append_fact(&fact("the sky is blue")).await.unwrap();

        let history = store.read_history().await.unwrap();
        assert_eq!(history.len(), 1, "expected exactly one history entry");
        let entry = &history[0];
        assert_eq!(entry.op, "append", "op should be 'append'");
        assert_eq!(entry.kind, "fact", "kind should be 'fact'");
        assert_eq!(entry.title, "the sky is blue");
    }

    /// `append_lesson` must emit an `op="append", kind="lesson"` entry in
    /// `memory-history.jsonl`.
    #[tokio::test]
    async fn append_lesson_writes_history_entry() {
        let dir = tempdir().unwrap();
        let store = MarkdownStore::open(dir.path()).await.unwrap();
        store
            .append_lesson(&lesson("when editing migrations", "run sqlx prepare"))
            .await
            .unwrap();

        let history = store.read_history().await.unwrap();
        assert_eq!(history.len(), 1, "expected exactly one history entry");
        let entry = &history[0];
        assert_eq!(entry.op, "append", "op should be 'append'");
        assert_eq!(entry.kind, "lesson", "kind should be 'lesson'");
        assert_eq!(entry.title, "when editing migrations");
    }

    /// Both operations together produce two distinct history entries.
    #[tokio::test]
    async fn fact_and_lesson_produce_two_history_entries() {
        let dir = tempdir().unwrap();
        let store = MarkdownStore::open(dir.path()).await.unwrap();
        store.append_fact(&fact("gravity exists")).await.unwrap();
        store
            .append_lesson(&lesson("always write tests", "saves time"))
            .await
            .unwrap();

        let history = store.read_history().await.unwrap();
        assert_eq!(history.len(), 2, "expected two history entries");
        assert_eq!(history[0].kind, "fact");
        assert_eq!(history[1].kind, "lesson");
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

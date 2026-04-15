//! Memory kinds and their metadata.
//!
//! See `DESIGN.md` §5 for the full taxonomy.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// The five kinds of memory Thoth tracks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// In-process, session-scoped scratchpad.
    Working,
    /// Facts derived from the code itself (symbols, graph edges, ...).
    Semantic,
    /// Append-only log of queries, answers, and outcomes.
    Episodic,
    /// Reusable skill / playbook stored on disk.
    Procedural,
    /// Lesson learned from a past mistake.
    Reflective,
}

/// Universal metadata attached to every memory record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryMeta {
    /// Globally unique id.
    pub id: Uuid,
    /// Which kind of memory.
    pub kind: MemoryKind,
    /// Creation timestamp.
    pub created_at: OffsetDateTime,
    /// Last access timestamp (for decay).
    pub last_accessed_at: OffsetDateTime,
    /// How many times this has been retrieved.
    pub access_count: u64,
    /// Salience in `[0.0, 1.0]` — how important.
    pub salience: f32,
    /// Confidence in `[0.0, 1.0]` — for lessons / skills.
    pub confidence: f32,
    /// Optional TTL in seconds.
    pub ttl_seconds: Option<u64>,
    /// Upstream source events that produced this memory.
    pub sources: Vec<Uuid>,
    /// Memories superseded by this one (chain of evolution).
    pub supersedes: Option<Uuid>,
    /// Memories contradicted by this one.
    pub contradicts: Vec<Uuid>,
}

impl MemoryMeta {
    /// Construct a fresh metadata record for the given kind.
    pub fn new(kind: MemoryKind) -> Self {
        let now = OffsetDateTime::now_utc();
        Self {
            id: Uuid::new_v4(),
            kind,
            created_at: now,
            last_accessed_at: now,
            access_count: 0,
            salience: 0.5,
            confidence: 0.5,
            ttl_seconds: None,
            sources: Vec::new(),
            supersedes: None,
            contradicts: Vec::new(),
        }
    }
}

/// A fact recorded in `MEMORY.md` (or its derived index).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fact {
    /// Metadata.
    pub meta: MemoryMeta,
    /// Human-readable text of the fact.
    pub text: String,
    /// Optional tags for filtering.
    pub tags: Vec<String>,
}

/// A lesson learned from a mistake, stored in `LESSONS.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lesson {
    /// Metadata.
    pub meta: MemoryMeta,
    /// Trigger pattern — when this lesson should be injected as context.
    pub trigger: String,
    /// The advice / rule / warning itself.
    pub advice: String,
    /// How many retrievals this lesson has been helpful on.
    pub success_count: u64,
    /// How many retrievals this lesson has hurt on.
    pub failure_count: u64,
}

/// A procedural skill — stored as a directory under `.thoth/skills/`,
/// compatible with the `agentskills.io` standard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    /// Metadata.
    pub meta: MemoryMeta,
    /// Human-readable slug (e.g. `auth-jwt-pattern`).
    pub slug: String,
    /// One-line description (from SKILL.md frontmatter).
    pub description: String,
    /// Relative path to the skill directory inside `.thoth/skills/`.
    pub path: std::path::PathBuf,
}

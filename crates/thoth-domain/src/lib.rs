//! # thoth-domain
//!
//! Domain / business-rule memory for Thoth — the sixth memory kind.
//!
//! While [`thoth_core::MemoryKind::Semantic`] captures code facts (symbols,
//! calls, imports) extracted by tree-sitter, Domain memory captures
//! *declarative business rules, invariants, workflows, and ubiquitous
//! language* — the "why" behind the code.
//!
//! See [`docs/adr/0001-domain-memory.md`](../../../docs/adr/0001-domain-memory.md)
//! for the architectural decision record.
//!
//! ## Architecture
//!
//! ```text
//!   external source ──[DomainIngestor]──> RemoteRule
//!                                              │
//!                                              ▼
//!        <root>/domain/<context>/_remote/<source>/<id>.md
//!               (markdown + TOML frontmatter, git-trackable)
//!                                              │
//!                                              ▼
//!        tantivy + vector index (Mode::Full) ← same pipeline
//!
//!   `thoth domain sync` drives this flow. `recall()` only reads the
//!   on-disk snapshots, so Mode::Zero keeps its deterministic guarantee
//!   (DESIGN.md §6).
//! ```
//!
//! ## Design goals (per ADR 0001)
//!
//! - **Markdown is source of truth.** Frontmatter captures provenance;
//!   body is human-readable.
//! - **Suggest-only merge.** All ingested rules land as `status: proposed`;
//!   human PR promotes to `accepted`.
//! - **Feature-gated adapters.** `notion`, `asana`, `notebooklm`.
//! - **Redact on ingest.** PII filters run inside the adapter, not later.
//! - **Offline-safe.** `thoth domain sync` is the only network entry
//!   point; `recall()` never calls an external API.

#![deny(rust_2018_idioms)]
#![warn(missing_docs)]

pub mod error;
pub mod ingest;
pub mod redact;
pub mod snapshot;
pub mod sync;
pub mod types;

pub mod file;

#[cfg(feature = "notion")]
pub mod notion;

#[cfg(feature = "asana")]
pub mod asana;

#[cfg(feature = "notebooklm")]
pub mod notebooklm;

pub use error::DomainError;
pub use ingest::{DomainIngestor, IngestFilter};
pub use snapshot::{SnapshotStore, SnapshotWrite};
pub use sync::{SyncReport, SyncStats, sync_source};
pub use types::{RemoteRule, RuleKind, RuleStatus};

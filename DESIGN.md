# Thoth — Long-Term Memory for Coding Agents

> *"Thoth, scribe of the gods, weigher of hearts, keeper of knowledge."*

Thoth is an embedded Rust library (plus CLI and MCP server) that gives coding
agents a **persistent, queryable, code-aware memory** — with or without an LLM
in the loop.

This document captures the north-star architecture, the decisions behind it,
and the references we are standing on.

---

## 1. Vision

A coding agent should be able to:

- **Know** a codebase structurally — every function, call, import, reference.
- **Remember** what has happened — past queries, answers, bugs, fixes, reviews.
- **Learn** from mistakes — promote working patterns, demote failed ones.
- **Forget** what no longer matters — deleted files, stale facts, unused lessons.
- **Work offline** for the deterministic slice, and **invite an LLM in** only
  where semantic understanding actually helps.

Thoth is the substrate for that — a single embedded library, not a service.

## 2. Non-goals

- **Not a server.** No gRPC daemon, no IDE plugin in v1. Thoth is a library and
  two thin binaries (CLI, MCP server).
- **Not general-purpose memory.** Scope is *code*. For general chat memory,
  use Hermes/Letta/Mem0.
- **Not a search engine for the internet.** The memory is local to a project.
- **Not locked to one model.** Embedding and synthesis are trait-based; users
  bring their own provider (Voyage, OpenAI, Anthropic, ...).

## 3. Core Decisions

| Topic | Decision |
|---|---|
| Language | Rust 2021, 100% |
| Async runtime | `tokio` |
| Operating mode | `Mode::Zero` (offline, symbolic) + `Mode::Full` (embedding + LLM) |
| Storage (vector) | SQLite flat-cosine (default); [LanceDB](https://lancedb.com) behind the `lance` feature (planned) |
| Storage (KV / graph / metadata) | [redb](https://github.com/cberner/redb) — pure-Rust embedded KV |
| Storage (full-text / BM25) | [tantivy](https://github.com/quickwit-oss/tantivy) |
| Storage (episodic log) | SQLite + FTS5 (through `rusqlite`) |
| Memory source-of-truth | **Markdown files on disk** (`MEMORY.md`, `LESSONS.md`, `skills/*/SKILL.md`). Indexes are derived. |
| Parser | [tree-sitter](https://tree-sitter.github.io) with dynamic grammar loading (`libloading`) |
| File watcher | [notify](https://github.com/notify-rs/notify), internal only |
| Embedding | `Embedder` trait; adapters: Voyage, OpenAI, Cohere (feature-gated) |
| Synthesis | `Synthesizer` trait; adapters: Anthropic (feature-gated) |
| Skills format | Compatible with [agentskills.io](https://agentskills.io) standard |
| Delivery | (1) `thoth-core` library, (2) `thoth` CLI, (3) `thoth-mcp` server |

## 4. Architecture

```
            ┌──────────────────────────────────────────────────────┐
            │              PERCEPTION LAYER (Observers)            │
            │  File events │ Query │ Answer │ Outcome / Feedback  │
            └─────────────────────────┬────────────────────────────┘
                                      │ event stream
                                      ▼
           ┌────────────────────────────────────────────────────────┐
           │               INGEST (tree-sitter parse)                │
           │  AST diff → symbols, call edges, chunks, content hash   │
           └─────────────────────────┬──────────────────────────────┘
                                     │
                                     ▼
           ┌────────────────────────────────────────────────────────┐
           │                MEMORY MANAGER (policy core)             │
           │   ┌────────────┐  ┌────────────┐  ┌──────────────┐     │
           │   │ Writer     │  │ Forgetter  │  │ Nudge / LLM  │     │
           │   │ (TTL+hash) │  │ (decay)    │  │ curator (B)  │     │
           │   └────────────┘  └────────────┘  └──────────────┘     │
           └─────────────────────────┬──────────────────────────────┘
                                     │  atomic delta writes
                                     ▼
        ┌──────────────────────────────────────────────────────────┐
        │                   STORAGE (embedded)                      │
        │                                                           │
        │  ┌──────────┐ ┌─────────┐ ┌─────────┐ ┌────────────────┐ │
        │  │ markdown │ │  redb   │ │ tantivy │ │ LanceDB        │ │
        │  │ MEMORY/  │ │ graph + │ │ BM25    │ │ vectors        │ │
        │  │ LESSONS/ │ │ metadata│ │ index   │ │ (Mode::Full)   │ │
        │  │ skills/  │ │         │ │         │ │                │ │
        │  │          │ │ sqlite  │ │         │ │                │ │
        │  │          │ │ + FTS5  │ │         │ │                │ │
        │  │          │ │ episodes│ │         │ │                │ │
        │  └──────────┘ └─────────┘ └─────────┘ └────────────────┘ │
        └─────────────────────────┬─────────────────────────────────┘
                                  │
                                  ▼
              ┌─────────────────────────────────────────┐
              │        RETRIEVAL ORCHESTRATOR            │
              │  intent → fan-out stores → rerank → ctx  │
              └─────────────────────────┬───────────────┘
                                        │
                        ┌───────────────┼───────────────┐
                        ▼                               ▼
                 ┌───────────────┐             ┌───────────────┐
                 │ Mode::Zero    │             │ Mode::Full    │
                 │ return chunks │             │ LLM synth +   │
                 │ + citations   │             │ lesson inject │
                 └───────────────┘             └───────────────┘
```

## 5. Five Memory Types

| Kind | What it holds | Where it lives | Lifecycle |
|---|---|---|---|
| **Working** | Current session context | In-process only | Session |
| **Semantic** | Code facts (symbols, call graph) | redb + tantivy + LanceDB | Mirrors source files |
| **Episodic** | Query → answer → outcome events | SQLite + FTS5 | TTL + decay |
| **Procedural** | Reusable skills / playbooks | `skills/*/SKILL.md` on disk | Persistent, edited by LLM or human |
| **Reflective** | Lessons from mistakes | `LESSONS.md` on disk | Confidence-gated |

## 6. Mode::Zero vs Mode::Full

Both share the same storage and core retrieval. The difference is purely in
which optional component is plugged in.

### Mode::Zero (offline, no LLM, no embedding)

- **Retrieval** = symbol lookup (tree-sitter) + graph traversal (redb) +
  BM25 (tantivy) + markdown grep
- **Writer** = append-only episode log, hard-delete by file deletion, TTL decay
- **No API keys required.** Fully local, deterministic, free.
- Good for: "find this function", "who calls X", "find files mentioning auth"

### Mode::Full (embedding + LLM synthesis)

- Adds **LanceDB vector index** on chunks and episodes
- Adds **LLM nudge** at end of session: "any new facts/lessons/skills to persist?"
- Adds **LLM self-critique** on failed outcomes → draft `LESSONS.md` entries
- Adds **LLM synthesis** of final answers with citations
- Good for: "how is auth done here", "why did this break", "summarize module X"

A user may run Mode::Full with only an `Embedder` (semantic search, no synth) or
only a `Synthesizer` — each component is independently optional.

## 7. On-disk Layout

```
<project>/
└── .thoth/
    ├── config.toml           ← user config (mode, providers, TTL)
    ├── MEMORY.md             ← project-level facts (human/LLM curated)
    ├── LESSONS.md            ← reflective memory, confidence-scored
    ├── skills/
    │   └── <slug>/
    │       ├── SKILL.md      ← agentskills.io compatible
    │       └── …             ← scripts, resources
    ├── episodes.db           ← SQLite + FTS5
    ├── graph.redb            ← symbol + call graph
    ├── fts.tantivy/          ← BM25 index
    └── chunks.lance/         ← vector index (Mode::Full only)
```

All markdown files are **first-class source of truth** — indexes are rebuildable.
This means the user can `git add .thoth/*.md` and review memory in PRs.

## 8. Public API (sketch)

```rust
use thoth::{CodeMemory, Mode, Query, Embedder, Synthesizer};

let mem = CodeMemory::open(".thoth").await?;
mem.index(".").await?;

// Mode::Zero — no external calls
let r = mem.recall(Query::text("where is auth handled"), Mode::Zero).await?;

// Mode::Full — bring your own provider
let r = mem.recall(
    Query::text("why does login flake"),
    Mode::Full {
        embedder: Some(Box::new(VoyageEmbedder::new(api_key))),
        synthesizer: Some(Box::new(AnthropicSynth::new(api_key))),
    },
).await?;

// Write to episodic
mem.record_event(Event::QueryIssued { .. }).await?;

// Edit memory explicitly
mem.memory().append_fact("uses JWT in cookie").await?;
mem.skills().install_from_directory("./my-skill")?;
```

Traits:

```rust
#[async_trait]
pub trait Embedder: Send + Sync {
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
    fn dim(&self) -> usize;
    fn model_id(&self) -> &str;
}

#[async_trait]
pub trait Synthesizer: Send + Sync {
    async fn synthesize(&self, prompt: &Prompt) -> Result<Synthesis>;
    async fn critique(&self, outcome: &Outcome) -> Result<Option<Lesson>>;
}
```

## 9. Memory Lifecycle

### Write
- **Semantic** written on every file change: content-hash gated, atomic delta to
  redb + tantivy + (LanceDB if Full).
- **Episodic** appended on every `Event`. Cheap, never blocking.
- **Procedural / Reflective** written only via explicit call or LLM nudge.

### Forget
- **Hard delete**: semantic fact whose source file is deleted; symbol whose AST
  node disappears.
- **TTL**: episodic older than `ttl_days` (default 30) with no incoming lesson
  references → archive → delete.
- **Decay** (Mode::Full optional): `effective = salience · exp(-λ·days_idle) ·
  log(1 + access_count)`; below floor → forgotten.
- **Capacity cap**: if episode store exceeds `max_episodes`, drop lowest-scored.

### Learn
- **Nudge** (Mode::Full): at session end the memory manager runs two passes
  against the synthesizer:
  1. `Synthesizer::critique` on each recent `OutcomeObserved` event —
     produces zero or one `Lesson` per outcome.
  2. `Synthesizer::propose_session_memory` on the full recent window —
     returns a `NudgeProposal` bundle of `Fact`, `Lesson`, and `Skill`
     drafts. Facts are deduped by first-line title, lessons by trigger,
     and skills by slug, so the pass is idempotent across sessions.
- **Confidence evolves**: each lesson tracks `success_count` /
  `failure_count` as a hidden `<!-- success: N / failure: N -->` footer
  in `LESSONS.md`. The forget pass drops any lesson whose ratio is below
  `lesson_floor` once it has reached `lesson_min_attempts` retrievals.

## 10. Influences

The design borrows deliberately and acknowledges sources.

- **Hermes Agent (Nous Research)** — markdown-first memory, skill autonomy,
  LLM nudge pattern over algorithmic policies. *This shaped the final design
  more than anything else.*
- **MemGPT / Letta** (Packer et al., UC Berkeley) — tiered memory concept.
- **Generative Agents** (Park et al., Stanford 2023) — reflection loop,
  salience scoring idea.
- **Reflexion** (Shinn et al.) — self-critique on failure.
- **Voyager** (Wang et al., NVIDIA) — skill library as executable procedural
  memory.
- **ACT-R / Soar** — the 5-kind taxonomy (working / semantic / episodic /
  procedural / reflective).
- **Ebbinghaus** — exponential forgetting curve.
- **Sourcegraph / Cody / Aider / Cursor** — code-intelligence + repo-map patterns.
- **agentskills.io** — procedural memory format compatibility.

## 11. Roadmap

| Milestone | Content |
|---|---|
| **M0 — Scaffold** | Workspace, crates, traits, `cargo check` green |
| **M1 — Parser + watcher** | tree-sitter wrapper, dynamic grammar load, file events |
| **M2 — Storage** | redb + tantivy + SQLite schema + markdown writer |
| **M3 — Mode::Zero retrieval** | symbol lookup + graph traversal + BM25 hybrid |
| **M4 — CLI** | `thoth init`, `index`, `query`, `watch`, `memory`, `skills` |
| **M5 — MCP server** | `thoth-mcp` stdio, tool catalog, resource exposure |
| **M6 — Mode::Full** | `Embedder` + `Synthesizer` adapters, LanceDB, nudge flow |
| **M7 — Hardening** | Eval harness, benchmarks, docs, examples |

## 12. Open Questions (explicitly deferred)

- **Multi-repo / monorepo graph joins.** Defer until single-repo is solid.
- **Encryption at rest.** Defer; relies on filesystem permissions for v1.
- **Remote Thoth (shared team memory).** Out of scope; local-first only.
- **Fine-tuning on collected episodes.** Export hooks yes, training no.
- **LanceDB as the default vector store.** §3 and the storage diagram show
  LanceDB because it's the long-term target — Arrow-native, columnar, and
  purpose-built for this use case. v1 ships with a SQLite flat-cosine index
  (`thoth_store::VectorStore`) because it (a) keeps the dependency surface
  small, (b) piggybacks on the same SQLite file the episodic log already
  needs, and (c) is trivially deterministic in tests. LanceDB support is
  planned behind the `lance` Cargo feature in `thoth-store`; the
  `chunks.lance/` path in §7 is reserved for it. Cut-over happens once the
  flat-cosine index becomes a bottleneck on real repos (tens of thousands
  of chunks with per-token embedding latency).
- **Dynamic tree-sitter grammar loading (`libloading`).** §3 lists this as the
  long-term direction, but the v1 implementation statically links a fixed set
  of grammars (Rust, Python, TypeScript, JavaScript, Go) behind Cargo features
  in `thoth-parse`. Dynamic loading is deferred because (a) the `tree-sitter`
  crate ABI for externally-compiled grammars is unstable across versions, and
  (b) shipping `.so`/`.dylib` grammars complicates install on Windows and in
  distroless containers. Revisit once the grammar set needs to expand past
  what's convenient to vendor.

## 13. Credits

Thoth stands on the shoulders of the projects listed in §10 and the broader
open-source Rust, LLM agent, and code-intelligence communities.

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
- **Not general-purpose memory.** Scope is *code and the business rules
  that code enforces*. For general chat memory, use Hermes/Letta/Mem0.
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
| Parser | [tree-sitter](https://tree-sitter.github.io) with statically-linked grammars (Rust, Python, TS, JS, Go); dynamic grammar loading is deferred (§12) |
| Code graph | redb KV-backed; edges: `Calls`, `Imports`, `Extends`, `References`, `DeclaredIn`. Indexer resolves call / extends targets through file-local alias maps (import aliases + same-file symbols) before writing. |
| Discipline | `thoth-gate` (PreToolUse hook) enforces memory consultation via a 3-factor decision: intent (read-only Bash bypasses), recency (recall within `window_short_secs` passes), relevance (token-overlap containment ≥ `relevance_threshold` against the last ~20 recalls). Actor-aware policy map, optional JSONL telemetry. |
| File watcher | [notify](https://github.com/notify-rs/notify), internal only |
| Embedding | `Embedder` trait; adapters: Voyage, OpenAI, Cohere (feature-gated) |
| Synthesis | `Synthesizer` trait; adapters: Anthropic (feature-gated) |
| Domain ingest | `DomainIngestor` trait; adapters: `file` (always on), `notion`, `asana`, `notebooklm` (feature-gated). Snapshots to markdown — no live remote calls on the recall path. See [ADR 0001](adr/0001-domain-memory.md). |
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
                                     │
        ┌────────── DOMAIN INGEST (on `thoth domain sync`) ─────────┐
        │  DomainIngestor trait: file / notion / asana / notebooklm │
        │         │          redact (JWT, keys, PAN, AWS)           │
        │         ▼          hash (blake3) → SnapshotStore          │
        │   RemoteRule → domain/<context>/_remote/<src>/<id>.md     │
        └─────────────────────────┬─────────────────────────────────┘
                                     ▼
        ┌──────────────────────────────────────────────────────────┐
        │                   STORAGE (embedded)                      │
        │                                                           │
        │  ┌──────────┐ ┌─────────┐ ┌─────────┐ ┌────────────────┐ │
        │  │ markdown │ │  redb   │ │ tantivy │ │ LanceDB        │ │
        │  │ MEMORY/  │ │ graph + │ │ BM25    │ │ vectors        │ │
        │  │ LESSONS/ │ │ metadata│ │ index   │ │ (Mode::Full)   │ │
        │  │ domain/  │ │         │ │         │ │                │ │
        │  │ skills/  │ │ sqlite  │ │         │ │                │ │
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

   ┌── DISCIPLINE (PreToolUse hook, out-of-band) ─────────────────────┐
   │  thoth-gate  ←── reads episodes.db (query_issued rows)           │
   │                                                                   │
   │  decide(tool_name, tool_input, actor) →                           │
   │    Intent    (read-only Bash whitelist → pass silent)             │
   │    Recency   (recall within short window → pass)                  │
   │    Relevance (containment(edit tokens, recall pool) ≥ threshold)  │
   │                                                                   │
   │  → verdict: off | nudge | strict  (policy-driven)                 │
   │  → optional telemetry to .thoth/gate.jsonl                        │
   └───────────────────────────────────────────────────────────────────┘
```

## 5. Six Memory Types

| Kind | What it holds | Where it lives | Lifecycle |
|---|---|---|---|
| **Working** | Current session context | In-process only | Session |
| **Semantic** | Code facts (symbols, call graph) | redb + tantivy + LanceDB | Mirrors source files |
| **Episodic** | Query → answer → outcome events | SQLite + FTS5 | TTL + decay |
| **Procedural** | Reusable skills / playbooks | `skills/*/SKILL.md` on disk | Persistent, edited by LLM or human |
| **Reflective** | Lessons from mistakes | `LESSONS.md` on disk | Confidence-gated |
| **Domain** | Business rules, invariants, workflows, glossary | `domain/<context>/DOMAIN.md` + `_remote/<source>/*.md` | Proposed → Accepted via PR; rebuildable from remote |

Semantic answers *"what calls X?"*. Domain answers *"why does X enforce a $500 refund limit?"*. The two together let the agent reason about both the code *and* the business rules it embodies. See [ADR 0001](adr/0001-domain-memory.md) for the full decision record.

### 5a. Domain memory layout and lifecycle

Source of truth is markdown on disk, same discipline as `MEMORY.md` / `LESSONS.md`:

```
.thoth/domain/
├── index.md                         ← optional glossary + bounded-context map
└── <context>/                       ← one folder per bounded context (DDD)
    ├── DOMAIN.md                    ← human-authored rules (## Accepted)
    └── _remote/<source>/<id>.md     ← ingestor-written snapshots (## Proposed)
```

Flat layout (`domain/DOMAIN.md` only) is fine for small repos; `layout = "hierarchical"` in config enables per-context sharding once the glossary starts crossing bounded contexts.

**Four write paths, one review gate.** All ingested content lands in a `## Proposed` section. Only a human PR (or an owner listed in `CODEOWNERS`) promotes an entry to `## Accepted`. Retrieval ranks `Accepted` first; `Proposed` only surfaces when explicitly asked.

| Path | Trust | Source |
|---|---|---|
| Human PR | high | editor |
| Remote sync (`thoth domain sync`) | medium | Notion / Asana / NotebookLM / file |
| LLM nudge (Mode::Full) | low | session diff + conversation |
| Test extraction | high (narrow) | test assertions |

**Snapshots are first-class markdown** with TOML frontmatter (delimited by `+++`):

```markdown
+++
id = "PROJ-1234"
source = "asana"
source_uri = "https://app.asana.com/…/task/1234"
source_hash = "blake3:ab…"
context = "billing"
kind = "invariant"
last_synced = 2026-04-16T08:00:00Z
status = "proposed"
+++
# Refund over $500 requires manager approval
…
```

`source_hash` enables drift detection (re-sync with no upstream change is a no-op); `status` gates retrieval. `kind` is one of `invariant | workflow | glossary | policy`.

**Mode::Zero preserved.** Retrieval only reads on-disk snapshots. All remote API calls live in `thoth domain sync` — never in `recall()`. §6 determinism guarantee intact.

**Redaction runs before every write.** The sync pipeline scans each `RemoteRule` for JWTs, provider tokens (`sk-`, `xoxb-`, `ghp_`, …), 16-digit card numbers and AWS access keys; any hit drops the rule and records a `redacted` stat. No snapshot is ever written for a redacted rule.

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

### 6a. Graph-centric retrieval

`recall()` is the hybrid "search for a topic" path. Once the agent
knows *which* symbol it wants to understand, three graph-native tools
answer more structured questions without a BM25 / vector round trip:

| Question                                                   | Tool                     | Graph access |
|------------------------------------------------------------|--------------------------|-------------|
| What breaks if I change `fqn`? (callers / references / subtypes) | `thoth_impact`           | BFS up, depth-grouped  |
| What does `fqn` depend on? (callees / parent types)        | `thoth_impact` `down`    | BFS down, depth-grouped |
| Full 360° view of a single symbol                          | `thoth_symbol_context`   | depth-1 neighbours per edge kind |
| Which symbols does this PR actually touch + their blast radius? | `thoth_detect_changes`   | diff ↔ declaration span → impact |

These sit on top of five edge kinds (`Calls`, `Imports`, `Extends`,
`References`, `DeclaredIn`). Two indexer invariants make them useful:

- **Call resolution.** During indexing, every `(caller_fqn,
  callee_leaf_name)` pair is rewritten through a file-local map built
  from (a) import aliases extracted by the language-specific parser and
  (b) same-file symbol FQNs. `foo()` called inside `m::bar` writes the
  edge as `m::bar → m::foo` instead of the unresolved bare leaf,
  so BFS over `Calls` actually connects inside a module.
- **Inheritance edges.** `impl Trait for Type` (Rust), `class X extends Y
  implements Z` (TS/JS), `class X(Y, Z)` (Python) and `trait Sub:
  Super1 + Super2` (Rust) all emit `Extends` edges with parent names
  resolved through the same file-local map.

Parser schema changes bump a `PARSER_SCHEMA_VERSION` constant baked
into the per-file hash sentinel, so a version bump invalidates every
cached hash in one go and the next indexer run re-parses all files.

## 7. On-disk Layout

```
<project>/
└── .thoth/
    ├── config.toml           ← user config (mode, gate v2 knobs, TTL, domain sources)
    ├── MEMORY.md             ← project-level facts (human/LLM curated)
    ├── LESSONS.md            ← reflective memory, confidence-scored
    ├── MEMORY.pending.md     ← staged entries (`memory_mode = "review"`)
    ├── LESSONS.pending.md    ← staged entries (`memory_mode = "review"`)
    ├── LESSONS.quarantined.md ← lessons auto-demoted by the forget pass
    ├── memory-history.jsonl  ← versioned audit trail (stage/promote/reject)
    ├── gate.jsonl            ← decision telemetry (when enabled)
    ├── domain/
    │   ├── index.md          ← optional glossary + bounded-context map
    │   └── <context>/
    │       ├── DOMAIN.md     ← human-authored rules (## Accepted)
    │       └── _remote/
    │           └── <source>/<id>.md   ← ingestor snapshots (## Proposed)
    ├── skills/
    │   └── <slug>/
    │       ├── SKILL.md      ← agentskills.io compatible
    │       └── …             ← scripts, resources
    ├── episodes.db           ← SQLite + FTS5 (event log: query_issued, …)
    ├── graph.redb            ← symbol graph (Calls, Imports, Extends, …)
    ├── fts.tantivy/          ← BM25 index
    ├── vectors.db            ← flat-cosine vector index (Mode::Full, default)
    └── chunks.lance/         ← LanceDB vector index (Mode::Full + `lance`)
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

#[async_trait]
pub trait DomainIngestor: Send + Sync {
    /// Stable identifier: "notion", "asana", "file", "notebooklm", ...
    fn source_id(&self) -> &str;
    /// Pull rules from the upstream source, respecting `filter`.
    async fn list(&self, filter: &IngestFilter) -> Result<Vec<RemoteRule>>;
    /// Project-level routing: which bounded context does this rule belong to?
    fn map_to_context(&self, rule: &RemoteRule) -> Option<String> { /* default */ }
}
```

Domain sync is a single top-level call from the CLI / MCP:

```rust
use std::sync::Arc;
use thoth_domain::{DomainIngestor, IngestFilter, SnapshotStore, sync_source};

let ing: Arc<dyn DomainIngestor> = Arc::new(FileIngestor::new("./specs"));
let snap = SnapshotStore::new(".thoth");
let report = sync_source(ing, &snap, &IngestFilter::default()).await?;
println!("{report}"); // created / updated / unchanged / redacted / unmapped
```

## 9. Memory Lifecycle

### Write
- **Semantic** written on every file change: content-hash gated, atomic delta to
  redb + tantivy + (LanceDB if Full).
- **Episodic** appended on every `Event`. Cheap, never blocking.
- **Procedural / Reflective** written only via explicit call or LLM nudge.
- **Domain** written only via `thoth domain sync` (any adapter) or a human PR
  editing `DOMAIN.md`. Remote snapshots are content-hash gated (re-sync with
  no upstream change is a no-op); redaction runs before disk touch; writes
  are atomic (tmp + rename).

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

### Discipline (consult-before-mutate)

Memory is only useful if the agent actually consults it before acting.
`thoth-gate` is a tiny native PreToolUse hook that makes consultation
observable by reading `episodes.db` directly — no MCP round trip.

Decision is a 3-factor pipeline:

1. **Intent.** Classify the tool call. Read-only Bash (`cargo test`,
   `git status`, `grep`, `rg`, `ls`, `cat`, …) and non-mutation tools
   (`Read`, `Glob`) bypass silently. `Edit`, `Write`, `NotebookEdit`,
   and mutating `Bash` continue.
2. **Recency.** If a `query_issued` event landed within
   `gate_window_short_secs` (default 60s), the call passes. The short
   default intentionally kills "recall once, edit forever" patterns
   without forcing a relevance check on every rapid edit.
3. **Relevance.** Past the short window, tokenise the edit context
   (file basename, old/new strings, diff body) and the recall pool
   (`query_issued` events within `gate_window_long_secs`, default 30
   min, capped at ~20 rows). Score = `containment(edit ∩ recall) /
   min(|edit|, |recall|)`. Score ≥ `gate_relevance_threshold` passes;
   miss is handled by the policy `mode`:

   - `off` — pass silent (fully disabled).
   - `nudge` — pass with a stderr warning listing the edit tokens,
     ranked recent recalls with overlap scores, and a suggested
     `thoth_recall` query built from the missed tokens.
   - `strict` — emit `{"decision":"block"}`.

Tokenizer: lowercase, split non-identifier characters, drop stopwords
and language keywords, single-char tokens, and pure digits; preserve
snake_case as a single token; split CamelCase into segments **and** the
joined form (`FooBar` → `{foo, bar, foobar}`). Capped at 200 tokens per
side.

**Actor-aware policies** let the same gate binary behave differently
per caller. `THOTH_ACTOR` selects a policy via first-matching glob in
`[[discipline.policies]]`; default policy applies otherwise. Useful for
treating an orchestrated wave worker (`hoangsa/*`, nudge-only, long
window) differently from an interactive editing session
(`claude-code-direct`, strict + low threshold) without spawning
separate daemons.

**Telemetry** is opt-in (`gate_telemetry_enabled = true`). When on, each
decision appends one JSON line to `.thoth/gate.jsonl` with actor, tool,
decision, reason code (`readonly_whitelist` / `recency` / `relevance` /
`relevance_miss` / `cold_start` / `time_lapsed`), scores and missed
tokens. Users calibrate `relevance_threshold` against real logs rather
than guesses.

**Fail-open invariant.** Any internal error (missing DB, unreadable
config, SQLite corruption) reverts to pass+stderr so a broken gate
never bricks the editor.

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
  procedural / reflective); Thoth adds a 6th, **Domain**, for business rules.
- **Domain-Driven Design** (Evans, Vernon) — bounded contexts as the
  sharding unit for `domain/<context>/` layout.
- **Ebbinghaus** — exponential forgetting curve.
- **Sourcegraph / Cody / Aider / Cursor** — code-intelligence + repo-map patterns.
- **GitNexus** — graph-centric MCP tools (impact / context / detect_changes) that informed §6a; Thoth intentionally omits the gimmicks (clustering, process tracing, Cypher surface) and keeps multi-repo out of scope.
- **agentskills.io** — procedural memory format compatibility.

## 11. Roadmap

| Milestone | Status | Content |
|---|---|---|
| **M0 — Scaffold** | ✅ | Workspace, crates, traits, `cargo check` green |
| **M1 — Parser + watcher** | ✅ | tree-sitter wrapper (static grammars), file events; dynamic loading deferred (§12) |
| **M2 — Storage** | ✅ | redb + tantivy + SQLite schema + markdown writer |
| **M3 — Mode::Zero retrieval** | ✅ | symbol lookup + graph traversal + BM25 hybrid, RRF fusion |
| **M4 — CLI** | ✅ | `thoth init`, `index`, `query`, `watch`, `memory`, `skills`, `impact`, `context`, `changes`, `eval` |
| **M5 — MCP server** | ✅ | `thoth-mcp` stdio, tool catalog, resource exposure |
| **M6 — Mode::Full** | ✅ | `Embedder` + `Synthesizer` adapters, SQLite flat-cosine default, LanceDB feature-gated, nudge flow |
| **M7 — Domain memory** | ✅ | `thoth-domain` crate, `DomainIngestor` trait, file/Notion/Asana adapters, `thoth domain sync` CLI, suggest-only merge |
| **M8 — Discipline v2** | ✅ | `thoth-gate` 3-factor decision (intent / recency / relevance), actor-aware policies, optional JSONL telemetry |
| **M9 — Graph-centric tools** | ✅ | `thoth_impact`, `thoth_symbol_context`, `thoth_detect_changes` (MCP + CLI); import-alias resolution + `Extends` edges in the indexer |
| **M10 — Eval hardening** | ✅ | `thoth eval` reports P@k, MRR, latency p50/p95; `--mode zero|full|both` ablation |
| **M11 — Hardening** | 🚧 | Benchmarks, more language extractors, documented recipes |

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
- **Universal MCP ingestor.** `DomainIngestor` currently ships three native
  adapters (file, Notion, Asana) and one stub (NotebookLM). A universal
  MCP-based ingestor would collapse most remote adapters into one path that
  any MCP-enabled tool (Jira, Linear, NotebookLM, custom internal systems)
  can plug into. Deferred because (a) the MCP tool-catalog conventions for
  "list knowledge rows" are not yet standardized and (b) two real adapters
  are more informative than a premature abstraction. Revisit after the
  third or fourth native adapter.
- **Summary-first retrieval.** Per-context `DOMAIN.md` files can grow past
  the retrieval budget on large monorepos. A two-stage "summary → full text"
  retrieval (summary hit expands to the referenced rules) is in ADR 0001
  but deferred until BM25 + accepted-first ranking proves insufficient in
  the eval harness.

## 13. Credits

Thoth stands on the shoulders of the projects listed in §10 and the broader
open-source Rust, LLM agent, and code-intelligence communities.

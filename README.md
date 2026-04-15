<p align="center">
  <img src="thoth.png" alt="Thoth" width="200" />
</p>

<h1 align="center">Thoth</h1>
<p align="center"><em>"Thoth, scribe of the gods, keeper of knowledge."</em></p>

<p align="center">Long-term memory for claude coding agents. Embedded, Rust-native, code-aware.</p>

<p align="center"><strong>🇬🇧 English</strong> · <a href="./README.vi.md">🇻🇳 Tiếng Việt</a></p>


<p align="center">
  <a href="https://github.com/unknown-studio-dev/thoth/actions/workflows/ci.yml"><img src="https://github.com/unknown-studio-dev/thoth/actions/workflows/ci.yml/badge.svg" alt="ci" /></a>
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/Rust-1.91%2B-orange?logo=rust" alt="Rust" /></a>
  <a href="https://github.com/unknown-studio-dev/thoth/releases"><img src="https://img.shields.io/badge/version-0.0.1--alpha-blue" alt="version" /></a>
  <a href="./LICENSE-MIT"><img src="https://img.shields.io/badge/license-MIT%2FApache--2.0-green" alt="license" /></a>
</p>

---

## What it is

Thoth is a Rust library (plus a CLI, an MCP server, and a Claude Code /
Cowork plugin) that gives a coding agent a *persistent*, *disciplined*
memory of a codebase. The project ships in two layers:

1. **The engine** — `thoth`, `thoth-mcp`, `thoth-gate` binaries.
2. **The plugin** — `thoth-discipline` (hooks + skills + MCP wiring) that
   makes a Claude Code / Cowork session actually *use* the memory on every
   turn.

Four memory kinds, one store:

- **Semantic** — every symbol, call, import, reference, parsed by tree-sitter.
- **Episodic** — every query, answer, and outcome appended to an FTS5 log.
- **Procedural** — reusable skills stored as `agentskills.io`-compatible folders.
- **Reflective** — lessons learned from mistakes, confidence-scored in
  `LESSONS.md`, auto-quarantined when they start doing more harm than good.

Two operating modes:

- **`Mode::Zero`** — fully offline, deterministic. No LLM, no embedding API.
  Symbol lookup, graph traversal, BM25 via tantivy, RRF fusion.
- **`Mode::Full`** — plug in an `Embedder` (Voyage / OpenAI / Cohere) and/or
  a `Synthesizer` (Anthropic Claude) for semantic vector search and
  LLM-curated memory (the "nudge" flow). The vector backend is a
  SQLite-resident flat cosine index — zero extra infrastructure.

## Install

Thoth ships as three binaries: `thoth` (CLI), `thoth-mcp` (MCP server),
`thoth-gate` (strict-mode hook). Pick any channel — they all deliver the
same set.

### Homebrew (macOS + Linux)

```bash
brew tap unknown-studio-dev/thoth
brew install thoth
```

### npm

```bash
npm install -g thoth-memory
# or one-off:
npx thoth-memory setup
```

npm publishes `thoth-memory` plus four platform-specific subpackages
(`thoth-memory-{darwin-arm64,darwin-x64,linux-arm64,linux-x64}`);
`optionalDependencies` make npm pick the right one automatically.

### From source

```bash
cargo install --git https://github.com/unknown-studio-dev/thoth thoth-cli thoth-mcp
```

### Verify

```bash
thoth --version
thoth-mcp --version
thoth-gate < /dev/null    # should print {"decision":"approve",...}
```

## Getting started in 30 seconds

```bash
cd your-project
thoth setup              # interactive wizard → .thoth/config.toml
thoth index .            # build the code index
thoth install            # wire up Claude Code hooks + skill + MCP
```

`thoth setup` walks you through the knobs that matter — enforcement mode,
memory mode, gate window — and writes a commented `config.toml` so you
can tweak the rest later. Pass `--show` to print the current config, or
`--accept-defaults` for non-interactive bootstrap.

The Cowork / Claude Code plugin (`thoth-discipline`) is what turns those
binaries into an *actively enforced* recall loop:

- Download [`thoth-discipline-x.y.z.plugin`](https://github.com/unknown-studio-dev/thoth/releases) from the
  GitHub Release that matches your binary version.
- Install via Cowork's plugin picker, or `claude plugin install` for
  Claude Code.
- Details: [`plugins/thoth-discipline/README.md`](./plugins/thoth-discipline/README.md).

⚠️ **The plugin alone is not enough.** Hooks call `thoth-gate`, and the
MCP entry launches `thoth-mcp` — install the binaries first.

## Configuration

`thoth setup` writes everything, but if you want to edit by hand,
`<root>/config.toml` looks like:

```toml
[memory]
episodic_ttl_days = 30
enable_nudge      = true

[discipline]
mode                      = "soft"       # "soft" | "strict"
global_fallback           = true
reflect_cadence           = "end"        # "end" | "every"
nudge_before_write        = true
grounding_check           = false
gate_window_secs          = 180

# v2 knobs
memory_mode               = "auto"       # "auto" | "review"
gate_require_nudge        = false
quarantine_failure_ratio  = 0.66
quarantine_min_attempts   = 5
```

| Scenario            | `mode`   | `gate_require_nudge` | `memory_mode` |
|---------------------|----------|----------------------|---------------|
| Solo, low-friction  | `soft`   | `false`              | `auto`        |
| Solo, careful       | `strict` | `false`              | `auto`        |
| Team, experimental  | `strict` | `true`               | `review`      |
| Team, post-v1       | `strict` | `true`               | `auto`        |

## Architecture

```
  ┌── Cowork / Claude Code ────────────────────────────────────────────┐
  │                                                                    │
  │   thoth-discipline plugin                                          │
  │   ├── hooks/hooks.json      SessionStart / PreToolUse / Stop       │
  │   ├── skills/               memory-discipline + thoth-reflect      │
  │   └── .mcp.json             launches `thoth-mcp`                   │
  │          │                                                         │
  │          ▼                                                         │
  │   thoth-gate  ─ read-only SQLite check ─► episodes.db              │
  │   (PreToolUse command hook, blocks on missing recall / nudge)      │
  │                                                                    │
  └────────────────────────┬───────────────────────────────────────────┘
                           │ JSON-RPC / stdio
                           ▼
  ┌── thoth-mcp ───────────────────────────────────────────────────────┐
  │   tools    thoth_recall, thoth_remember_*, thoth_memory_*,         │
  │            thoth_request_review, thoth_skill_propose, …            │
  │   prompts  thoth.nudge  (logs NudgeInvoked event)                  │
  │            thoth.reflect                                           │
  │   resources thoth://memory/MEMORY.md, thoth://memory/LESSONS.md    │
  └────────────────────────┬───────────────────────────────────────────┘
                           │
                           ▼
  ┌── `.thoth/` store ─────────────────────────────────────────────────┐
  │   episodes.db           event log (query_issued, nudge_invoked…)   │
  │   graph.redb            symbol / import / call graph               │
  │   fts.tantivy/          BM25 index                                 │
  │   vectors.db            flat cosine vector index (Mode::Full)      │
  │   MEMORY.md             declarative facts                          │
  │   LESSONS.md            reflective lessons (active)                │
  │   LESSONS.quarantined.md  lessons auto-demoted after repeated miss │
  │   MEMORY.pending.md, LESSONS.pending.md  staged in `review` mode   │
  │   memory-history.jsonl  versioned audit trail                      │
  │   skills/               procedural skills                          │
  └────────────────────────────────────────────────────────────────────┘
```

Three enforcement layers, ordered by how bypassable they are:

1. **Prompts + skills** — SessionStart hook dumps lessons in context;
   `memory-discipline` skill guides the agent through recall/nudge/act/reflect.
2. **Hook prompts** — PreToolUse/PostToolUse hooks push short reminders
   that are hard to miss but still text.
3. **`thoth-gate`** (strict mode) — a native binary runs on every
   `Write` / `Edit` / `Bash` PreToolUse. It queries `episodes.db`
   directly for a recent `query_issued` (and optionally `nudge_invoked`)
   event and **blocks** the tool call if they're missing. The model can't
   self-talk past a `{"decision":"block"}` verdict.

`thoth-gate` fails open on any error (missing DB, unreadable config) so a
broken gate never bricks your editor — at the cost of silently reverting to
soft mode. Check `stderr` if strict feels weak.

## CLI cheatsheet

```bash
# project lifecycle
thoth setup                               # interactive config wizard
thoth setup --show                        # print current config
thoth init                                # create .thoth/
thoth index .                             # parse + index
thoth watch .                             # stay resident, reindex on change
thoth query "how does the nudge flow work"

# memory
thoth memory show
thoth memory fact "Auth tokens expire after 15m" --tags auth,jwt
thoth memory lesson --when "touching db/migrations" "run make db-check"
thoth memory pending                      # review queue (review mode)
thoth memory promote lesson 0
thoth memory reject  fact   2 --reason "duplicate"
thoth memory log --limit 50               # audit trail from memory-history.jsonl
thoth memory forget                       # TTL + quarantine pass
thoth --synth anthropic memory nudge      # LLM-curated lesson proposals

# Claude Code wiring
thoth install                             # skills + hooks + MCP, project scope
thoth install --scope user                # global
thoth uninstall                           # remove in that scope

# eval
thoth eval --gold eval/gold.toml -k 8
```

Run `thoth --help` for the full surface.

## MCP server

`thoth-mcp` speaks JSON-RPC 2.0 over stdio (MCP version `2024-11-05`).
Tools published:

| Tool                     | What it does                                                 |
|--------------------------|--------------------------------------------------------------|
| `thoth_recall`           | Mode::Zero hybrid recall                                     |
| `thoth_index`            | Walk + parse + index a path                                  |
| `thoth_remember_fact`    | Append / stage a fact                                        |
| `thoth_remember_lesson`  | Append / stage a lesson (refuses to silently overwrite)      |
| `thoth_memory_show`      | Read both markdown files                                     |
| `thoth_memory_pending`   | List staged entries                                          |
| `thoth_memory_promote`   | Accept a staged entry                                        |
| `thoth_memory_reject`    | Drop a staged entry with a reason                            |
| `thoth_memory_history`   | Tail `memory-history.jsonl`                                  |
| `thoth_memory_forget`    | TTL + capacity eviction + auto-quarantine pass               |
| `thoth_lesson_outcome`   | Bump success/failure counters on a lesson                    |
| `thoth_request_review`   | Flag something for human audit                               |
| `thoth_skill_propose`    | Draft a new skill from ≥5 consolidated lessons               |
| `thoth_skills_list`      | Enumerate installed skills                                   |

Plus two resources (`thoth://memory/MEMORY.md`, `thoth://memory/LESSONS.md`)
and two prompts (`thoth.nudge`, `thoth.reflect`) — the nudge prompt logs a
`NudgeInvoked` event that strict mode's two-event gate can check.

## Release flow

- Tag `vX.Y.Z` on `main`.
- `.github/workflows/release.yml` builds `aarch64-apple-darwin`,
  `x86_64-apple-darwin`, `aarch64-unknown-linux-gnu`,
  `x86_64-unknown-linux-gnu`, uploads tarballs + sha256s + the plugin
  bundle to the GitHub Release.
- `packaging/homebrew/bump.sh vX.Y.Z` stamps fresh SHAs into the formula
  — copy output to your tap's `Formula/thoth.rb` and push.
- `packaging/npm/publish.sh vX.Y.Z` re-packs the tarballs as npm packages
  and publishes (+ optional `DRY_RUN=1`).

## Embedding as a library

Mode::Zero:

```rust
use thoth_core::Query;
use thoth_parse::LanguageRegistry;
use thoth_retrieve::{Indexer, Retriever};
use thoth_store::StoreRoot;

let store = StoreRoot::open(".thoth").await?;
Indexer::new(store.clone(), LanguageRegistry::new())
    .index_path(".")
    .await?;

let r = Retriever::new(store);
let out = r.recall(&Query::text("token refresh logic")).await?;
for chunk in out.chunks {
    println!("{:?}  {}:{}  {}", chunk.source, chunk.path.display(),
             chunk.span.0, chunk.preview);
}
```

Mode::Full — add an embedder and/or a synthesizer:

```rust
use std::sync::Arc;
use thoth_core::{Embedder, Query, Synthesizer};
use thoth_embed::voyage::VoyageEmbedder;
use thoth_synth::anthropic::AnthropicSynthesizer;
use thoth_parse::LanguageRegistry;
use thoth_retrieve::{Indexer, Retriever};
use thoth_store::{StoreRoot, VectorStore};

let store    = StoreRoot::open(".thoth").await?;
let vectors  = VectorStore::open(StoreRoot::vectors_sqlite_path(".thoth".as_ref())).await?;
let embed: Arc<dyn Embedder>   = Arc::new(VoyageEmbedder::from_env()?);
let synth: Arc<dyn Synthesizer> = Arc::new(AnthropicSynthesizer::from_env()?);

Indexer::new(store.clone(), LanguageRegistry::new())
    .with_embedding(embed.clone(), vectors.clone())
    .index_path(".")
    .await?;

let r = Retriever::with_full(store, Some(vectors), Some(embed), Some(synth));
let out = r.recall_full(&Query::text("how does the nudge flow work")).await?;
println!("{}", out.synthesized.unwrap_or_default());
```

## Contributing

Bug reports, feature requests, memory-drift reports, translations and
PRs are all welcome. See [`CONTRIBUTING.md`](./CONTRIBUTING.md) for the
workflow, code style, and issue templates.

## Status

**Alpha.** Design frozen in [`DESIGN.md`](./DESIGN.md). Milestones M1–M6
(parse + store + graph + retrieve + CLI + MCP + Mode::Full + discipline
plugin) are in.

## License

Licensed under either of Apache License 2.0 ([LICENSE-APACHE](./LICENSE-APACHE))
or the MIT license ([LICENSE-MIT](./LICENSE-MIT)), at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.

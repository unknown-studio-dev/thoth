# Thoth

Long-term memory for coding agents. Embedded, Rust-native, code-aware.

> *"Thoth, scribe of the gods, keeper of knowledge."*

[![ci](https://github.com/unknown-studio-dev/thoth/actions/workflows/ci.yml/badge.svg)](https://github.com/unknown-studio-dev/thoth/actions/workflows/ci.yml)

---

## What is it

Thoth is a Rust library (plus a CLI and an MCP server) that gives a coding
agent a persistent memory of a codebase. Four memory kinds, one store:

- **Semantic** — every symbol, call, import, reference, parsed by tree-sitter.
- **Episodic** — every query, answer, and outcome appended to an FTS5 log.
- **Procedural** — reusable skills stored as `agentskills.io`-compatible folders.
- **Reflective** — lessons learned from mistakes, confidence-scored in
  `LESSONS.md`.

Two operating modes:

- **`Mode::Zero`** — fully offline, deterministic. No LLM, no embedding API.
  Symbol lookup, graph traversal, BM25 via tantivy, RRF fusion.
- **`Mode::Full`** — plug in an `Embedder` (Voyage / OpenAI / Cohere) and/or a
  `Synthesizer` (Anthropic Claude) for semantic vector search and LLM-curated
  memory (the "nudge" flow). The vector backend is a SQLite-resident flat
  cosine index — zero extra infrastructure.

## Status

**Alpha.** Design frozen in [`DESIGN.md`](./DESIGN.md). Milestones M1–M6
(parse + store + graph + retrieve + CLI + MCP + Mode::Full) are in.

## Layout

```
thoth/
├── DESIGN.md                  ← north-star design
├── Makefile                   ← dogfood targets
├── Cargo.toml                 ← workspace
├── eval/gold.toml             ← precision@k gold set
└── crates/
    ├── thoth-core/            ← public API, traits, types
    ├── thoth-parse/           ← tree-sitter, walker, watcher
    ├── thoth-store/           ← redb + tantivy + rusqlite + markdown
    ├── thoth-graph/           ← call / import / ref graph
    ├── thoth-memory/          ← MEMORY.md / LESSONS.md / forget pass
    ├── thoth-retrieve/        ← indexer + Mode::Zero recall (RRF)
    ├── thoth-embed/           ← Embedder adapters (feature-gated)
    ├── thoth-synth/           ← Synthesizer adapters (feature-gated)
    ├── thoth-cli/             ← `thoth` binary
    └── thoth-mcp/             ← `thoth-mcp` stdio JSON-RPC server
```

## Quick start (Mode::Zero — offline)

```bash
# 1. Build
cargo build --release

# 2. Point Thoth at a source tree
./target/release/thoth --root .thoth init
./target/release/thoth --root .thoth index path/to/your/repo

# 3. Ask it things
./target/release/thoth --root .thoth query "where is auth handled"
./target/release/thoth --root .thoth query -k 4 "hybrid recall RRF"
```

## Quick start (Mode::Full — embeddings + synthesis)

`Mode::Full` is feature-gated at build time. Enable the providers you want —
they're all optional, and you can mix and match an embedder with a
synthesizer:

```bash
# Build with every provider wired in
cargo build --release --features "thoth-cli/full"

# Or pick-and-choose: voyage | openai | cohere  (embedders)
#                    anthropic                 (synthesizer)
cargo build --release --features "thoth-cli/voyage thoth-cli/anthropic"
```

API keys come from the provider's standard env var:

| Provider      | Env var              |
|---------------|----------------------|
| Voyage        | `VOYAGE_API_KEY`     |
| OpenAI        | `OPENAI_API_KEY`     |
| Cohere        | `COHERE_API_KEY`     |
| Anthropic     | `ANTHROPIC_API_KEY`  |

Then pass `--embedder` and/or `--synth` to any subcommand:

```bash
# Index *and* embed every chunk
export VOYAGE_API_KEY=...
./target/release/thoth --embedder voyage index .

# Hybrid recall (symbol + BM25 + graph + markdown + vector)
./target/release/thoth --embedder voyage query "token refresh logic"

# Full RAG-style answer (retrieval + Claude synthesis with chunk citations)
export ANTHROPIC_API_KEY=...
./target/release/thoth --embedder voyage --synth anthropic \
    query "how does the nudge flow decide to persist a lesson"

# Ask the synthesizer to critique recent outcomes and suggest lessons
./target/release/thoth --synth anthropic memory nudge
```

The vector index lives at `.thoth/index/vectors.sqlite` — a single SQLite
file, safe to delete and rebuild.

Everything is a normal directory. `.thoth/index/` holds the derived
`redb`/`tantivy`/`sqlite` files and is safe to delete (it will be rebuilt on
next `index`). `MEMORY.md`, `LESSONS.md`, and `skills/` are the human-editable
source of truth — commit them alongside your code.

## Dogfood Makefile

The repo carries a `Makefile` that wires up the full happy path against
*this* source tree:

```bash
make help          # list targets
make demo          # build → init → index → run 6 sample queries
make eval          # run the precision@k gold set in eval/gold.toml
make watch         # re-index on change
make mcp           # run the MCP stdio server against .thoth-self/
```

See `make help` for the full surface. Everything Thoth writes goes under
`.thoth-self/` (git-ignored).

## MCP server

`thoth-mcp` speaks JSON-RPC 2.0 over stdio, implementing the
[Model Context Protocol](https://modelcontextprotocol.io/) (version
`2024-11-05`). Seven tools are exposed:

| Tool                       | What it does                                                   |
|----------------------------|----------------------------------------------------------------|
| `thoth_recall`             | Mode::Zero hybrid recall over the index                        |
| `thoth_index`              | Walk + parse + index a path                                    |
| `thoth_remember_fact`      | Append a fact to `MEMORY.md`                                   |
| `thoth_remember_lesson`    | Append a lesson to `LESSONS.md`                                |
| `thoth_skills_list`        | Enumerate installed skills                                     |
| `thoth_memory_show`        | Read back both markdown files                                  |
| `thoth_memory_forget`      | Run TTL + capacity eviction over the episodic log              |

Plus two resources: `thoth://memory/MEMORY.md` and `thoth://memory/LESSONS.md`.

To wire it into a client (Claude Desktop, Continue, etc.), point the client's
MCP config at the binary and set `THOTH_ROOT`:

```json
{
  "mcpServers": {
    "thoth": {
      "command": "/path/to/thoth-mcp",
      "env": { "THOTH_ROOT": "/path/to/your/project/.thoth" }
    }
  }
}
```

Then run `make mcp` (or invoke `thoth-mcp` directly) once the client
connects.

## Claude Code integration

Two one-shot commands wire Thoth into Claude Code — the skill makes it
discoverable to the agent, the hooks make memory passive (dumped at
session start, recall injected on every prompt, incremental re-index
after every edit, forget + nudge at stop).

```bash
# Install the agentskills.io-compatible skill.
thoth skills install --scope project   # writes .thoth/skills/thoth/SKILL.md
thoth skills install --scope user      # writes ~/.claude/skills/thoth/SKILL.md

# Install the Claude Code hook block into settings.json.
thoth hooks install --scope project    # writes .claude/settings.json
thoth hooks install --scope user       # writes ~/.claude/settings.json

# Remove Thoth's hook block (leaves user-owned hooks untouched).
thoth hooks uninstall --scope project
```

The `install` merge is idempotent — running it twice leaves
`settings.json` unchanged. Four hook events are wired:

| Event              | Matcher                  | What Thoth does                                   |
|--------------------|--------------------------|---------------------------------------------------|
| `SessionStart`     | `*`                      | Dump `MEMORY.md` + `LESSONS.md` into the context. |
| `UserPromptSubmit` | `*`                      | Inject top-5 hybrid recall for the prompt.        |
| `PostToolUse`      | `Edit\|Write\|MultiEdit` | Incrementally re-index the edited file.           |
| `Stop`             | `*`                      | `forget_pass` (+ `nudge` in Mode::Full).          |

Each hook resolves to `thoth hooks exec <event>` — a dispatcher that reads
the hook payload from stdin as JSON, runs the action, and prints any new
context back on stdout. Errors are swallowed so a failing hook never
blocks the agent.

To also expose the MCP tools directly, add the `thoth-mcp` server block
to the same `settings.json`:

```json
{
  "mcpServers": {
    "thoth": {
      "command": "thoth-mcp",
      "args": ["--root", ".thoth"]
    }
  }
}
```

(Template bundled at `crates/thoth-cli/assets/hooks/mcp.json`.)

## Memory lifecycle

```bash
thoth memory show                                  # cat MEMORY.md + LESSONS.md
thoth memory fact "Auth tokens expire after 15m" --tags auth,jwt
thoth memory lesson --when "touching db/migrations" \
                    "run `make db-check` before committing"
thoth memory forget                                # TTL + capacity eviction
thoth --synth anthropic memory nudge               # LLM-curated lesson proposals
```

The forget pass is deterministic in Mode::Zero: delete every episode older
than `episodic_ttl_days` (default 30d) and cap the log at
`max_episodes` (default 50 000 newest). The nudge pass is Mode::Full only
— it walks the most recent `OutcomeObserved` episodes, asks the
`Synthesizer` to critique each one, and appends any proposed lessons to
`LESSONS.md` (idempotent on trigger).

## Evaluating recall

The `thoth eval` subcommand runs a gold set of queries against the current
index and prints precision@k. See `eval/gold.toml` for the schema:

```bash
thoth --root .thoth-self eval --gold eval/gold.toml -k 8
```

The binary exits non-zero on any miss, so it slots neatly into CI. The
repo's own CI workflow (`.github/workflows/ci.yml`) runs `fmt-check`,
`clippy -D warnings`, `cargo test`, and the eval gate on every PR.

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
let vectors  = VectorStore::open(".thoth/index/vectors.sqlite").await?;
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

## License

Dual-licensed under MIT or Apache-2.0.

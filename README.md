<p align="center">
  <img src="docs/thoth.png" alt="Thoth" width="200" />
</p>

<h1 align="center">Thoth</h1>
<p align="center"><em>"Thoth, scribe of the gods, keeper of knowledge."</em></p>

<p align="center">Long-term memory for Claude Code agents. Rust-native, code-aware, zero API required.</p>

<p align="center"><strong>English</strong> · <a href="./README.vi.md">Tieng Viet</a></p>

<p align="center">
  <a href="https://github.com/unknown-studio-dev/thoth/actions/workflows/ci.yml"><img src="https://github.com/unknown-studio-dev/thoth/actions/workflows/ci.yml/badge.svg" alt="ci" /></a>
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/Rust-1.91%2B-orange?logo=rust" alt="Rust" /></a>
  <a href="https://github.com/unknown-studio-dev/thoth/releases"><img src="https://img.shields.io/badge/version-0.0.1--alpha-blue" alt="version" /></a>
  <a href="./LICENSE-MIT"><img src="https://img.shields.io/badge/license-MIT%2FApache--2.0-green" alt="license" /></a>
</p>

> [!WARNING]
> **Work in progress — not production-ready.** APIs, on-disk formats, and CLI flags may change without notice. Do **not** rely on it for production workloads yet.

---

## What it is

Thoth gives a Claude Code agent persistent, disciplined memory of a
codebase. It parses source with tree-sitter, builds a symbol graph
(calls, imports, extends, references), indexes everything with BM25
(tantivy) and RRF fusion, and enforces a "recall before write" gate so
the agent consults memory before mutating code.

Three binaries:

1. **`thoth`** — CLI: setup, index, query, eval, memory ops.
2. **`thoth-mcp`** — MCP stdio server (41 tools, 3 prompts, 2 resources).
3. **`thoth-gate`** — PreToolUse hook: blocks writes without prior recall.

`thoth setup` wires everything — hooks, MCP server, skills, config — in
one command.

Nothing leaves your machine unless you opt in.

---

## Install

```bash
npx @unknownstudio/thoth        # downloads binary + runs setup wizard
```

Other channels:

```bash
brew install unknown-studio-dev/thoth/thoth
# or
cargo install --git https://github.com/unknown-studio-dev/thoth thoth-cli thoth-mcp
thoth setup
```

## Quickstart

```bash
thoth index .                                    # build code index
thoth query "how does the gate work"             # hybrid recall
thoth impact "module::symbol" --direction up     # blast radius
thoth memory fact "tokens expire after 15m" --tags auth
```

Inside Claude Code, the gate and skills work automatically after setup.

---

## Benchmarks

All numbers from this repo with the commands below. Machine: MacBook Pro
14" (Nov 2023), Apple M3 Pro, 18 GB RAM, release build. Corpus: Thoth's
own source tree (**109 Rust files, ~47 k LoC**). Mode::Zero only (no
embedding, no LLM calls).

**Recall accuracy (seeded gold set, 10 queries over facts + lessons + code):**

```bash
cargo test -p thoth-retrieve --test recall_accuracy -- --nocapture
```

| Metric | Value |
|--------|-------|
| R@5 | **100 %** (10/10) |
| R@3 | **100 %** (10/10) |
| Target | R@5 >= 80 %, R@3 >= 60 % |

The test seeds 8 facts, 5 lessons, and 3 Rust source files into a temp
store, then runs 10 natural-language queries and asserts each finds the
expected substring in the top-k results.

**Eval on Thoth's own source tree (8 gold queries):**

```bash
thoth eval --gold eval/gold.toml -k 8
```

| Metric | Value |
|--------|-------|
| P@8 | **100 %** (8/8) |
| MRR | **0.771** |
| Latency p50 | 75 ms |
| Latency p95 | 90 ms |

**`graph_bfs` microbenchmark (Criterion):**

```bash
cargo bench -p thoth-store --bench graph_bfs
```

| Direction | Start | Median |
|-----------|-------|-------:|
| Out | root | **1.73 ms** |
| In | deepest leaf | **13.5 us** |
| Both | deepest leaf | **501 us** |

Synthetic 4-ary tree (~341 nodes, 5 levels), BFS depth 8. The `In`
direction benefits from the reverse-edge index (`edges_by_dst`).

---

## Memory

Four working memory kinds, one store:

| Kind | Storage | What |
|------|---------|------|
| **Semantic** | `graph.redb` + `fts.tantivy/` | Symbols, calls, imports, references (tree-sitter) |
| **Episodic** | `episodes.db` (SQLite FTS5) | Every query, outcome, event — timeline for reflect |
| **Reflective** | `LESSONS.md` | Lessons from mistakes, confidence-scored, auto-quarantined |
| **Domain** | `domain/<ctx>/` | Business rules synced from Notion / Asana / local files |

Facts live in `MEMORY.md`, preferences in `USER.md`.

## Gate (search-before-write)

`thoth-gate` runs on every Write/Edit/Bash PreToolUse and decides from
three factors: **intent** (read-only Bash bypasses), **recency** (recent
recall passes), **relevance** (edit tokens scored against recall
history). Mode: `off` / `nudge` (default) / `strict`.

Reflection debt (`mutations - remembers`) adds a second enforcement
loop: nudge at 10, hard block at 20. Tunable in `config.toml`.

## Knowledge graph

Temporal entity-relationship triples with validity windows — add, query,
invalidate, timeline — backed by SQLite. Available via MCP tools
(`thoth_kg_*`) and CLI.

## MCP server

41 tools covering recall, memory CRUD, graph analysis, knowledge graph,
overrides, workflows, and conversation archive. Plus 3 prompts
(`thoth.nudge`, `thoth.reflect`, `thoth.grounding_check`) and 2
resources (`thoth://memory/MEMORY.md`, `thoth://memory/LESSONS.md`).

Run `thoth --help` or see the tool table in [CLAUDE.md](./CLAUDE.md).

## Background review & compact

`thoth review` — LLM-driven session review, spawned automatically by
PostToolUse hook. Builds context from event logs (~1k tokens), not full
conversation. Uses `claude-haiku-4-5` by default.

`thoth compact` — merges near-duplicate facts/lessons. Preview with
`--dry-run`. Backs up originals before overwriting.

## Domain memory

Business rules synced from external sources via `thoth domain sync`.
Feature-gated adapters:

| Adapter | Feature | Auth |
|---------|---------|------|
| `file` | always on | -- |
| `notion` | `notion` | `NOTION_TOKEN` |
| `asana` | `asana` | `ASANA_TOKEN` |
| `notebooklm` | `notebooklm` | -- (stub; export to file) |

## CLI cheatsheet

```bash
thoth setup                            # interactive wizard
thoth index .                          # parse + index
thoth watch .                          # stay resident, reindex on change
thoth query "nudge flow"               # hybrid recall

thoth impact  "mod::sym" -d 3          # blast radius
thoth context "mod::sym"               # 360 symbol view
thoth changes                          # git diff HEAD -> touched symbols

thoth memory show                      # read MEMORY.md + LESSONS.md
thoth memory fact "..." --tags x,y     # append fact
thoth memory lesson --when "..." "..." # append lesson
thoth memory forget                    # TTL + quarantine pass

thoth review                           # LLM session review
thoth compact --dry-run                # preview memory consolidation

thoth domain sync --source file --from ./specs/
thoth eval --gold eval/gold.toml -k 8  # precision@k eval
thoth install                          # wire Claude Code (hooks+MCP+skills)
thoth uninstall                        # remove wiring
```

`thoth --help` for the full surface.

## Embedding as a library

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
let hits = r.recall(&Query::text("token refresh logic")).await?;
```

---

## Requirements

- Rust >= 1.91 (for building from source)
- Git >= 2.30

No API key required for Mode::Zero. For Mode::Full, set
`ANTHROPIC_API_KEY` / `VOYAGE_API_KEY` as needed.

## Contributing

See [`CONTRIBUTING.md`](./CONTRIBUTING.md).

## Status

**Alpha.** Core is working: parse, store, graph, retrieve, CLI, MCP,
gate, reflection debt, background review, domain sync, knowledge graph,
conversation archive. On-disk format may still change.

## License

Dual-licensed: [Apache 2.0](./LICENSE-APACHE) or [MIT](./LICENSE-MIT).

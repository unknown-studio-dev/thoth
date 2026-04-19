# Thoth Architecture

## Overview

Thoth is a long-term memory system for coding agents. It indexes source trees
with tree-sitter, stores facts and lessons in human-readable markdown files,
and serves hybrid retrieval (symbol graph + BM25 + optional vector search) over
a Model Context Protocol (MCP) stdio interface.

---

## Crate Map

| Crate | Purpose |
|-------|---------|
| `thoth` | Umbrella library facade — wires all sub-crates behind `CodeMemory` |
| `thoth-core` | Stable public API: types, traits, errors shared by every crate |
| `thoth-parse` | Tree-sitter wrapper — AST chunking, file walking, file watching |
| `thoth-store` | Storage backends: redb (graph/KV), tantivy (BM25), SQLite (episodes), ChromaDB, markdown |
| `thoth-graph` | Symbol/call/import graph traversal and BFS impact analysis |
| `thoth-memory` | Memory lifecycle — TTL forgetting, lesson confidence, nudge flow |
| `thoth-retrieve` | Retrieval orchestrator — fan-out, RRF fusion, Mode dispatch |
| `thoth-domain` | Domain/business-rule memory (declarative rules separate from code facts) |
| `thoth-mcp` | MCP stdio server exposing all tools to MCP-aware clients |
| `thoth-cli` | `thoth` binary — setup, index, query, watch, memory management |

---

## Dependency Graph

```
thoth-core          (types, traits, errors — no deps on other thoth crates)
    │
    ├── thoth-parse     (tree-sitter; reads source → chunks + symbol tables)
    ├── thoth-store     (redb + tantivy + sqlite + chromadb + markdown)
    │       │
    │       └── thoth-graph     (graph traversal over KvStore)
    │
    ├── thoth-memory    (lifecycle policy over MarkdownStore + EpisodeLog)
    ├── thoth-domain    (domain rule ingestor over MarkdownStore)
    │
    └── thoth-retrieve  (orchestrator — depends on store + graph + memory)
            │
            ├── thoth           (facade — CodeMemory)
            │       │
            │       ├── thoth-mcp   (MCP server — depends on thoth)
            │       └── thoth-cli   (CLI binary — depends on thoth + thoth-mcp)
```

---

## Retrieval Pipeline

### Mode::Zero (lexical only — no external calls)

```
Query
  ├─ symbol lookup      (KvStore exact/prefix match on FQN)
  ├─ graph traversal    (BFS callers/callees via thoth-graph)
  ├─ BM25 full-text     (tantivy FtsIndex)
  ├─ markdown search    (MEMORY.md + LESSONS.md prefix scan)
  └─ ChromaDB vector    (optional; skipped if not configured)
        │
        └── RRF fuse → Retrieval { chunks, citations }
```

### Mode::Full (adds LLM synthesis)

Same fan-out as Mode::Zero, but ChromaDB is always used and a caller-supplied
`Synthesizer` is applied after RRF fusion:

```
... (same fan-out) ...
  └── RRF fuse
        └── Synthesizer::synthesize  (LLM re-rank + answer generation)
              └── Retrieval { chunks, citations, synthesis }
```

The `rerank_markdown_boost` knob (in `config.toml`) up-weights markdown hits
relative to code chunks in both modes.

---

## Storage Layout

```
.thoth/
  config.toml           # optional: TTL, nudge flags, chroma settings
  MEMORY.md             # append-only facts (source of truth)
  LESSONS.md            # lessons with trigger + advice + outcome counts
  skills/<slug>/
    SKILL.md            # installed skill definitions
  graph.redb            # redb: symbol nodes + call/import edges
  fts.tantivy/          # tantivy BM25 index directory
  episodes.db           # SQLite + FTS5 episodic event log
  chroma/               # ChromaDB persistence (managed by chroma server)
  archive_sessions.db   # session archive tracker
  domain/<context>/     # domain rule markdown files (thoth-domain)
```

Legacy stores (`index/kv.redb`, `index/fts/`, `index/episodes.sqlite`) are
migrated in-place the first time a stale store is opened.

---

## Entry Points

### CLI (`thoth`)

Binary in `thoth-cli`. Subcommands include `setup`, `index`, `query`,
`memory`, `watch`, `impact`, `context`, `changes`, `review`, `compact`, and
`uninstall`. The CLI communicates with a running `thoth-mcp` process via a
Unix socket when available, falling back to in-process operation.

### MCP Server (`thoth-mcp`)

Binary in `thoth-mcp`. Listens on stdio (JSON-RPC 2.0) for MCP-aware clients
(Claude Code, Cursor, Zed). Concurrently runs a Unix socket listener for the
CLI thin-client. Root resolution order: `$THOTH_ROOT` > `./.thoth/` >
`~/.thoth/projects/<blake3-slug>/`.

### Library (`thoth`)

`CodeMemory::open(".thoth")` is the single entry point for embedding Thoth in
another Rust program. All backends are opened eagerly; ChromaDB is opened
lazily on first `Mode::Full` recall.

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

> [!WARNING]
> **Work in progress — not production-ready.** Thoth is under active development (`0.0.1-alpha`). APIs, on-disk formats, and CLI flags may change without notice. Expect bugs, breaking changes, and incomplete features. Use at your own risk; do **not** rely on it for production workloads yet.

---

## What it is

Thoth is a Rust library (plus a CLI, an MCP server, and a one-shot
bootstrap that wires Claude Code) that gives a coding agent a
*persistent*, *disciplined* memory of a codebase. Three binaries do
all the work:

1. **`thoth`** — CLI: setup wizard, indexer, query, eval, memory ops.
2. **`thoth-mcp`** — MCP stdio server Claude Code talks to over `mcpServers`.
3. **`thoth-gate`** — `PreToolUse` hook that enforces "search before write".

`thoth setup` is the single command that installs hooks, registers the
MCP server, copies skills, and seeds `.thoth/`. There is no separate
plugin to install.

Five memory kinds, one store:

- **Semantic** — every symbol, call, import, reference, parsed by tree-sitter.
- **Episodic** — every query, answer, and outcome appended to an FTS5 log.
- **Procedural** — reusable skills stored as `agentskills.io`-compatible folders.
- **Reflective** — lessons learned from mistakes, confidence-scored in
  `LESSONS.md`, auto-quarantined when they start doing more harm than good.
- **Domain** — business rules, invariants, workflows and glossary ingested
  from Notion / Asana / NotebookLM / local files and snapshotted to
  `domain/<context>/` as reviewable markdown. Answers *"why does this
  enforce a $500 refund limit?"* — the code-aware → codebase-aware gap.
  See [ADR 0001](./docs/adr/0001-domain-memory.md).

Two operating modes:

- **`Mode::Zero`** — fully offline, deterministic. No LLM, no embedding API.
  Symbol lookup, graph traversal, BM25 via tantivy, RRF fusion.
- **`Mode::Full`** — plug in an `Embedder` (Voyage / OpenAI / Cohere) and/or
  a `Synthesizer` (Anthropic Claude) for semantic vector search and
  LLM-curated memory (the "nudge" flow). The default vector backend is a
  SQLite-resident flat cosine index (`vectors.db`) — zero extra infrastructure.
  Build with `--features lance` to swap in a LanceDB index (`chunks.lance/`)
  for larger corpora; the API is identical.

## Install

**One command.** Everything else happens at runtime.

```bash
# Zero-config: drops you into the setup wizard, then prints the next step.
npx @unknownstudio/thoth
```

That single invocation:

1. Downloads the prebuilt binary (`thoth`, `thoth-mcp`, `thoth-gate`)
   for your platform via npm.
2. Runs `thoth setup` — the interactive wizard that writes
   `.claude/settings.json` (MCP + hooks), copies skills into
   `.claude/skills/`, and seeds `.thoth/` with `config.toml`,
   `MEMORY.md`, `LESSONS.md`.
3. Tells you to review `.thoth/config.toml`, then run `thoth index .`.

Re-running `npx @unknownstudio/thoth` on a project that's already
bootstrapped detects the existing install and offers to reinstall
hooks, reconfigure, or self-heal missing pieces.

Other channels (same binaries, no Node required):

```bash
brew install unknown-studio-dev/thoth/thoth
# or
cargo install --git https://github.com/unknown-studio-dev/thoth thoth-cli thoth-mcp
# then:
thoth setup
```

## First use

Setup leaves you with a wired Claude Code project but an empty index.
One command to populate it:

```bash
cd your-project
thoth index .            # build the code index (incremental after this)
```

Open Claude Code in the project. `SessionStart` loads `LESSONS.md` /
`MEMORY.md`, `PreToolUse(Write|Edit|Bash|NotebookEdit)` fires
`thoth-gate`, and `Stop` triggers `thoth.reflect` to persist lessons.

Optional knobs (`mode`, `gate_relevance_threshold`,
`quarantine_failure_ratio`, …) live in `.thoth/config.toml`. Re-run
`thoth setup` any time you want to revisit the wizard; defaults are
sane, so skip if you don't care.

### Verify

```bash
thoth --version
thoth-gate < /dev/null    # should print {"decision":"approve",...}
# inside Claude Code:
/mcp                      # → thoth  ✓ connected
```

<!-- legacy anchors -->
<a id="getting-started"></a>
<a id="getting-started-in-30-seconds"></a>

## Configuration

`thoth setup` writes everything, but if you want to edit by hand,
`<root>/config.toml` looks like:

```toml
[memory]
episodic_ttl_days = 30
enable_nudge      = true

[discipline]
# Master switch — flip to `false` to disable the gate entirely.
nudge_before_write       = true
# Fall back to ~/.thoth when this project has no .thoth/.
global_fallback          = true
# `end` (only on Stop) or `every` (after each tool call).
reflect_cadence          = "end"
# `auto` commits straight to MEMORY.md/LESSONS.md.
# `review` stages to *.pending.md — user must promote/reject.
memory_mode              = "auto"

# --- reflection debt -------------------------------------------------
# Soft nudge threshold: once `mutations - remembers` crosses this, the
# Stop hook drops a nag marker, UserPromptSubmit prefixes every prompt
# with a "### Reflection debt" block, and `thoth curate` surfaces it.
# Set to 0 to disable the soft reminder.
reflect_debt_nudge       = 10
# Hard block threshold: PreToolUse gate returns `{"decision":"block"}`
# on Write/Edit/Bash when debt crosses this. Bypass one session with
# `THOTH_DEFER_REFLECT=1` or lower the threshold. Set to 0 to disable.
reflect_debt_block       = 20

# --- gate v2 ---------------------------------------------------------
# Verdict on a relevance miss:
#   "off"    — disable the gate (pass silently).
#   "nudge"  — pass + stderr warning.  [default]
#   "strict" — block.
mode                     = "nudge"
# Recency shortcut — a recall within this window passes without a
# relevance check. Short so ritual recall ("recall once, edit forever")
# can't sneak past.
gate_window_short_secs   = 60
# Relevance pool — how far back the gate looks for a topically matching
# recall when scoring the upcoming edit.
gate_window_long_secs    = 1800
# Containment ratio in [0.0, 1.0] — 0 disables relevance, 0.30 balanced,
# 0.50 strict. See the comment block in the generated config.toml.
gate_relevance_threshold = 0.30
# Append every decision to .thoth/gate.jsonl. Useful for calibration.
gate_telemetry_enabled   = false

# Optional: Bash prefixes that always bypass the gate (additive with
# built-ins like `cargo test`, `git status`, `grep`).
# gate_bash_readonly_prefixes = ["pnpm lint", "just check"]

# Actor-specific overrides. `THOTH_ACTOR` env var selects the policy;
# first matching glob wins. Useful when you want different gate
# behaviour for interactive Claude Code vs. an orchestrated worker
# pipeline vs. a CI bot.
# [[discipline.policies]]
# actor = "hoangsa/*"                # wave workers in a bounded-context orchestrator
# mode = "nudge"
# window_short_secs = 300
# relevance_threshold = 0.20
#
# [[discipline.policies]]
# actor = "ci-*"                     # trusted automation
# mode = "off"

grounding_check          = false
quarantine_failure_ratio = 0.66
quarantine_min_attempts  = 5
```

| Scenario                                         | `mode`   | `gate_relevance_threshold` | `memory_mode` |
|--------------------------------------------------|----------|----------------------------|---------------|
| Solo, low-friction (just get reminded)           | `nudge`  | `0.30`                     | `auto`        |
| Solo, careful (block on unrelated edits)         | `strict` | `0.30`                     | `auto`        |
| Team, experimental (review every memory write)   | `strict` | `0.30`                     | `review`      |
| Permissive warnings only                         | `nudge`  | `0.15`                     | `auto`        |
| Tight discipline (requires focused recall)       | `strict` | `0.50`                     | `auto`        |
| Automation / CI                                  | `off`    | —                          | `auto`        |

**Legacy fields** (`mode = "soft"`, `gate_window_secs`,
`gate_require_nudge`) are still parsed for backward compatibility —
`soft` maps to `nudge`, `gate_window_secs` becomes `window_short_secs`,
and `gate_require_nudge` emits a deprecation hint. Re-run `thoth setup`
to migrate to the v2 schema.

## Background review

Thoth can automatically review your coding session and persist durable
facts, lessons, and skill proposals — without you asking. Inspired by
[Hermes Agent](https://github.com/nousresearch/hermes-agent)'s
background review fork, but ~10x more token-efficient: Thoth builds
context from structured event logs (~1k tokens) instead of copying the
full conversation (~5-50k tokens).

**How it works:**

1. The `PostToolUse` hook counts mutations (Write/Edit) since the last review.
2. When the count crosses `background_review_interval`, a detached
   `thoth review` process is spawned.
3. `thoth review` assembles context from `episodes.db`, `gate.jsonl`,
   `git diff --stat`, and current `MEMORY.md` / `LESSONS.md`.
4. A single LLM call (via `claude` CLI or Anthropic API) produces
   structured JSON with facts/lessons/skills.
5. Results are deduped against existing memory and persisted.
6. A `.last-review` watermark resets the mutation counter.

**Enable in `config.toml`:**

```toml
[discipline]
background_review          = true   # opt-in (default false)
background_review_interval = 10     # mutations between reviews
background_review_backend  = "auto" # "auto" | "cli" | "api"
gate_telemetry_enabled     = true   # required (counter reads gate.jsonl)
```

| Backend | How | When |
|---------|-----|------|
| `cli`   | `claude --print --dangerously-skip-permissions` via stdin | Default — uses your Claude subscription |
| `api`   | Direct POST to `api.anthropic.com/v1/messages` | When `ANTHROPIC_API_KEY` is set |
| `auto`  | API if key is set, else CLI | Recommended default |

Run manually: `thoth review --backend cli`

## Status line

`thoth setup` installs a status line into Claude Code that shows:

```
⚡ debt:5 | 📝 12F/8L | 🔄 2m ago
```

| Segment | Meaning |
|---------|---------|
| `debt:N` | Session-scoped reflection debt (mutations minus remembers) |
| `NF/NL` | Total facts in MEMORY.md / lessons in LESSONS.md |
| `🔄 Xm ago` | Time since last background review (or "never") |

The script lives at `.claude/thoth-statusline.sh` and refreshes every 5 seconds.

## Architecture

```
  ┌── Cowork / Claude Code ────────────────────────────────────────────┐
  │                                                                    │
  │   .claude/settings.json     installed by `thoth setup`             │
  │   ├── hooks                  SessionStart / PreToolUse /           │
  │   │                          PostToolUse / Stop                    │
  │   ├── mcpServers.thoth       launches `thoth-mcp`                  │
  │   └── .claude/skills/        memory-discipline + thoth-reflect     │
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
  │            thoth.grounding_check                                   │
  │   resources thoth://memory/MEMORY.md, thoth://memory/LESSONS.md    │
  └────────────────────────┬───────────────────────────────────────────┘
                           │
                           ▼
  ┌── `.thoth/` store ─────────────────────────────────────────────────┐
  │   episodes.db           event log (query_issued, nudge_invoked…)   │
  │   graph.redb            symbol graph (Calls, Imports, Extends,     │
  │                         References, DeclaredIn edges)              │
  │   fts.tantivy/          BM25 index                                 │
  │   vectors.db            flat cosine vector index (Mode::Full)      │
  │   chunks.lance/         LanceDB vector index (Mode::Full + `lance`)│
  │   MEMORY.md             declarative facts                          │
  │   LESSONS.md            reflective lessons (active)                │
  │   LESSONS.quarantined.md  lessons auto-demoted after repeated miss │
  │   MEMORY.pending.md, LESSONS.pending.md  staged in `review` mode   │
  │   memory-history.jsonl  versioned audit trail                      │
  │   gate.jsonl            gate decisions (when telemetry enabled)    │
  │   domain/<ctx>/DOMAIN.md        accepted business rules            │
  │   domain/<ctx>/_remote/<src>/*  ingestor-written proposed snapshots│
  │   skills/               procedural skills                          │
  └────────────────────────────────────────────────────────────────────┘
```

Three enforcement layers, ordered by how bypassable they are:

1. **Prompts + skills** — SessionStart hook dumps lessons in context;
   `memory-discipline` skill guides the agent through recall/nudge/act/reflect.
2. **Hook prompts** — PreToolUse/PostToolUse hooks push short reminders
   that are hard to miss but still text.
3. **`thoth-gate`** — a native binary runs on every `Write` / `Edit` /
   `Bash` / `NotebookEdit` PreToolUse and decides from three factors:
   - **Intent.** Read-only Bash (cargo test / git status / grep / rg /
     ls / cat / ...) bypasses silently. Mutation tools continue to step 2.
   - **Recency.** If a `query_issued` event landed within
     `gate_window_short_secs`, the call passes without a relevance check.
     The short default (60s) deliberately kills "recall once, edit
     forever" patterns.
   - **Relevance.** Past the short window, the gate tokenises the edit
     context (file basename, old/new strings, diff body) and scores
     containment against every recall within `gate_window_long_secs`.
     Score ≥ `gate_relevance_threshold` passes; otherwise the policy's
     `mode` decides — `off` passes silently, `nudge` passes with a
     stderr warning, `strict` emits `{"decision":"block"}`.

   The stderr message is actionable: it lists the edit tokens, the
   top-ranked recent recalls with their overlap score, and a suggested
   `thoth_recall` query built from the tokens no recall covered. The
   agent can copy-paste it to unblock itself.

   Actor-aware policies (`THOTH_ACTOR` env var + `[[discipline.policies]]`
   glob patterns) let you run one gate binary with different thresholds
   per caller — interactive Claude Code strict, orchestrated workers
   nudge-only, CI off.

   Optional `gate_telemetry_enabled = true` writes every decision to
   `.thoth/gate.jsonl` so you can calibrate the threshold from real
   behaviour instead of guessing.

`thoth-gate` fails open on any error (missing DB, unreadable config) so
a broken gate never bricks your editor — at the cost of silently
reverting to `nudge` mode. Check stderr if the gate feels weaker than
expected.

### Reflection debt

Pre-action recall (gated by `thoth-gate`) is one half of the loop.
The other half — *post-action reflection* — used to be a prompt
contract only, so agents drifted. Since 2026-04-17 reflection is
an enforced counter in its own right.

**Definition.** `debt = mutations - remembers` within the current
session, clamped at zero. Mutations are `Write`/`Edit`/`NotebookEdit`
tool calls that the gate passed (read from `.thoth/gate.jsonl`).
Remembers are `thoth_remember_fact` / `thoth_remember_lesson` calls
(read from `.thoth/memory-history.jsonl`). Session boundary comes
from `.thoth/.session-start`, bumped by the SessionStart hook.

**Three enforcement tiers.** All use the same debt number, differ
only in where they fire:

| Tier | Hook               | Fires when        | Effect                                                          |
|------|--------------------|-------------------|-----------------------------------------------------------------|
| 1    | Stop               | `debt ≥ nudge`    | stderr nag + `.thoth/.reflect-nag` marker for next SessionStart |
| 2    | UserPromptSubmit   | `debt ≥ nudge`    | `### Reflection debt` banner injected into every prompt         |
| 2b   | PostToolUse        | `cadence=every`¹  | Same banner, fires after every mutation                         |
| 3    | PreToolUse (gate)  | `debt ≥ block`    | Hard `block` on Write/Edit/Bash until the agent remembers       |

¹ Only when `[discipline] reflect_cadence = "every"`. The default
`"end"` wires only tiers 1–3.

**Tuning.** `reflect_debt_nudge` (default `10`) and `reflect_debt_block`
(default `20`) are both in `[discipline]`. Setting either to `0`
disables that tier. Lower `nudge` for tighter loops, raise `block`
(or set to `0`) if your workflow genuinely batches many edits per
reflection.

**Escape hatches.**

- `THOTH_DEFER_REFLECT=1` — bypass tier 3 for one process. Use
  sparingly; it's a temporary release valve, not a default.
- `thoth_remember_fact` with `stage: true` — writes to
  `MEMORY.pending.md` and still counts as a remember, so agents who
  need to drain debt mid-task without committing canonical facts
  have a graceful path. Clean up later via `thoth memory reject` (or
  `thoth memory promote` to accept).
- `thoth curate` — same debt report, on demand, plus the forget pass
  and lesson-cluster detector. Whitelisted in the gate's readonly
  prefix list so it keeps working even under a tier-3 block.

**Why it works.** The counter is cheap (two JSONL tail reads, a few
ms on a session-sized log), the trigger is one user-visible number,
and every tier emits a recoverable message. No silent drift, no
guessing.

## CLI cheatsheet

```bash
# project lifecycle
thoth setup                               # interactive config wizard
thoth setup --status                      # print detected install state
thoth init                                # create .thoth/
thoth index .                             # parse + index
thoth watch .                             # stay resident, reindex on change
thoth query "how does the nudge flow work"

# graph-centric analysis (over the code graph built by `thoth index`)
thoth impact  "module::symbol" --direction up -d 3         # blast radius
thoth context "module::symbol"                             # 360° symbol view
thoth changes --from -                                      # piped git diff
thoth changes                                               # defaults to `git diff HEAD`

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

# domain (business-rule memory — needs the matching cargo feature)
thoth domain sync --source file       --from ./specs/          # air-gapped / tests
thoth domain sync --source notion     --project-id <database-id>  # needs NOTION_TOKEN
thoth domain sync --source asana      --project-id <gid>          # needs ASANA_TOKEN
thoth domain sync --source notebooklm                          # stub; export → file

# background review
thoth review                              # run once (auto backend)
thoth review --backend cli                # force claude CLI (subscription)
thoth review --backend api                # force Anthropic API (needs key)

# Claude Code wiring
thoth install                             # skills + hooks + MCP + statusline
thoth install --scope user                # global
thoth uninstall                           # remove in that scope

# eval — precision@k / MRR / latency p50·p95, optional Zero vs. Full ablation
thoth eval --gold eval/gold.toml -k 8
thoth eval --gold eval/gold.toml --mode both --embedder voyage
```

Run `thoth --help` for the full surface.

## MCP server

`thoth-mcp` speaks JSON-RPC 2.0 over stdio (MCP version `2024-11-05`).
Tools published:

| Tool                       | What it does                                                              |
|----------------------------|---------------------------------------------------------------------------|
| `thoth_recall`             | Mode::Zero hybrid recall (symbol + BM25 + graph + markdown, RRF-fused)    |
| `thoth_index`              | Walk + parse + index a path                                               |
| `thoth_impact`             | Blast-radius analysis — who breaks if `fqn` changes (depth-grouped BFS)   |
| `thoth_symbol_context`     | 360° view of a symbol: callers / callees / extends / extended_by / siblings |
| `thoth_detect_changes`     | Parse a unified diff → touched symbols + upstream blast radius            |
| `thoth_remember_fact`      | Append / stage a fact                                                     |
| `thoth_remember_lesson`    | Append / stage a lesson (refuses to silently overwrite)                   |
| `thoth_memory_show`        | Read both markdown files                                                  |
| `thoth_memory_pending`     | List staged entries                                                       |
| `thoth_memory_promote`     | Accept a staged entry                                                     |
| `thoth_memory_reject`      | Drop a staged entry with a reason                                         |
| `thoth_memory_history`     | Tail `memory-history.jsonl`                                               |
| `thoth_memory_forget`      | TTL + capacity eviction + auto-quarantine pass                            |
| `thoth_episode_append`     | Append an observed event (file edit, outcome, …) from a hook              |
| `thoth_lesson_outcome`     | Bump success/failure counters on a lesson                                 |
| `thoth_request_review`     | Flag something for human audit                                            |
| `thoth_skill_propose`      | Draft a new skill from ≥5 consolidated lessons                            |
| `thoth_skills_list`        | Enumerate installed skills                                                |

Plus two resources (`thoth://memory/MEMORY.md`, `thoth://memory/LESSONS.md`)
and three prompts (`thoth.nudge`, `thoth.reflect`, `thoth.grounding_check`)
— the nudge prompt logs a `NudgeInvoked` event the reflect pass
consumes; `thoth.grounding_check` asks the agent to verify a factual
claim against the indexed codebase before asserting it.

## Graph-centric analysis

`thoth index` builds a symbol graph with `Calls`, `Imports`, `Extends`,
`References` and `DeclaredIn` edges. Three MCP tools (and matching CLI
subcommands) expose it directly without a hybrid-recall round trip —
useful once an agent already knows which symbol it cares about.

| Use case                                              | Tool / CLI                                 |
|-------------------------------------------------------|--------------------------------------------|
| *"What breaks if I change `Foo::bar`?"*               | `thoth_impact` / `thoth impact`            |
| *"Show me everything around `Foo::bar`"*              | `thoth_symbol_context` / `thoth context`   |
| *"Which symbols do this PR's hunks actually touch?"*  | `thoth_detect_changes` / `thoth changes`   |

- **`thoth impact`** walks BFS from the symbol — `--direction up`
  (default) follows incoming `Calls`, `References`, `Extends` edges for
  "who depends on me"; `--direction down` follows outgoing edges for
  "what do I depend on". Results are grouped by depth so you can see
  direct callers separately from transitive ones.
- **`thoth context`** returns a categorised 360° view: callers, callees,
  parent types, subtypes, references, siblings in the same file, and
  any external imports the graph couldn't resolve (so third-party
  dependencies are visible without being injected as stub nodes).
- **`thoth changes`** parses a unified diff (either from `--from <file>`,
  `--from -` for stdin, or `git diff HEAD` by default), intersects each
  hunk's line range with the declaration spans of indexed symbols, and
  returns the touched symbols plus their upstream blast radius. Handy
  as a PR pre-check: "these 7 functions need re-testing because you
  modified X".

The indexer now resolves call targets through a file-local map built
from import aliases (`use foo::Bar as Baz` / `import { a as b }` /
`from x import y as z` / Go aliased imports) and same-file symbols,
so `Calls` edges connect across modules instead of dead-ending at the
bare leaf name. Class / trait inheritance emits `Extends` edges so the
two inheritance-aware columns in `symbol_context` (extends / extended_by)
populate for Rust `impl Trait for Type`, TypeScript `extends` /
`implements`, Python multi-inheritance, and friends.

## Domain memory (business rules)

Thoth's sixth memory kind (see [ADR 0001](./docs/adr/0001-domain-memory.md))
captures the *why* — business rules, invariants, workflows and glossary —
that lives outside the AST. It's a separate code path from the rest of
memory on purpose:

- **Ingest only on command.** `thoth domain sync` pulls from the selected
  remote. `recall()` never hits the network — Mode::Zero stays deterministic.
- **Snapshot-based.** Each rule lands as a single markdown file with TOML
  frontmatter (`id`, `source`, `source_hash`, `context`, `kind`,
  `last_synced`, `status`). `source_hash` (blake3) makes re-sync a no-op
  when nothing upstream changed.
- **Suggest-only merge.** Ingestor output goes to `## Proposed`. Humans
  (or CODEOWNERS) promote entries to `## Accepted` via PR. Retrieval
  ranks Accepted first.
- **Redaction first.** JWTs, provider tokens (`sk-`, `xoxb-`, `ghp_`, …),
  16-digit card numbers and AWS access keys are scanned before any write;
  hits drop the rule and log a `redacted` counter.

Build feature flags in `thoth-cli` (all opt-in, none on by default):

```bash
cargo install --git https://github.com/unknown-studio-dev/thoth \
  thoth-cli --features "notion,asana,notebooklm"
# or: thoth-cli --features full   (everything)
```

Adapters:

| Adapter | Feature | Auth | Notes |
|---|---|---|---|
| `file` | always on | — | reads `*.toml` from a directory; for air-gapped use and tests |
| `notion` | `notion` | `NOTION_TOKEN` | queries one database; routes by `Thoth.Context` property |
| `asana` | `asana` | `ASANA_TOKEN` | queries one project; routes by `Thoth.Context` custom field |
| `notebooklm` | `notebooklm` | — | stub until MCP lands; use export → `file` adapter |

Route rules to bounded contexts by setting a `Thoth.Context` property /
custom field on the source side; any rule without a context is dropped
(the `unmapped` stat). This is the ADR 0001 rule that PMs opt a record
into Thoth explicitly.

## Benchmarks

All numbers measured on MacBook Pro 14" (Nov 2023), Apple M3 Pro, 18 GB RAM, macOS 26.3.1,
release build (`cargo build --release`). Corpus: Thoth's own source tree
(**65 Rust files, ~26 k LoC, 1 313 chunks, 9 817 call edges, 1 313 symbols**).
Mode::Zero only (no embedding, no LLM calls).

### Indexing (cold, Mode::Zero)

| Metric | Value |
|--------|-------|
| Wall time (median of 3) | **~1.23 s** |
| Throughput | ~53 files/s, ~1 070 chunks/s, ~21 k LoC/s |
| CPU utilization | ~40 % (now parse-bound; was 10 % / I/O-bound pre-batching) |
| Concurrency | 11 (auto = CPU count, capped at 16) |
| Store on disk | 3.8 MB (graph.redb 3.1 MB, fts.tantivy 640 KB, episodes.db 32 KB) |

> **Optimization history:** the first version did one `redb` transaction per symbol /
> node / edge — ~13 000 fsyncs for this corpus → **40 s wall / 4 s CPU** (I/O-bound).
> Batching all writes per file into one transaction each dropped this to **1.23 s —
> a 33× speedup**. See `Indexer::index_file_no_embed` +
> `KvStore::{put_symbols_batch, put_nodes_batch, put_edges_batch}`.

### Recall (`thoth query`, top-k = 8)

| Query | Run 1 | Run 2 | Run 3 |
|-------|------:|------:|------:|
| `hybrid recall RRF fusion` | 25 ms | 24 ms | 23 ms |
| `symbol graph blast radius` | 24 ms | 23 ms | 23 ms |
| `index walk parse` | 22 ms | 22 ms | 22 ms |
| `memory lesson fact` | 22 ms | 22 ms | 22 ms |
| `gate relevance threshold` | 23 ms | 23 ms | 22 ms |
| **Median** | | **~23 ms** | |

### Eval (`thoth eval --gold eval/gold.toml -k 8`)

| Metric | Value |
|--------|-------|
| Precision@8 | **87.5 %** (7/8 gold queries hit) |
| MRR | **0.88** |
| Latency p50 | 83 ms |
| Latency p95 | 117 ms |

### Graph analysis tools

All graph tools run under **25 ms** (median of 3 runs):

| Command | Median |
|---------|-------:|
| `thoth impact <fqn>` | 22 ms |
| `thoth context <fqn>` | 22 ms |
| `thoth changes --from -` | 22 ms |
| `thoth memory show` | 22 ms |

> **Note:** Eval p50/p95 are higher than the `thoth query` median because eval
> runs every gold query back-to-back against a single opened store — the first
> few include redb page-cache warmup. Graph queries hit warm pages and are
> effectively O(1) for local lookups.

## Release flow

- Tag `vX.Y.Z` on `main`.
- `.github/workflows/release.yml` builds `aarch64-apple-darwin`,
  `x86_64-apple-darwin`, `aarch64-unknown-linux-gnu`,
  `x86_64-unknown-linux-gnu`, uploads tarballs + sha256s to the GitHub
  Release.
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
// `vectors_path` resolves to `vectors.db` by default, or `chunks.lance/`
// when built with `--features lance`. The `VectorStore` type alias follows.
let vectors  = VectorStore::open(&StoreRoot::vectors_path(".thoth".as_ref())).await?;
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

**Alpha.** Design frozen in [`DESIGN.md`](docs/DESIGN.md). Milestones M1–M6
(parse + store + graph + retrieve + CLI + MCP + Mode::Full + discipline
hooks/skills bundled by `thoth setup`) are in. **M7 — Domain memory**
(the `thoth-domain` crate with file / Notion / Asana / NotebookLM
adapters and `thoth domain sync` CLI) landed in 0.0.1-alpha; the
MCP-universal ingestor remains on the roadmap.

## License

Licensed under either of Apache License 2.0 ([LICENSE-APACHE](./LICENSE-APACHE))
or the MIT license ([LICENSE-MIT](./LICENSE-MIT)), at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.

# thoth-discipline

**🇬🇧 English** · [🇻🇳 Tiếng Việt](./README.vi.md)

A Claude Code / Cowork plugin that wraps your session in a
memory-disciplined loop. It turns a local [Thoth](https://github.com/unknown-studio-dev/thoth)
MCP server into a persistent reflective memory for the coding agent —
so Claude stops hallucinating APIs, re-learning the same conventions
every session, or ignoring lessons it learned yesterday.

> **Requires the Thoth binaries.** This plugin calls `thoth-mcp` and
> `thoth-gate` via hooks and `.mcp.json`. Install them first —
> `brew install thoth` or `npm i -g @unknownstudio/thoth-cc` — then
> install the plugin. See the [main README](../../README.md#install)
> for options.

## What it does

On every session:

1. **SessionStart** — loads `LESSONS.md` + `MEMORY.md` into context.
2. **Before every Write/Edit** — forces a `thoth.nudge` pass that names
   every applicable lesson before the file mutates.
3. **After every Bash** — records observable outcomes (tests, commits,
   errors) to the episodic log.
4. **Stop (end of turn)** — runs `thoth.reflect`, offering to persist
   any durable fact or lesson.

No paid API keys involved. The loop runs entirely against your local
Thoth daemon using your Claude Code subscription for the reasoning.

## Components

| Kind     | Name                | Purpose                                         |
| -------- | ------------------- | ----------------------------------------------- |
| Skill    | `memory-discipline` | The per-action loop: recall → nudge → act → log |
| Skill    | `thoth-reflect`     | End-of-session reflection + persistence         |
| Hooks    | `hooks/hooks.json`  | Wires the skills into SessionStart, PreToolUse, PostToolUse, Stop |
| MCP      | `thoth` (stdio)     | Launches the local `thoth-mcp` binary           |

## Setup

### 1. Install the Thoth binaries

```bash
cargo install --path crates/thoth-cli    # `thoth` CLI
cargo install --path crates/thoth-mcp    # `thoth-mcp` + `thoth-gate`
```

The `thoth-mcp` crate ships two binaries: the MCP stdio server and
`thoth-gate`, the strict-mode enforcement hook. All three need to be on
`$PATH`. Verify:

```bash
thoth --version
thoth-mcp --version
thoth-gate < /dev/null   # should print: {"decision":"approve", ...}
```

No Python, Node, or any other runtime required — just the two Rust
binaries.

### 2. Index your project

From the project root:

```bash
thoth index .
```

That creates `.thoth/` with the code graph, BM25 index, markdown files
(`MEMORY.md`, `LESSONS.md`), and the episodic SQLite log.

### 3. Install the plugin

Drop the `.plugin` file into Cowork (or `claude plugin install …` for
Claude Code). The MCP server launches automatically on session start.

### 4. (Optional) Configure enforcement

Create `<project>/.thoth/config.toml`:

```toml
[memory]
episodic_ttl_days  = 30
enable_nudge       = true

[discipline]
mode                      = "soft"      # "soft" or "strict"
global_fallback           = true        # fall back to ~/.thoth if project has none
reflect_cadence           = "end"       # "end" or "every"
nudge_before_write        = true
grounding_check           = false
gate_window_secs          = 180         # max age of a recall that still counts

# v2 knobs ---------------------------------------------------------------
memory_mode               = "auto"      # "auto" (commit) or "review" (stage)
gate_require_nudge        = false       # strict mode also requires thoth.nudge
quarantine_failure_ratio  = 0.66        # failure_count / attempts threshold
quarantine_min_attempts   = 5           # min attempts before a lesson can be quarantined
```

**Recommended presets:**

| Scenario           | `mode`   | `gate_require_nudge` | `memory_mode` |
|--------------------|----------|----------------------|---------------|
| Solo, low-friction | `soft`   | `false`              | `auto`        |
| Solo, careful      | `strict` | `false`              | `auto`        |
| Team, experimental | `strict` | `true`               | `review`      |
| Team, post-v1      | `strict` | `true`               | `auto`        |

## Soft vs. strict mode

| Aspect                          | `soft` (default)          | `strict`                          |
| ------------------------------- | ------------------------- | --------------------------------- |
| Missed the recall/nudge loop?   | Reminder appended to turn | Tool call **blocked** by hook     |
| Enforcement mechanism           | Prompt-based hooks        | Prompt hooks + `gate.py` command  |
| Gate check source of truth      | Claude's self-reflection  | `.thoth/episodes.db` (SQLite)     |
| Can the agent talk itself past? | Yes                       | No — the hook fails before tool   |

**How strict mode works.** Every `thoth_recall` call appends a
`query_issued` event to `episodes.db`. The PreToolUse hook runs the
`thoth-gate` binary (a tiny standalone executable shipped alongside
`thoth-mcp`), which opens `episodes.db` read-only and queries for the
most recent such event. If the last recall is older than
`gate_window_secs` (or there has never been one for this project), the
hook emits:

```json
{"decision": "block",
 "reason": "Thoth discipline: last `thoth_recall` was 420s ago ..."}
```

Claude Code surfaces that reason, the tool call aborts, and the agent is
forced to call `thoth_recall` before retrying. Because the gate reads
SQLite rather than asking the model, it can't be bypassed by clever
self-talk.

The gate **fails open** if `.thoth/` doesn't exist or the SQLite file is
missing — a broken gate must never brick your editor. In that case
`stderr` gets a one-line warning and the tool call proceeds.

Turn strict mode on once you've built up enough LESSONS.md entries to
justify the friction — starting with `soft` lets you observe without
being blocked.

### Two-event strict mode

Setting `gate_require_nudge = true` alongside `mode = "strict"` turns
the gate into a two-event check: before any `Write`/`Edit`/`Bash` the
agent must have logged **both** a `query_issued` (from `thoth_recall`)
**and** a `nudge_invoked` (from expanding the `thoth.nudge` prompt)
inside `gate_window_secs`. This closes the "agent ran a perfunctory
`thoth_recall` with no query body and moved on" loophole — the nudge
prompt can't be gamed because the server only logs `nudge_invoked` when
`prompts/get` is actually served for `thoth.nudge`.

## Review mode for memory

`memory_mode = "review"` changes where new facts and lessons land:

- `thoth_remember_fact` writes to `MEMORY.pending.md` (not MEMORY.md).
- `thoth_remember_lesson` writes to `LESSONS.pending.md`.
- The user (or a reviewer) promotes entries explicitly:

```bash
thoth memory pending           # list with indices
thoth memory promote lesson 0  # move lesson [0] into LESSONS.md
thoth memory reject fact 2 --reason "duplicate of §4 in MEMORY.md"
thoth memory log --limit 50    # audit trail
```

**Conflict detection.** Even in `auto` mode, the server refuses to
silently overwrite an existing lesson: if a `trigger` already exists,
the new entry is staged instead of appended, and the tool returns a
`conflict` block. The agent must flag it to the user
(`thoth_request_review`) instead of guessing.

**Self-correction.** The forget pass auto-moves lessons whose
failure_count / attempts exceeds `quarantine_failure_ratio` (after
`quarantine_min_attempts` attempts) into `LESSONS.quarantined.md`.
They're preserved, not deleted — a human can restore them manually.

**Versioning.** Every memory mutation (stage / promote / reject /
quarantine / propose / request_review) appends a JSONL entry to
`<root>/memory-history.jsonl` with timestamp, actor, and reason. That's
the audit trail — `thoth memory log` tails it.

**Skill self-improvement.** When the agent spots the same pattern in
≥5 lessons it can call `thoth_skill_propose { slug, body,
source_triggers }`. The draft lands under `.thoth/skills/<slug>.draft/`
and a `propose` entry is added to the history log. Promote with
`thoth skills install .thoth/skills/<slug>.draft`.

## Environment variables

| Var          | Default  | Meaning                                  |
| ------------ | -------- | ---------------------------------------- |
| `THOTH_ROOT` | `.thoth` | Root directory for the Thoth store       |
| `RUST_LOG`   | `info`   | `thoth-mcp` log level (`debug`, `trace`) |

## Usage examples

Every skill and hook fires automatically — you just use Claude normally.
But you can also invoke them directly:

```
> /memory-discipline
```

…forces the loop on the current turn.

```
> /thoth-reflect
```

…forces an end-of-session reflection now, without waiting for the Stop
hook.

## How this differs from a normal MCP

A vanilla MCP server gives Claude tools. This plugin goes further: it
also gives Claude **prompts** (templates that force self-reflection)
and **hooks** (automatic loop triggers). The combination is what turns
intermittent recall into a habit.

## Troubleshooting

- **"thoth-mcp not found"** — the binary isn't on `$PATH`. Re-run
  `cargo install --path crates/thoth-mcp`.
- **"no recall hits for X"** — you haven't indexed this repo. Run
  `thoth index .` from the project root.
- **Hooks feel noisy** — set `reflect_cadence = "end"` and
  `grounding_check = false` in `config.toml`.
- **Strict mode blocked my edit** — read the block reason, run
  `thoth_recall` + `thoth.nudge` explicitly, then retry.
- **Gate doesn't trigger** — make sure `thoth-gate` is on `$PATH`
  (`cargo install --path crates/thoth-mcp` installs it alongside
  `thoth-mcp`). Run `thoth-gate < /dev/null` manually to see its
  verdict; watch stderr for errors.
- **Gate fails open unexpectedly** — stderr tells you why (parse error,
  permissions, missing file). The binary always fails open so a broken
  gate never blocks you — but it also means misconfiguration reverts
  to soft mode silently. Check stderr if strict feels weak.

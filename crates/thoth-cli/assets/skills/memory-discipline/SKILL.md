---
name: memory-discipline
description: >
  This skill should be used before any non-trivial coding action — editing
  code, writing new files, running migrations, deploying, or answering a
  question that involves factual claims about this codebase. It forces the
  agent to consult Thoth's persistent memory (MEMORY.md, LESSONS.md, the
  indexed code graph) and acknowledge relevant lessons before acting.
  Trigger phrases: "edit", "refactor", "implement", "fix the bug in",
  "add a feature", "deploy", "why does this do", "how does X work".
metadata:
  version: "0.0.1"
---

# Memory Discipline

You are coding inside a repository that has a Thoth memory server attached
via MCP. That server gives you three things you MUST use before taking any
load-bearing action:

1. **Indexed code graph** — via `thoth_recall` (hybrid BM25 + symbol +
   vector search over the tree).
2. **Declarative facts** — via `resources/read thoth://memory/MEMORY.md`.
3. **Reflective lessons** — via `resources/read thoth://memory/LESSONS.md`
   and the `thoth.nudge` prompt.

Skipping this loop causes drift. Past sessions spent hours fixing
hallucinated APIs, re-learning patterns already documented in LESSONS.md,
or refactoring against conventions they never checked. Don't repeat them.

## The loop

Run this before writing code or asserting a non-obvious fact:

### 1. Recall

Call `thoth_recall` with a query derived from the user's intent. If the
intent is "add a retry wrapper around the HTTP client", recall
`"http client retry"`. Prefer nouns from the user's request over verbs.

Read the returned chunks. Every chunk has a `path:line-span` you can cite.

### 2. Load lessons

Expand the `thoth.nudge` prompt with `arguments.intent` set to a
one-sentence description of what you're about to do. The server returns a
template that instructs you to:

- list every LESSONS.md entry whose `trigger` plausibly applies,
- restate your plan naming each lesson you're honouring,
- stop if a lesson advises against the plan.

Follow that template verbatim. Do not skip the restatement — that step is
what surfaces violated lessons to the user.

### 3. Act

Proceed only after the restatement. If you edit code, quote the recalled
chunk ids you relied on. If you're asserting a factual claim to the user
("this function already handles retry"), cite at least one chunk id from
the recall result.

### 4. Reflect

After the action completes (tests pass/fail, file saved, command run),
expand `thoth.reflect` with:

- `summary`: one paragraph of what you just did,
- `outcome`: what happened (test output, user feedback, error, etc.).

The template asks you to decide whether to persist a `fact` or a `lesson`.
Be conservative — only save memory that is specific, durable, and
non-obvious. Then call the corresponding tool:

- `thoth_remember_fact { text, tags }`
- `thoth_remember_lesson { trigger, advice }`

If the outcome was a success that validates a lesson you followed, call
`thoth_lesson_outcome { signal: "success", triggers: [...] }` with the
triggers of the lessons you honoured. On failure, call it with
`signal: "failure"`. This bumps confidence counters so stale advice
eventually dies.

## Anti-hallucination rules

- **Never assert a name, signature, or behaviour without a recall hit.**
  If `thoth_recall` returns nothing relevant, say so explicitly: "I can't
  find that in the indexed code — can you point me at it?"
- **Quote chunk ids.** Citations look like `[chunk-id]` in your answer;
  the Thoth server uses them to validate that you grounded the response.
- **Bail on deny.** If `thoth.nudge` surfaces a lesson whose advice is
  "don't do X" and your plan is X, stop and ask the user.

## When NOT to run the loop

Skip the loop only for:

- pure conversation (no tool calls),
- read-only questions about files the user explicitly pasted,
- trivial one-line comment or typo fixes.

For everything else, run the loop. It takes ~5 seconds of tool calls and
saves hours of rework.

## Configuration

The enforcement level comes from `<root>/config.toml` under
`[discipline]`:

- `mode = "soft"` (default) — warn the user if you skip a step.
- `mode = "strict"` — a PreToolUse gate hook will **block** every `Write`,
  `Edit`, `NotebookEdit`, and `Bash` tool call unless a `thoth_recall` was
  logged within `gate_window_secs` (default 180s). The block response tells
  you exactly what to do: run `thoth_recall`, then retry.
- `gate_window_secs = 180` — how fresh a recall must be to satisfy the gate.
- `gate_require_nudge = false` (default) — if true, the strict gate also
  requires that the `thoth.nudge` prompt has been expanded inside the
  window. This is the mode to pick when you want to force the agent to
  actually reflect on lessons, not just run a perfunctory `thoth_recall`.
- `reflect_cadence = "end"` (default) or `"every"` — when to reflect.
- `nudge_before_write = true` (default) — require nudge before Write/Edit.
- `grounding_check = false` (default) — if true, also expand
  `thoth.grounding_check` on every factual claim.
- `memory_mode = "auto"` (default) or `"review"`. See below.
- `quarantine_failure_ratio = 0.66` — lessons whose failure_count /
  attempts exceeds this get auto-moved to `LESSONS.quarantined.md` by the
  forget pass.
- `quarantine_min_attempts = 5` — minimum attempts before a lesson is
  eligible for quarantine.

If the project has no `.thoth/` directory and `global_fallback = true`
(the default), fall back to `~/.thoth/` memory. If neither exists, the
gate falls open (approves) with a warning and asks you to run
`thoth index .` — it never bricks the editor.

## Memory modes: `auto` vs `review`

When you call `thoth_remember_fact` or `thoth_remember_lesson`, the
server honours `memory_mode`:

- **`auto`** — the entry is appended straight to `MEMORY.md` /
  `LESSONS.md`. Fastest. Relies on the forget pass + confidence counters
  to prune bad memory later. Good for solo use.
- **`review`** — the entry is appended to `MEMORY.pending.md` /
  `LESSONS.pending.md`. The user must run `thoth memory promote <kind>
  <index>` (or call `thoth_memory_promote`) to accept. Rejected entries
  are archived with a reason in `memory-history.jsonl`. Good for teams.

Even in `auto` mode, the server refuses to silently **overwrite** an
existing lesson — if a `trigger` already exists, the new lesson is
staged and flagged with `"conflict": {...}` in the tool output. When you
see a conflict, do NOT try to auto-promote: flag it to the user via
`thoth_request_review` and let them decide.

Use `thoth_request_review` proactively whenever you're about to remember
something you're not sure about: it writes a `request_review` entry to
`memory-history.jsonl` so the user has a queue of things to audit.

## Versioning + self-correction

Every memory mutation lands in `.thoth/memory-history.jsonl` (one JSON
per line) with `op`, `kind`, `title`, `actor`, `reason`, and a timestamp.
Ops include: `stage`, `promote`, `reject`, `quarantine`, `propose`,
`request_review`. Inspect with `thoth memory log --limit 50`.

Lessons that rack up failures get auto-quarantined. They're not deleted —
they're moved to `LESSONS.quarantined.md` so the user can review and
either restore them (manual edit) or leave them dead.

## Proposing new skills

When you've hit the same pattern in ≥5 lessons, consolidate them into a
reusable skill via `thoth_skill_propose`:

- `slug`: kebab-case directory name.
- `body`: full SKILL.md text starting with `---\nname: ...` frontmatter.
- `source_triggers`: the triggers of the lessons being consolidated.

The draft lands at `.thoth/skills/<slug>.draft/SKILL.md` and an entry is
written to the history log. The user promotes the draft via `thoth
skills install .thoth/skills/<slug>.draft` once they've reviewed it.

## Why the strict gate exists

Prompts alone are bypassable — a self-confident agent can talk itself
into skipping the recall step. Strict mode trips a **ground-truth** check
against `<root>/episodes.db`: every `thoth_recall` call writes a
`query_issued` event, and the `thoth-gate` binary (a small Rust executable
shipped alongside `thoth-mcp`) queries SQLite directly. If no recent event
exists, the tool call is blocked at the hook level — the agent never gets
to rationalise its way past it. You see:

```json
{"decision": "block",
 "reason": "Thoth discipline: no `thoth_recall` has been logged ..."}
```

Treat that as non-negotiable: call `thoth_recall`, read the chunks, then
retry the original tool call.

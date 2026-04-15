---
name: thoth
description: Long-term memory for coding agents. Use this skill when working inside a repository that has a `.thoth/` directory — recall prior context before answering questions about the codebase, and record new facts/lessons after non-trivial work. Triggers include phrases like "where is X handled", "why did we do Y", "what did we learn about Z", or any task that would benefit from durable memory across sessions.
---

# Thoth — code memory for agents

Thoth is a long-term memory system for coding agents. It indexes the current
repository into a hybrid store (BM25 full-text + symbol graph + optional
semantic vectors) and exposes memory-editing primitives (`MEMORY.md`,
`LESSONS.md`) that persist across sessions.

Use Thoth **whenever you are about to reason about code in a repo that has a
`.thoth/` directory**. That directory is the signal the user opted in.

## When to invoke

- **Before answering** any question about the current codebase — recall first,
  then synthesize. Do not speculate from filenames alone.
- **After finishing** a non-trivial change — consider whether a fact or lesson
  should be persisted so the next session doesn't relearn it.
- **When the user mentions** "last time", "we decided", "we tried",
  "convention here", "why does X", "where is X" — that's a memory lookup.
- **At session end** (Mode::Full only) — run `thoth memory nudge` so the LLM
  can distill reusable lessons from the session's outcomes.

Skip Thoth when the task is a one-off, repo-agnostic question (e.g. "what
does `git rebase -i` do?").

## Two ways to call Thoth

Prefer the MCP server if it's connected — it avoids shelling out. Fall back
to the CLI otherwise.

### A. MCP tools (preferred when `thoth-mcp` is wired up)

| Tool                 | What it does                                          |
|----------------------|-------------------------------------------------------|
| `recall`             | Hybrid retrieval. Input: `{text, top_k?}`. Returns chunks + optional synthesized answer. |
| `index`              | Re-index a path. Input: `{path?}`.                    |
| `remember_fact`      | Append a fact to `MEMORY.md`. Input: `{text, tags?}`. |
| `remember_lesson`    | Append a lesson to `LESSONS.md`. Input: `{when, advice}`. |
| `memory_show`        | Dump `MEMORY.md` + `LESSONS.md`.                      |
| `memory_forget`      | Run TTL + capacity eviction over the episodic log.    |
| `skills_list`        | List installed skills under `<root>/skills/`.         |

### B. CLI (fallback — works from any shell)

```bash
thoth query "<question>" -k 8 --json          # recall
thoth index .                                  # (re)index the tree
thoth memory fact "uses JWT in a cookie named 'sid'" --tags auth,session
thoth memory lesson --when "adding a migration" "run `make db-check` first"
thoth memory show                              # dump MEMORY.md + LESSONS.md
thoth memory forget                            # TTL pass
thoth memory nudge                             # Mode::Full: LLM-distilled lessons
```

Add `--embedder voyage|openai|cohere` and/or `--synth anthropic` to use
Mode::Full (semantic search + synthesized answers). Requires the matching
feature build and env var (e.g. `VOYAGE_API_KEY`).

## Recommended workflow

1. **Start of task** — call `recall` with the user's question. If the top
   chunks include a relevant `Lesson` from `LESSONS.md`, follow it.
2. **While working** — if you uncover a load-bearing fact ("this module is
   actually three files, not one"), append it with `remember_fact` so the
   next session starts ahead.
3. **After a failure** — if you had to learn something the hard way,
   append a lesson with `remember_lesson`:
   - `when`: concise trigger pattern (e.g. `"editing a migration in crates/db"`)
   - `advice`: the rule (e.g. `"always run sqlx-cli prepare before commit"`)
4. **End of session (Mode::Full only)** — run `memory_forget` to trim old
   episodes, then `thoth memory nudge` to let the LLM propose lessons.

## Examples

### Recall before answering

> User: "Where do we handle auth tokens?"

```text
→ MCP: recall { "text": "auth token handling", "top_k": 5 }
← Top hit: crates/api/src/auth.rs:42  fn verify_token(...)
← Top hit: MEMORY.md — fact: "JWT is signed with RS256, key rotates weekly"
```

Now answer using those chunks — cite file:line.

### Record a lesson after a close call

> You just spent 20 minutes debugging why `cargo test` deadlocked in CI,
> caused by a shared `parking_lot::Mutex` held across `.await`.

```text
→ MCP: remember_lesson {
    "when": "holding a mutex across an .await in async code",
    "advice": "use tokio::sync::Mutex, or drop the guard before awaiting"
  }
```

### Record a durable fact

> While grep'ing, you discover that the `events` table is partitioned by
> month, which is non-obvious.

```text
→ MCP: remember_fact {
    "text": "events table is list-partitioned by month; queries must include at_unix_ns in WHERE",
    "tags": ["database", "events"]
  }
```

## Anti-patterns

- **Don't re-index on every tool call.** The `PostToolUse` hook handles
  incremental indexing — manual `index` is only needed after a bulk change
  outside the agent's control (e.g. `git pull`).
- **Don't dump `memory_show` into every prompt.** The `UserPromptSubmit`
  hook and `SessionStart` hook already prime the context. Call it
  explicitly only when the user asks "what do you know about X".
- **Don't invent lessons.** A lesson should come from a real observed
  outcome in this session — not a guess.

## Relation to hooks

If `thoth hooks install` was run in this repo, Claude Code already
automatically:

- dumps memory at `SessionStart`,
- injects top-k recall on every `UserPromptSubmit`,
- re-indexes any file after `Edit` / `Write` / `MultiEdit`,
- runs `memory nudge` on `Stop` / `SessionEnd` (Mode::Full).

This skill stays useful even without hooks — hooks automate the *passive*
path, the skill directs the *active* path (when you decide to remember or
recall something on purpose).

---
name: thoth-reflect
description: >
  This skill should be used at the end of a coding session, after a
  bug fix, a finished feature, a deployment, or whenever the user says
  "we're done", "wrap up", "summarize the session", "what did we learn",
  or "save what you learned". It drives a structured self-reflection
  pass that decides whether to persist new facts, lessons, or skills to
  Thoth's long-term memory. Also triggers on phrases like "reflect",
  "postmortem", or "retrospective" applied to the current session.
metadata:
  version: "0.0.1"
---

# Thoth Reflect

Drive a deliberate reflection pass over the session that just ended.
The goal is to extract durable memory — not chat noise. Thoth already
captured the raw episodic log; your job is to decide what deserves to
survive after the TTL sweeps the log clean.

## Procedure

### 1. Pull the timeline

Call the Thoth MCP server:

- `thoth_memory_show` — current `MEMORY.md` + `LESSONS.md` contents, so
  you know what's already there and don't duplicate it.
- `resources/read thoth://memory/MEMORY.md` (same data, different wire
  shape — use whichever your client supports).

If the session had enough tool calls to make a summary expensive, ask the
user for a one-paragraph recap instead of reading the full log.

### 2. Run the reflect prompt

Expand the MCP prompt `thoth.reflect`:

```
prompts/get {
  "name": "thoth.reflect",
  "arguments": {
    "summary": "<one-paragraph summary of this session>",
    "outcome": "<tests passed, user shipped, bug still open, etc.>"
  }
}
```

The server returns a user-role message telling you exactly what to
decide. Follow it literally. It asks three questions:

1. Is there a durable FACT worth saving?
2. Is there a LESSON — a non-obvious pattern a future session would miss?
3. If neither, reply `no memory needed`.

### 3. Persist

For each decision from step 2:

**Facts** — call `thoth_remember_fact`:

```
thoth_remember_fact {
  "text": "The HTTP client in crates/net uses its own retry wrapper; the
           reqwest defaults are overridden in src/client.rs:42.",
  "tags": ["net", "retry"]
}
```

Keep the first line crisp — it becomes the MEMORY.md heading. Don't
restate things already visible from the file tree (e.g. "this repo is a
Rust workspace" is not a fact worth persisting).

**Lessons** — call `thoth_remember_lesson`:

```
thoth_remember_lesson {
  "trigger": "adding a retry to an HTTP call in this repo",
  "advice": "Use the existing RetryPolicy in crates/net/retry.rs; do not
             add reqwest middleware directly or it double-retries."
}
```

Lessons fire when their `trigger` matches a future intent, so write the
trigger as a situation description, not a command.

### 4. Confirm and close

Write a two-line summary to the user:

```
Saved to memory:
  - fact:   "HTTP retry lives in crates/net/retry.rs"
  - lesson: "trigger → advice"
  (or) no durable memory this session.
```

Don't dump the full MEMORY.md back — the user can open the file.

## Quality gates

A memory entry is worth persisting only if:

- it would save a future session a round-trip (e.g. a recall that failed
  and a chunk that wasn't indexed),
- it encodes a **decision** or **convention**, not a raw file path,
- it wouldn't be obvious from a five-minute read of the README,
- it's not already in MEMORY.md / LESSONS.md under a similar trigger.

If none of those apply, emit `no memory needed` and stop. False positives
pollute the store faster than missing entries hurt.

## Failure modes to avoid

- **Diary-style dumps.** "Today we fixed the parser." Not a fact. Skip.
- **Overly specific code lines.** The code graph already holds that.
  Facts should live at a higher level than symbols.
- **Duplicate lessons.** Check existing LESSONS.md first; if the trigger
  already exists, bump its confidence via `thoth_lesson_outcome` instead
  of appending a near-duplicate.
- **Lessons for one-off bugs.** A lesson earns its place by being likely
  to recur across sessions.

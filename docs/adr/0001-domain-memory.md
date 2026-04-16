# ADR 0001 — Domain Memory (business logic layer)

- Status: **Proposed**
- Date: 2026-04-16
- Deciders: @thevansit
- Related: `DESIGN.md` §5 (five memory kinds), §6 (Mode::Zero / Full), §7 (on-disk layout)

## Context

Thoth's `Semantic` memory captures *code facts* — symbols, call graphs, imports,
references — via tree-sitter. It answers "what calls X?" well, but cannot
answer "why does this enforce a $500 refund limit?". Business rules,
invariants, workflows and ubiquitous language live *outside* the AST:
in the heads of PMs, in Jira tickets, in Notion specs, in NotebookLM
notebooks, occasionally in code comments.

Agents built on Thoth therefore refactor blindly, miss compliance edges,
and cannot explain *why* — the gap between *code-aware* and
*codebase-aware*.

## Decision

Introduce a **sixth memory kind: `Domain`**, sitting alongside
Working / Semantic / Episodic / Procedural / Reflective.

1. **Source of truth is markdown on disk** (`<root>/domain/<context>/DOMAIN.md`)
   — git-reviewable, rebuildable indexes. Same discipline as
   `MEMORY.md` / `LESSONS.md`.

2. **Layout is hierarchical by bounded context**:
   ```
   .thoth/domain/
     index.md                     ← glossary + bounded-context map
     <context>/
       DOMAIN.md                  ← human-authored rules (## Accepted)
       _remote/<source>/<id>.md   ← ingestor-written cache (## Proposed)
   ```
   Flat layout allowed for small repos; `layout = "hierarchical"` in
   config enables per-context sharding.

3. **Four write paths, one review gate.** All ingested content lands in a
   `## Proposed` section. Only human PR (or an owner, CODEOWNERS-style)
   promotes an entry to `## Accepted`.

   | Path | Trust | Source |
   |---|---|---|
   | Human PR | high | editor |
   | Remote sync (Notion / Asana / NotebookLM / …) | medium | external tool |
   | LLM nudge (Mode::Full) | low | session diff + conversation |
   | Test extraction | high (narrow) | test assertions |

4. **Remote ingest via `DomainIngestor` trait** — same shape as the
   existing `Embedder` / `Synthesizer` traits. Feature-gated adapters:
   `notion`, `asana`, `notebooklm`, plus a `file` adapter always on
   for tests and air-gapped usage.

5. **Snapshots are first-class markdown** with TOML frontmatter:
   ```markdown
   ---
   id: jira-PROJ-1234
   source: jira
   source_uri: https://foo.atlassian.net/browse/PROJ-1234
   source_hash: blake3:ab...
   context: billing
   kind: invariant
   last_synced: 2026-04-16T08:00:00Z
   status: proposed
   ---
   # Refund over $500 requires manager approval
   ...
   ```
   `source_hash` enables drift detection; `status` gates what retrieval
   returns in Mode::Zero.

6. **Mode::Zero preserved.** Retrieval only reads on-disk snapshots. All
   remote API calls live in `thoth domain sync` — never in `recall()`.
   DESIGN.md §6 determinism guarantee intact.

## Consequences

Positive:

- Agent can answer *why* questions, not just *what*.
- PM-owned rules in Jira / Notion / Asana flow in without dev gluecode.
- `source_hash` → drift alarm when upstream changed.
- Adapter pattern mirrors `Embedder` → consistent mental model.

Negative / costs:

- +1 memory kind → schema surface, tests, docs.
- CODEOWNERS-style ownership file required to prevent "everyone's file".
- Redaction filter mandatory (PII leaks from Jira are not undo-able).
- Initial trait shape may shift when the second adapter lands (Rule of
  Three) — kept intentionally small.

## What we explicitly do NOT do in this ADR

These were discussed and deferred until metrics justify:

- Hot/cold tier + 2-pass summary-first retrieval.
- Auto-detect bounded contexts from code graph.
- Multi-context rule with symlink references.
- Universal MCP adapter.
- Bidirectional sync (write back to external).
- Keychain-based credential storage.

Flat layout + env-var auth + suggest-only merge mode ship first. Revisit
each of the above when a concrete user or metric demands it.

## Alternatives considered

- **Reuse Procedural skills** for business workflows. Rejected: skills are
  imperative playbooks, business rules are declarative invariants. Mixing
  corrodes both abstractions.
- **LLM-only extraction from code**. Rejected: bypasses PM ownership and
  requires Mode::Full. Sits badly with DESIGN.md §2 (scope) and §6 (Zero).
- **External service / microservice**. Rejected: violates DESIGN.md §2
  (Thoth is a library, not a server).

## Rollout

1. Land trait + types + FileIngestor + snapshot writer + CLI command.
2. Ship one real adapter (Notion).
3. Ship second adapter (Asana) to validate trait shape.
4. NotebookLM via MCP — no public read API, separate design note when
   MCP universal adapter lands.
5. Retrieve wiring (tantivy ingest of snapshots) is follow-up — out of
   scope for v1 so the first cut stays <1k LoC.

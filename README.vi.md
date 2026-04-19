<p align="center">
  <img src="docs/thoth.png" alt="Thoth" width="200" />
</p>

<h1 align="center">Thoth</h1>
<p align="center"><em>"Thoth -- than ghi chep, nguoi luu giu tri thuc."</em></p>

<p align="center">Bo nho dai han cho Claude Code agent. Rust-native, code-aware, khong can API.</p>

<p align="center"><a href="./README.md">English</a> · <strong>Tieng Viet</strong></p>

<p align="center">
  <a href="https://github.com/unknown-studio-dev/thoth/actions/workflows/ci.yml"><img src="https://github.com/unknown-studio-dev/thoth/actions/workflows/ci.yml/badge.svg" alt="ci" /></a>
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/Rust-1.91%2B-orange?logo=rust" alt="Rust" /></a>
  <a href="https://github.com/unknown-studio-dev/thoth/releases"><img src="https://img.shields.io/badge/version-0.0.1--alpha-blue" alt="version" /></a>
  <a href="./LICENSE-MIT"><img src="https://img.shields.io/badge/license-MIT%2FApache--2.0-green" alt="license" /></a>
</p>

> [!WARNING]
> **Dang phat trien -- chua san sang cho production.** API, dinh dang luu tru va CLI flag co the thay doi bat cu luc nao. **Khong** nen dung cho production.

---

## Tong quan

Thoth cho Claude Code agent bo nho dai han, co to chuc cho codebase.
Parse source bang tree-sitter, build symbol graph (calls, imports,
extends, references), index bang BM25 (tantivy) va RRF fusion, ep gate
"recall truoc khi write" de agent phai hoi memory truoc khi sua code.

Ba binary:

1. **`thoth`** -- CLI: setup, index, query, eval, memory ops.
2. **`thoth-mcp`** -- MCP stdio server (41 tools, 3 prompts, 2 resources).
3. **`thoth-gate`** -- PreToolUse hook: block write neu chua recall.

`thoth setup` wire tat ca -- hooks, MCP server, skills, config -- mot
lenh duy nhat.

Du lieu khong roi may ban tru khi ban cho phep.

---

## Cai dat

```bash
npx @unknownstudio/thoth        # tai binary + chay setup wizard
```

Kenh khac:

```bash
brew install unknown-studio-dev/thoth/thoth
# hoac
cargo install --git https://github.com/unknown-studio-dev/thoth thoth-cli thoth-mcp
thoth setup
```

## Bat dau nhanh

```bash
thoth index .                                    # build code index
thoth query "gate hoat dong the nao"             # hybrid recall
thoth impact "module::symbol" --direction up      # blast radius
thoth memory fact "token het han sau 15m" --tags auth
```

Trong Claude Code, gate va skills tu dong hoat dong sau setup.

---

## Benchmarks

Tat ca so do tu repo nay voi cac lenh ben duoi. May: MacBook Pro 14"
(Nov 2023), Apple M3 Pro, 18 GB RAM, release build. Corpus: source tree
cua chinh Thoth (**109 file Rust, ~47k LoC**). Mode::Zero (khong
embedding, khong LLM).

**Recall accuracy (seeded gold set, 10 query tren facts + lessons + code):**

```bash
cargo test -p thoth-retrieve --test recall_accuracy -- --nocapture
```

| Metric | Gia tri |
|--------|---------|
| R@5 | **100 %** (10/10) |
| R@3 | **100 %** (10/10) |
| Target | R@5 >= 80 %, R@3 >= 60 % |

Test seed 8 fact, 5 lesson, 3 file Rust vao temp store, chay 10
natural-language query va assert moi query tim dung expected substring
trong top-k.

**Eval tren source tree cua Thoth (8 gold query):**

```bash
thoth eval --gold eval/gold.toml -k 8
```

| Metric | Gia tri |
|--------|---------|
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
| In | leaf sau nhat | **13.5 us** |
| Both | leaf sau nhat | **501 us** |

Synthetic 4-ary tree (~341 nodes, 5 levels), BFS depth 8.

---

## Memory

Bon loai bo nho hoat dong, mot store:

| Loai | Luu tru | Mo ta |
|------|---------|-------|
| **Semantic** | `graph.redb` + `fts.tantivy/` | Symbol, call, import, reference (tree-sitter) |
| **Episodic** | `episodes.db` (SQLite FTS5) | Moi query, outcome, event -- timeline cho reflect |
| **Reflective** | `LESSONS.md` | Bai hoc tu sai lam, confidence-scored, auto-quarantined |
| **Domain** | `domain/<ctx>/` | Business rule sync tu Notion / Asana / file |

Fact nam trong `MEMORY.md`, preference trong `USER.md`.

## Gate (search-before-write)

`thoth-gate` chay moi PreToolUse cua Write/Edit/Bash, quyet dinh theo
ba yeu to: **intent** (Bash read-only bypass), **recency** (recall gan
day pass), **relevance** (edit token scored voi recall history). Mode:
`off` / `nudge` (mac dinh) / `strict`.

Reflection debt (`mutations - remembers`) them vong enforcement thu hai:
nudge o 10, hard block o 20. Tinh chinh trong `config.toml`.

## Knowledge graph

Temporal entity-relationship triple voi validity window -- add, query,
invalidate, timeline -- backend SQLite. Co qua MCP (`thoth_kg_*`) va CLI.

## MCP server

41 tools cover recall, memory CRUD, graph analysis, knowledge graph,
override, workflow, va conversation archive. 3 prompts va 2 resources.

Xem bang tool day du tai [CLAUDE.md](./CLAUDE.md).

## Background review & compact

`thoth review` -- LLM review session tu dong, spawn boi PostToolUse
hook. Build context tu event log (~1k token). Mac dinh `claude-haiku-4-5`.

`thoth compact` -- gop near-duplicate fact/lesson. Xem truoc voi
`--dry-run`. Backup truoc khi ghi de.

## Domain memory

Business rule sync tu nguon ngoai qua `thoth domain sync`.
Adapter feature-gated:

| Adapter | Feature | Auth |
|---------|---------|------|
| `file` | luon bat | -- |
| `notion` | `notion` | `NOTION_TOKEN` |
| `asana` | `asana` | `ASANA_TOKEN` |
| `notebooklm` | `notebooklm` | -- (stub; export sang file) |

## CLI cheatsheet

```bash
thoth setup                            # wizard
thoth index .                          # parse + index
thoth watch .                          # resident, reindex khi file doi
thoth query "nudge flow"               # hybrid recall

thoth impact  "mod::sym" -d 3          # blast radius
thoth context "mod::sym"               # 360 symbol view
thoth changes                          # git diff HEAD -> symbol bi dung

thoth memory show                      # doc MEMORY.md + LESSONS.md
thoth memory fact "..." --tags x,y     # them fact
thoth memory lesson --when "..." "..." # them lesson
thoth memory forget                    # TTL + quarantine pass

thoth review                           # LLM session review
thoth compact --dry-run                # xem truoc memory consolidation

thoth domain sync --source file --from ./specs/
thoth eval --gold eval/gold.toml -k 8  # eval precision@k
thoth install                          # wire Claude Code
thoth uninstall                        # go wiring
```

`thoth --help` de xem day du.

## Dung nhu library

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

## Yeu cau

- Rust >= 1.91 (build tu source)
- Git >= 2.30

Khong can API key cho Mode::Zero.

## Dong gop

Xem [`CONTRIBUTING.md`](./CONTRIBUTING.md).

## Trang thai

**Alpha.** Core hoat dong: parse, store, graph, retrieve, CLI, MCP,
gate, reflection debt, background review, domain sync, knowledge graph,
conversation archive. On-disk format co the con thay doi.

## License

Dual license: [Apache 2.0](./LICENSE-APACHE) hoac [MIT](./LICENSE-MIT).

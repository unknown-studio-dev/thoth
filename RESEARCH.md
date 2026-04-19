# Research: Kiến trúc Thoth — Phân tích thừa/thiếu

## Tech Stack

- **Ngôn ngữ**: Rust (MSRV 1.91, resolver v3)
- **Async runtime**: Tokio (multi-threaded)
- **Storage**: redb (KV), tantivy (BM25 FTS), rusqlite + FTS5 (episodes), ChromaDB sidecar (vectors)
- **Parsing**: tree-sitter (Rust, Python, JS, TS, Go)
- **CLI**: clap 4.5
- **MCP**: JSON-RPC 2.0 qua stdio + Unix socket
- **Build**: `opt-level=3`, `lto=thin`, `strip=symbols` (release)

---

## Sơ đồ kiến trúc (Dependency Graph)

```
                    ┌──────────────┐
                    │  thoth-core  │  ← Leaf: types, traits, errors
                    └──────┬───────┘
                           │
              ┌────────────┼────────────┬──────────────┐
              ▼            ▼            ▼              ▼
        ┌──────────┐ ┌──────────┐ ┌──────────┐  ┌───────────┐
        │thoth-parse│ │thoth-store│ │thoth-synth│  │thoth-domain│
        │ (parser)  │ │(storage) │ │  (LLM)   │  │ (biz rule) │
        └────┬─────┘ └────┬─────┘ └──────────┘  └───────────┘
             │            │
             │       ┌────┴─────┐
             │       ▼          ▼
             │  ┌──────────┐ ┌──────────┐
             │  │thoth-graph│ │thoth-memory│
             │  │(call graph)│ │(lifecycle)│
             │  └────┬─────┘ └────┬──────┘
             │       │            │
             ▼       ▼            ▼
        ┌─────────────────────────────┐
        │       thoth-retrieve        │  ← Orchestrator: index + recall
        │  (parse → store → graph →   │
        │   fts → chroma → RRF)       │
        └──────────┬──────────────────┘
                   │
          ┌────────┼────────┐
          ▼        ▼        ▼
    ┌──────────┐ ┌────┐ ┌──────────┐
    │thoth-mcp │ │thoth│ │thoth-cli │
    │(MCP srv) │ │(lib)│ │  (CLI)   │
    └──────────┘ └────┘ └──────────┘
```

### Layer breakdown

| Layer | Crate | Vai trò |
|-------|-------|---------|
| **Leaf** | `thoth-core` | Types, traits, errors — zero dependency nội bộ |
| **Perception** | `thoth-parse` | tree-sitter parser, file walker, file watcher |
| **Storage** | `thoth-store` | KV (redb), FTS (tantivy), episodes (SQLite), markdown, archive, chroma |
| **Graph** | `thoth-graph` | Symbol/call graph trên KvStore, blast-radius analysis |
| **Lifecycle** | `thoth-memory` | Memory policy: TTL, confidence, promotion, nudge, rules, workflows |
| **Business** | `thoth-domain` | Business rules từ Notion/Asana, PII redaction |
| **Synthesis** | `thoth-synth` | LLM adapters (Anthropic), feature-gated |
| **Orchestrator** | `thoth-retrieve` | Index pipeline + retrieval pipeline (RRF fusion) |
| **Client** | `thoth-mcp` | MCP stdio server + gate binary |
| **Client** | `thoth-cli` | User-facing CLI, ~3000 dòng main.rs |
| **Facade** | `thoth` | Public library: `CodeMemory` facade re-export |

---

## Relevant Files (mới thêm / đang thay đổi)

| File | Mô tả |
|------|-------|
| `crates/thoth-store/src/chroma.rs` | **MỚI** — ChromaDB client qua Python sidecar |
| `crates/thoth-store/src/archive.rs` | **MỚI** — Archive session tracker (SQLite) |
| `crates/thoth-cli/assets/chroma_sidecar.py` | **MỚI** — Python subprocess cho ChromaDB |
| `crates/thoth-cli/src/archive_cmd.rs` | **MỚI** — CLI commands: ingest/status/topics/search |
| `crates/thoth-cli/src/resolve.rs` | **MỚI** — Root resolution logic |

---

## Patterns & Conventions

- **Error handling**: `thiserror` enums + `anyhow::Error` escape hatch, `From` impls bridge crate boundaries
- **Async**: `tokio` + `async_trait`, `#[tokio::test]` cho tests
- **Serialization**: `serde` derive, TOML cho config, JSON cho runtime
- **Config**: Async `load_or_default()`, `deny_unknown_fields`, process-lifetime `OnceLock` cache
- **Naming**: snake_case, module-per-concern
- **Logging**: `tracing` crate (`debug!`, `warn!`, `info!`), `tracing-subscriber` + `env-filter`
- **API surface**: Builder pattern (`with_*`), trait-based contracts (`Synthesizer`)

---

## Relevant Dependencies

| Package | Version | Dùng cho |
|---------|---------|----------|
| `tokio` | 1.43 | Async runtime (toàn workspace) |
| `redb` | 4.0.0 | Embedded KV store (graph, symbols) |
| `tantivy` | 0.26.0 | BM25 full-text search |
| `rusqlite` | 0.39.0 | SQLite (episodes, archive, gate) |
| `tree-sitter` | 0.23 | Source code parsing |
| `serde` + `serde_json` | 1.0.x | Serialization |
| `blake3` | 1.5 | Content hashing |
| `reqwest` | 0.12 | HTTP client (optional, feature-gated) |
| `clap` | 4.5 | CLI framework |
| `chromadb` (Python) | latest | Vector embeddings via sidecar |

---

## CHỖ THỪA (Redundancies)

### 1. `thoth-domain` — chưa tích hợp sâu, có thể gộp

**Vấn đề**: `thoth-domain` chỉ được `thoth-cli` dùng trực tiếp. Nó có adapter cho Notion/Asana nhưng đều là feature-gated stubs. Crate này **không** được `thoth-retrieve`, `thoth-mcp`, hay `thoth` (facade) reference.

**Hệ quả**: Domain rules được ingest nhưng không tham gia vào retrieval pipeline. Chúng nằm riêng, không được recall hay RRF fusion tìm thấy.

**Khuyến nghị**: Hoặc tích hợp domain rules vào retrieval pipeline (thêm `DomainStore` backend cho `Retriever`), hoặc gộp logic vào `thoth-memory` nếu domain rules thực chất là một dạng fact/lesson.

### 2. `thoth-synth` — quá mỏng, 18 dòng lib.rs

**Vấn đề**: Crate chỉ có 1 module (`anthropic`), 18 dòng lib.rs. Trait `Synthesizer` đã nằm ở `thoth-core`. Crate này chỉ là 1 impl.

**Khuyến nghị**: Gộp vào `thoth-retrieve` hoặc `thoth` (facade) dưới dạng feature-gated module. Giảm 1 crate, ít overhead hơn.

### 3. Hai thư viện hash: `blake3` + `sha2`

**Vấn đề**: `blake3` dùng khắp nơi cho content hashing. `sha2` chỉ dùng ở `thoth-mcp/bin/gate.rs` cho override matching.

**Khuyến nghị**: Thay `sha2` bằng `blake3` trong gate nếu không cần SHA-2 compatibility. Giảm 1 dependency.

### 4. `thoth-cli/src/main.rs` — 2991 dòng, quá lớn

**Vấn đề**: File main.rs chứa logic của ~15 subcommands. Mặc dù đã tách `archive_cmd.rs` và `resolve.rs`, phần còn lại vẫn monolithic.

**Khuyến nghị**: Tách mỗi subcommand thành file riêng (`index_cmd.rs`, `query_cmd.rs`, `memory_cmd.rs`, ...). Giữ `main.rs` chỉ là clap dispatch.

### 5. `thoth` (facade) không re-export `thoth-graph`

**Vấn đề**: `thoth` re-export `thoth-core`, `thoth-memory`, `thoth-parse`, `thoth-retrieve`, `thoth-store` — nhưng **không** re-export `thoth-graph`. Callers muốn dùng `Graph::impact()` phải depend trực tiếp vào `thoth-graph`.

**Khuyến nghị**: Re-export `thoth-graph` qua facade, hoặc expose `impact()` qua `CodeMemory` method.

### 6. Deleted nhưng chưa clean: `thoth-embed`, vector stores

**Vấn đề**: Git status cho thấy `crates/thoth-embed/` đã bị xóa, cùng với `vector.rs` và `vector_lance.rs` trong `thoth-store`. Nhưng có thể còn references trong code hoặc docs.

**Khuyến nghị**: Grep toàn bộ workspace cho `thoth-embed`, `vector_lance`, `lancedb` references. Xóa sạch remnants.

---

## CHỖ THIẾU (Gaps)

### 1. ❌ Không có integration test end-to-end

**Vấn đề**: Tests hiện tại là unit + crate-level integration. Không có test nào chạy full pipeline: `index → recall → remember → recall again`. MCP server tests (`rpc.rs`) gần nhất nhưng không cover full flow.

**Khuyến nghị**: Thêm `tests/e2e/` ở workspace root. Test golden path: setup → index sample project → recall → remember fact → verify fact appears in recall.

### 2. ❌ Không có CI/CD config

**Vấn đề**: Không thấy `.github/workflows/`, `Makefile`, hay CI script nào.

**Khuyến nghị**: Thêm GitHub Actions workflow: `cargo test --workspace`, `cargo clippy`, `cargo fmt --check`. Đặc biệt quan trọng khi có feature flags (`anthropic`, `notion`, `asana`).

### 3. ❌ Docs bị xóa, chưa thay thế

**Vấn đề**: `docs/DESIGN.md` và `docs/adr/0001-domain-memory.md` đã bị xóa. `RESEARCH.md` cũng bị xóa. Không còn tài liệu kiến trúc nào.

**Khuyến nghị**: Tạo `docs/ARCHITECTURE.md` tối thiểu — có thể dựa trên output của research này. ADR (Architecture Decision Records) nên được giữ lại hoặc thay bằng mechanism khác.

### 4. ❌ ChromaDB sidecar thiếu error recovery

**Vấn đề**: `chroma.rs` giao tiếp với Python sidecar qua JSON-RPC over pipes. Nếu sidecar crash, không có restart logic. Nếu Python chưa cài `chromadb`, error message có thể không rõ.

**Khuyến nghị**: Thêm health check + auto-restart cho sidecar. Validate `python3 -c "import chromadb"` trước khi spawn.

### 5. ❌ Không có benchmarks cho retrieval pipeline

**Vấn đề**: `criterion` benchmarks có trong `thoth-store` và `thoth-retrieve` (dev deps), nhưng không thấy benchmark files thực tế.

**Khuyến nghị**: Thêm benchmarks: `index 1000 files`, `recall latency`, `RRF fusion overhead`. Quan trọng khi scale lên repo lớn.

### 6. ❌ `thoth-graph` thiếu persistence cho temporal edges

**Vấn đề**: KG tools (`thoth_kg_add`, `thoth_kg_timeline`) hỗ trợ temporal triples (`valid_from`, `valid_to`). Nhưng `thoth-graph/src/lib.rs` (`Edge` struct) không có temporal fields. Temporal logic nằm ở đâu?

**Khuyến nghị**: Verify temporal KG implementation. Nếu nằm ở `thoth-mcp` server layer thay vì `thoth-graph`, cần document rõ hoặc move xuống đúng layer.

### 7. ❌ `thoth-memory` quá lớn (2276 dòng lib.rs)

**Vấn đề**: `lib.rs` của `thoth-memory` chứa 10+ modules nhưng bản thân file cũng rất dài. Crate này đảm nhiệm quá nhiều concerns: promotion, reflection, rules, workflows, working memory, outcome harvesting.

**Khuyến nghị**: Xem xét tách thành sub-crates hoặc ít nhất đảm bảo mỗi module là self-contained. `rules` + `workflow` + `override` có thể thành `thoth-policy` riêng.

### 8. ❌ Không có migration strategy rõ ràng

**Vấn đề**: CLI có `migrate` command nhưng không rõ schema versioning strategy. Khi `redb`, `tantivy`, hay `rusqlite` schema thay đổi, user upgrade thế nào?

**Khuyến nghị**: Thêm schema version table trong SQLite. Check version on `CodeMemory::open()`. Auto-migrate hoặc error rõ ràng.

---

## Sơ đồ On-Disk Layout

```
.thoth/
├── config.toml              ← User config (TOML, multi-table)
├── MEMORY.md                ← Facts (markdown, git-tracked)
├── LESSONS.md               ← Lessons (markdown, git-tracked)
├── USER.md                  ← User preferences
├── skills/<slug>/SKILL.md   ← Reusable skills
├── graph.redb               ← Symbol graph (redb KV)
├── fts.tantivy/             ← BM25 index (tantivy)
├── episodes.db              ← Episodic log (SQLite + FTS5)
├── archive_sessions.db      ← Archive tracker (SQLite) [MỚI]
├── chroma/                  ← ChromaDB vectors [MỚI]
└── workflow/*.json           ← Workflow state
```

---

## Retrieval Pipeline (Chi tiết)

```
Query(text, scope, mode)
  │
  ├─► Symbol lookup ──────────► KvStore (redb)
  │
  ├─► BM25 search ────────────► FtsIndex (tantivy)
  │
  ├─► Graph BFS ───────────────► Graph (redb edges)
  │
  ├─► Markdown search ────────► MarkdownStore (MEMORY.md, LESSONS.md)
  │
  └─► [Mode::Full] Vector ────► ChromaStore (sidecar) [MỚI]
  │
  ▼
  RRF (Reciprocal Rank Fusion) ← merge all results
  │
  ├─► [Mode::Full] Synthesizer::synthesize() → LLM summary
  │
  ▼
  Retrieval { chunks, citations, synthesis? }
```

---

## Key Findings

1. **Kiến trúc layered tốt** — tách concerns rõ ràng, dependency graph sạch (không circular).
2. **Mode::Zero là USP** — retrieval hoàn toàn lexical, deterministic, không cần LLM hay embedding. Đây là điểm mạnh cạnh tranh.
3. **Pivot thành công**: Xóa `thoth-embed` (in-process ONNX) → ChromaDB sidecar. Giảm ~2GB RSS, đơn giản hóa build.
4. **Bottleneck**: `thoth-cli/main.rs` (2991 dòng) và `thoth-memory/lib.rs` (2276 dòng) là hai file cần refactor sớm nhất.
5. **`thoth-domain` đang orphan** — ingest data nhưng không tham gia retrieval. Cần wire vào pipeline hoặc consolidate.

---

## Risks & Concerns

| Risk | Severity | Mitigation |
|------|----------|------------|
| ChromaDB sidecar crash → silent vector search failure | **High** | Health check + restart logic |
| `thoth-memory` quá lớn → merge conflicts, khó maintain | **Medium** | Tách thành sub-crates |
| Không có CI → regression khi feature flags change | **Medium** | GitHub Actions workflow |
| Schema migration không versioned → data loss khi upgrade | **Medium** | Version table + auto-migrate |
| Docs bị xóa → onboarding khó | **Low** | Tạo ARCHITECTURE.md tối thiểu |
| `thoth-domain` orphan → dead code accumulation | **Low** | Integrate hoặc remove |

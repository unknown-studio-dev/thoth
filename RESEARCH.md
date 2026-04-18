# Research: Hiệu quả của project Thoth — phân tích & khắc phục

## Tech Stack Detected

- **Ngôn ngữ:** Rust, edition 2024, MSRV khai báo 1.91 (comment nói 1.89 — stale)
- **Workspace:** 12 crates, ~28,100 LOC, resolver `"3"` (đúng cho MSRV-aware resolution)
- **Runtime:** tokio multi-thread (macros, rt-multi-thread, fs, sync, time, io-util)
- **Storage:** redb 4 (KV graph), tantivy 0.26 (BM25 FTS), rusqlite 0.39 bundled (episodes + vector), LanceDB 0.10 (optional via `lance` feature)
- **Parsing:** tree-sitter 0.23 (Rust/Python/JS/TS default, Go optional)
- **HTTP:** reqwest 0.12 rustls-tls (no OpenSSL) — optional ở mọi adapter crate
- **Interface:** MCP stdio + Unix socket server (agent-facing), CLI binary (user-facing), `thoth-gate` PreToolUse hook

## Project Structure

Dependency graph là DAG sạch, không circular:

```
thoth-core  (foundation, no internal deps)
  ├─ thoth-parse, thoth-store, thoth-embed, thoth-synth, thoth-domain
  ├─ thoth-graph   ← core, store
  ├─ thoth-memory  ← core, store
  ├─ thoth-retrieve ← core, parse, store, graph
  ├─ thoth (umbrella) ← core, parse, store, graph, memory, retrieve
  ├─ thoth-mcp     ← core, parse, graph, store, retrieve, memory
  └─ thoth-cli     ← gần như mọi crate
```

### Relevant Files (các file lớn nhất / điểm nóng)

| File | LOC | Vai trò |
|------|-----|---------|
| `crates/thoth-cli/src/main.rs` | 2,508 | clap tree + mọi command handler |
| `crates/thoth-mcp/src/server.rs` | 2,246 | MCP tool dispatcher (recall/impact/context/...) |
| `crates/thoth-cli/src/hooks.rs` | 2,178 | Claude Code hooks install/exec |
| `crates/thoth-mcp/src/bin/gate.rs` | 1,811 | `thoth-gate` PreToolUse hook binary |
| `crates/thoth-store/src/markdown.rs` | 1,121 | MEMORY.md / LESSONS.md reader |
| `crates/thoth-parse/src/language.rs` | 980 | tree-sitter grammar wiring |
| `crates/thoth-retrieve/src/retriever.rs` | 926 | Retriever — parallel stages + RRF |
| `crates/thoth-memory/src/lib.rs` | 938 | MemoryManager, forget/nudge loop |
| `crates/thoth-graph/src/lib.rs` | 485 | **BFS/DFS graph engine — 0 tests** |

## Patterns & Conventions

- **Error handling:** two-tier typed — library crates trả `thoth_core::Result` (thiserror + `Other(#[from] anyhow::Error)` escape hatch). Binary crates (`thoth-cli`, `thoth-mcp`) dùng `anyhow::Result`. Boundary giữa hai tầng không explicit ở một số internal method.
- **Async:** 50 `spawn_blocking` wrap đúng mọi blocking I/O (redb/SQLite/tantivy). Các `_sync` helper trong `reflection.rs`/`memory/lib.rs` có doc comment rõ "for callers without tokio runtime (thoth-gate)".
- **Naming:** crate-per-concern (`thoth-<layer>`), snake_case module, `*_sync` suffix cho sync twins.
- **Logging:** `tracing` + `tracing-subscriber` với env-filter. Không tự log có vẻ.
- **Feature gating:** mọi HTTP adapter (`voyage`/`openai`/`cohere`/`anthropic`/`notion`/`asana`) dùng `dep:reqwest` optional; `lance` feature off-by-default.

## Test Patterns

- **Framework:** built-in `#[test]` + criterion (benches/recall.rs)
- **Location:** inline `#[cfg(test)] mod tests` + `tests/*.rs` integration files
- **Total:** 225 test functions
- **Gap nghiêm trọng:** `thoth-graph` (485 LOC graph engine) và `thoth-embed` (3 HTTP adapter) có **0 tests**
- **Mocking:** Không thấy mock HTTP server ở embed/synth crates
- **CI không bật `--all-features`** → feature-gated code (lance/anthropic/notion/asana) không được compile trong CI

## Relevant Dependencies

| Package | Version | Dùng cho | Ghi chú |
|---------|---------|----------|---------|
| redb | 4.0 | KV graph store | bundled, ổn định |
| tantivy | 0.26 | BM25 full-text index | **bị compile 2 lần (0.22 + 0.26) khi `lance` on** — từ lance-index |
| rusqlite | 0.39 (bundled) | episodes + vector | bundled SQLite → build.rs nặng, ~3-5s cold build |
| lancedb | 0.10 (optional) | Vector search M6+ | Kéo theo ~75 crate (lance + arrow + datafusion + aws-sdk) |
| tree-sitter | 0.23 | AST chunking | 5 grammar crate, mỗi cái có C build step |
| reqwest | 0.12 rustls | HTTP adapters | optional, default-features=false |
| tokio | 1.43 | Async runtime | chuẩn |
| tokio-io-util | (feature) | Async file I/O | chuẩn |
| **Lockfile total** | 653 packages | | ~75 trong số này chỉ compile khi `lance` on |

**Duplicates (từ `lance` tree):** `hashbrown` 5 versions, `itertools` 4, `thiserror` 1+2, `syn` 1+2, `rand` 0.8+0.9, `rustls` 0.21+0.23 — mostly do lance 0.10 pin cũ.

## External Research

### 1. Rust workspace dependency bloat
- **Source:** [Cargo resolver docs](https://doc.rust-lang.org/cargo/reference/resolver.html), [cargo-dedupe](https://crates.io/crates/cargo-dedupe), [cargo-machete](https://github.com/bnjbvr/cargo-machete)
- **Findings:** Cargo không auto-dedupe khi version requirements không compatible ở mức semver (0.22 vs 0.26 — không thể unify). `[patch.crates-io]` ở workspace root là cách force unify; resolver v3 (đã dùng) giúp nhưng không giải quyết semver splits.
- **Relevance:** Chờ lancedb 0.11+ bump tantivy, hoặc `[patch.crates-io]` nếu thực sự cần giảm compile time cho users bật `lance`. Thêm `cargo-machete` vào CI để phát hiện unused deps.

### 2. CI strategy for feature flags
- **Source:** [cargo-hack README](https://github.com/taiki-e/cargo-hack), [Cargo CI guide](https://doc.rust-lang.org/cargo/guide/continuous-integration.html)
- **Findings:** `cargo-hack --each-feature --no-dev-deps` cho ~90% coverage mà không cần full powerset. `--rust-version` validate against `rust-version` field.
- **Relevance:** Thêm 1 CI job `cargo hack check --each-feature` + 1 job `cargo test --all-features`. Skip powerset — 12 crate × powerset là bùng nổ.

### 3. Rust MSRV enforcement
- **Source:** [Swatinem's blog](https://swatinem.de/blog/rust-toolchain/), [dtolnay/rust-toolchain](https://github.com/dtolnay/rust-toolchain), [RFC 3537](https://rust-lang.github.io/rfcs/3537-msrv-resolver.html)
- **Findings:** `rust-version` trong Cargo.toml chỉ là **khai báo**. Best practice: CI job dùng `dtolnay/rust-toolchain@1.91` (pin), chạy `cargo hack --rust-version check`. Dùng `@stable` floating → MSRV có thể vỡ im lặng.
- **Relevance:** Thoth CI hiện dùng `@stable` → thêm MSRV check job pin `1.91`.

### 4. HTTP provider abstraction patterns
- **Source:** [async-openai docs](https://docs.rs/async-openai), [Rig providers](https://docs.rs/rig-core/latest/rig/providers/index.html)
- **Findings:** Trait-per-capability (`CompletionModel`, `EmbeddingModel`) + generic `Client<C: Config>` đánh bại macro-based codegen về maintainability. Shared HTTP plumbing (retry, auth header, reqwest client) trong `BaseHttpClient` — provider **compose** thay vì copy-paste.
- **Relevance:** Thay vì duplicate 280 dòng × 3 adapter, tạo `struct HttpEmbedderBase { client, api_key, base_url }` với method `post_json::<Req, Resp>(&self, path, body)`. Mỗi provider chỉ ~20 LOC.

### 5. Embedded vector search at scale
- **Source:** [sqlite-vec 1.0 release](https://alexgarcia.xyz/blog/2024/sqlite-vec-stable-release/), [FAISS vs LanceDB](https://zilliz.com/comparison/faiss-vs-lancedb)
- **Findings:** Ở 100k vectors: `sqlite-vec` ~41ms, FAISS ~50ms, usearch ~46ms. Query latency chênh nhỏ; build time và integration cost quyết định. LanceDB thắng rõ từ ~1M+ vectors với HNSW/IVF.
- **Relevance:** Default SQLite flat cosine của Thoth là đúng cho sweet spot <1M vectors. Nếu muốn nâng, migrate sang `sqlite-vec` extension (cùng process, SIMD cosine, không thêm dep graph) trước khi đụng LanceDB. Khuyến cáo: **giữ SQLite làm default, consider sqlite-vec cho Mode::Full**.

### 6. Graph traversal N+1 on embedded KV
- **Source:** [redb README](https://github.com/cberner/redb), [MS-BFS paper VLDB 2015](https://db.in.tum.de/~kaufmann/papers/msbfs.pdf)
- **Findings:** redb không có multi-get nhưng range scan trong 1 read transaction là snapshot-isolated, không có per-call overhead. Pattern: **mở 1 read transaction, dùng adjacency key `(src_id, edge_kind, dst_id)` + range scan**, push vào next frontier.
- **Relevance:** `thoth-graph/src/lib.rs` BFS gọi 1 lookup per edge ở mỗi level. Fix: refactor `neighbors()` / `out_neighbors()` để mỗi BFS level là một range scan, open read txn 1 lần per query. Với depth=8 trên 50-node subgraph, có thể cut ~50x DB calls.

## Key Findings

**Thoth có kiến trúc mạnh nhưng reliability có lỗ hổng rõ ràng:**

1. **Kiến trúc (tốt):** DAG không cycle, crate separation clean, feature gating đúng, async discipline ổn (50 spawn_blocking đúng chỗ), 0 `todo!()`/`FIXME` trong production.

2. **Test coverage (vấn đề lớn):** 2 crate quan trọng `thoth-graph` và `thoth-embed` có 0 tests. Graph engine là backbone của `thoth_impact` / `thoth_detect_changes` — bug ở đây gây sai blast radius cho mọi agent dùng Thoth.

3. **CI gap:** Không bật `--all-features` → code feature-gated (lance/anthropic/notion/asana) bit-rots. Không pin MSRV → future breakage silently.

4. **Perf hotspot xác định:** `thoth-graph` BFS N+1 (1 DB lookup per edge), `retriever.rs` clone `Candidate` trong RRF fuse, `symbols_with_prefix` sequential per token.

5. **238 `unwrap()` trong production code** — hotspot nguy hiểm: `thoth-store/fts.rs:93-99` (7 unwrap liên tiếp trên schema lookup), `thoth-cli/hooks.rs` ~20 unwrap trên embedded JSON traversal.

6. **Code duplication hợp lý hoá được:** 3 embed provider duplicate ~280 dòng × 3 (đã có pattern rõ ràng trong Rust ecosystem — trait + BaseHttpClient).

## Risks & Concerns

| Risk | Severity | Notes |
|------|----------|-------|
| `thoth-graph` untested → sai impact analysis | High | Backbone của agent-facing tools; silent bug lan rộng |
| `thoth-embed` untested → embedding pipeline fail silently | High | Mode::Full không usable nếu provider API thay đổi |
| Feature-gated code bit-rots | High | Không ai biết cho đến khi user bật feature |
| FTS unwrap panic trên schema mismatch | Medium | Panic khi tantivy schema bump version |
| Graph N+1 ở depth=8 | Medium | MCP tool timeout với large codebase |
| MSRV không enforced | Medium | Có thể vỡ build khi `@stable` bump cao hơn 1.91 |
| Embed provider duplication | Low | Maintenance cost, không phải correctness issue |
| tantivy compile 2 lần với `lance` | Low | Chỉ ảnh hưởng users bật `lance` |

## Action Items — Priority Order

| # | Action | Effort | Impact | File liên quan |
|---|--------|--------|--------|----------------|
| 1 | Viết unit tests cho `thoth-graph` BFS/DFS/impact | 2-3h | Cao | `crates/thoth-graph/tests/` |
| 2 | Viết mock-HTTP tests cho `thoth-embed` providers | 2-3h | Cao | `crates/thoth-embed/tests/` |
| 3 | Thêm CI job `cargo hack check --each-feature` | 30min | Cao | `.github/workflows/ci.yml` |
| 4 | Audit + fix critical unwrap (fts.rs:93-99, hooks.rs JSON) | 1-2h | Cao | `thoth-store/src/fts.rs`, `thoth-cli/src/hooks.rs` |
| 5 | Refactor graph BFS: 1 read txn + range scan per level | 3-4h | Trung bình | `crates/thoth-graph/src/lib.rs` |
| 6 | Extract `HttpEmbedderBase` cho 3 provider | 2h | Trung bình | `crates/thoth-embed/src/*.rs` |
| 7 | Define `VectorBackend` trait cho SQLite + Lance | 2h | Trung bình | `crates/thoth-store/src/vector*.rs` |
| 8 | CI MSRV job pin `dtolnay/rust-toolchain@1.91` | 15min | Trung bình | `.github/workflows/ci.yml` |
| 9 | Profile dev builds với `[profile.dev.package.*] opt-level = 1` cho libsqlite3-sys + tantivy | 10min | Thấp | `Cargo.toml` |
| 10 | Fix MSRV comment stale ("1.89" → "1.91") | 2min | Thấp | `Cargo.toml:3` |

**Total critical work (items 1-4):** ~6-9h → project từ "good architecture, risky execution" lên "production-ready reliability".

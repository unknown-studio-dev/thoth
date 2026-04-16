<p align="center">
  <img src="thoth.png" alt="Thoth" width="200" />
</p>

<h1 align="center">Thoth</h1>

<p align="center"><em>"Thoth — thần ghi chép, người lưu giữ tri thức."</em></p>

<p align="center">Bộ nhớ dài hạn cho claude coding agent. Nhúng trực tiếp, viết bằng Rust.</p>

<p align="center"><a href="./README.md">🇬🇧 English</a> · <strong>🇻🇳 Tiếng Việt</strong></p>

<p align="center">
  <a href="https://github.com/unknown-studio-dev/thoth/actions/workflows/ci.yml"><img src="https://github.com/unknown-studio-dev/thoth/actions/workflows/ci.yml/badge.svg" alt="ci" /></a>
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/Rust-1.91%2B-orange?logo=rust" alt="Rust" /></a>
  <a href="https://github.com/unknown-studio-dev/thoth/releases"><img src="https://img.shields.io/badge/version-0.0.1--alpha-blue" alt="version" /></a>
  <a href="./LICENSE-MIT"><img src="https://img.shields.io/badge/license-MIT%2FApache--2.0-green" alt="license" /></a>
</p>

> [!WARNING]
> **Đang phát triển — chưa sẵn sàng cho production.** Thoth đang trong giai đoạn phát triển tích cực (`0.0.1-alpha`). API, định dạng lưu trữ trên đĩa và các flag CLI có thể thay đổi bất cứ lúc nào mà không báo trước. Sẽ có bug, breaking changes và nhiều tính năng chưa hoàn thiện. Dùng với rủi ro của riêng bạn; **không** nên dùng cho hệ thống production.

---

## Tổng quan

Thoth là một thư viện Rust (kèm CLI, MCP server và plugin cho Claude Code / Cowork) giúp coding agent có **bộ nhớ dài hạn, có tổ chức** cho một codebase. Hệ thống gồm hai lớp:

1. **Engine** — ba binary: `thoth`, `thoth-mcp`, `thoth-gate`.
2. **Plugin** — `thoth-discipline` (hooks + skills + MCP wiring) — biến session Claude Code / Cowork thành một vòng lặp *thực sự sử dụng bộ nhớ ở mỗi lượt*.

Một store duy nhất, năm loại bộ nhớ:

- **Semantic** — toàn bộ symbol, call, import, reference được phân tích bằng tree-sitter.
- **Episodic** — log toàn bộ query / answer / outcome bằng FTS5.
- **Procedural** — các skill tái sử dụng, lưu dưới dạng folder tương thích `agentskills.io`.
- **Reflective** — các bài học rút ra từ sai lầm, có confidence score trong `LESSONS.md`, tự động bị “cách ly” (quarantine) nếu gây hại nhiều hơn lợi.
- **Domain** — business rule, invariant, workflow và glossary được sync về từ Notion / Asana / NotebookLM / file cục bộ, snapshot dưới dạng markdown trong `domain/<context>/` để review qua git. Trả lời được câu hỏi *"vì sao chỗ này giới hạn refund $500?"* — đây là khoảng trống giữa *hiểu code* và *hiểu codebase*. Xem [ADR 0001](./docs/adr/0001-domain-memory.md).

Hai chế độ hoạt động:

- **`Mode::Zero`** — chạy hoàn toàn offline, deterministic. Không dùng LLM, không embedding API. Truy vấn bằng symbol search, graph traversal, BM25 (tantivy), kết hợp RRF.
- **`Mode::Full`** — bổ sung `Embedder` (Voyage / OpenAI / Cohere) và/hoặc `Synthesizer` (Anthropic Claude) để vector search và cho phép LLM tự tinh lọc bộ nhớ (flow "nudge"). Vector backend dùng SQLite (flat cosine index), không cần thêm hạ tầng.

## Cài đặt

Thoth cung cấp ba binary: `thoth` (CLI), `thoth-mcp` (MCP server), `thoth-gate` (hook strict mode). Dùng kênh nào cũng được — tất cả cùng thao tác trên một bộ file.

### Homebrew (macOS + Linux)

```bash
brew tap unknown-studio-dev/thoth
brew install thoth
````

### npm

```bash
npm install -g @unknownstudio/thoth-cc
# hoặc:
npx @unknownstudio/thoth-cc setup
```

Package `@unknownstudio/thoth-cc` publish kèm 4 subpackage theo platform
(`@unknownstudio/thoth-cc-{darwin-arm64,darwin-x64,linux-arm64,linux-x64}`);
`optionalDependencies` sẽ tự chọn đúng binary.

### Build từ source

```bash
cargo install --git https://github.com/unknown-studio-dev/thoth thoth-cli thoth-mcp
```

### Kiểm tra

```bash
thoth --version
thoth-mcp --version
thoth-gate < /dev/null    # phải in {"decision":"approve",...}
```

## Chạy trong 30 giây

```bash
cd your-project
thoth setup              # wizard tương tác → .thoth/config.toml
thoth index .            # build code index
thoth install            # gắn hook + skill + MCP cho Claude Code
```

`thoth setup` sẽ hỏi các cấu hình quan trọng — enforcement mode, memory mode, gate window — rồi sinh file `config.toml` có comment để bạn chỉnh sau.

* `--show`: in config hiện tại
* `--accept-defaults`: bootstrap không cần tương tác (dùng trong CI)

Plugin Cowork / Claude Code (`thoth-discipline`) là thành phần biến binary thành vòng lặp recall *bắt buộc*:

* Tải [`thoth-discipline-x.y.z.plugin`](https://github.com/unknown-studio-dev/thoth/releases)
* Cài qua Cowork plugin picker hoặc `claude plugin install`
* Xem chi tiết: [`plugins/thoth-discipline/README.md`](./plugins/thoth-discipline/README.md)

⚠️ **Cài plugin thôi là chưa đủ.** Hook sẽ gọi `thoth-gate`, MCP sẽ chạy `thoth-mcp` — cần cài binary trước.

## Cấu hình

`thoth setup` sẽ generate toàn bộ. Nếu chỉnh tay, file `<root>/config.toml`:

```toml
[memory]
episodic_ttl_days = 30
enable_nudge      = true

[discipline]
mode                      = "soft"       # "soft" | "strict"
global_fallback           = true
reflect_cadence           = "end"        # "end" | "every"
nudge_before_write        = true
grounding_check           = false
gate_window_secs          = 180

# v2 knobs
memory_mode               = "auto"       # "auto" | "review"
gate_require_nudge        = false
quarantine_failure_ratio  = 0.66
quarantine_min_attempts   = 5
```

| Kịch bản          | `mode`   | `gate_require_nudge` | `memory_mode` |
| ----------------- | -------- | -------------------- | ------------- |
| Solo, ít friction | `soft`   | `false`              | `auto`        |
| Solo, an toàn hơn | `strict` | `false`              | `auto`        |
| Team, thử nghiệm  | `strict` | `true`               | `review`      |
| Team, production  | `strict` | `true`               | `auto`        |

## Kiến trúc

```
  ┌── Cowork / Claude Code ────────────────────────────────────────────┐
  │                                                                    │
  │   plugin thoth-discipline                                          │
  │   ├── hooks/hooks.json      SessionStart / PreToolUse / Stop       │
  │   ├── skills/               memory-discipline + thoth-reflect      │
  │   └── .mcp.json             khởi chạy `thoth-mcp`                  │
  │          │                                                         │
  │          ▼                                                         │
  │   thoth-gate  ─ đọc SQLite (read-only) ─► episodes.db              │
  │   (hook PreToolUse, block nếu thiếu recall / nudge)                │
  │                                                                    │
  └────────────────────────┬───────────────────────────────────────────┘
                           │ JSON-RPC / stdio
                           ▼
  ┌── thoth-mcp ───────────────────────────────────────────────────────┐
  │   tools    thoth_recall, thoth_remember_*, thoth_memory_*,         │
  │            thoth_request_review, thoth_skill_propose, …            │
  │   prompts  thoth.nudge  (ghi event NudgeInvoked)                   │
  │            thoth.reflect                                           │
  │   resources thoth://memory/MEMORY.md, thoth://memory/LESSONS.md    │
  └────────────────────────┬───────────────────────────────────────────┘
                           │
                           ▼
  ┌── `.thoth/` store ─────────────────────────────────────────────────┐
  │   episodes.db           event log                                  │
  │   graph.redb            symbol / import / call graph               │
  │   fts.tantivy/          BM25 index                                 │
  │   vectors.db            vector index (Mode::Full)                  │
  │   MEMORY.md             facts                                      │
  │   LESSONS.md            lessons                                    │
  │   LESSONS.quarantined.md  lessons bị loại                          │
  │   MEMORY.pending.md, LESSONS.pending.md  staged (review mode)      │
  │   memory-history.jsonl  audit trail                                │
  │   domain/<ctx>/DOMAIN.md        business rule đã accepted          │
  │   domain/<ctx>/_remote/<src>/*  snapshot ingestor ghi (proposed)   │
  │   skills/               procedural skills                          │
  └────────────────────────────────────────────────────────────────────┘
```

Ba lớp enforcement (tăng dần độ khó bypass):

1. **Prompt + skill** — inject lessons vào context, skill điều hướng flow recall → nudge → act → reflect
2. **Hook prompt** — reminder ngắn ở Pre/PostToolUse
3. **`thoth-gate` (strict mode)** — binary native chặn trực tiếp tool call nếu thiếu recall/nudge

`thoth-gate` **fail-open** khi lỗi (DB/config lỗi sẽ không block) — ưu tiên không làm gián đoạn editor.

## CLI cheatsheet

```bash
# lifecycle
thoth setup
thoth setup --show
thoth init
thoth index .
thoth watch .
thoth query "how does the nudge flow work"

# memory
thoth memory show
thoth memory fact "Auth tokens expire after 15m" --tags auth,jwt
thoth memory lesson --when "touching db/migrations" "run make db-check"
thoth memory pending
thoth memory promote lesson 0
thoth memory reject  fact   2 --reason "trùng"
thoth memory log --limit 50
thoth memory forget
thoth --synth anthropic memory nudge

# domain (business-rule memory — cần bật đúng cargo feature)
thoth domain sync --source file    --from ./specs/             # offline / test
thoth domain sync --source notion  --from <database-id>        # cần NOTION_TOKEN
thoth domain sync --source asana   --project-id <gid>          # cần ASANA_TOKEN
thoth domain sync --source notebooklm                          # stub; export → file

# Claude Code
thoth install
thoth install --scope user
thoth uninstall

# eval
thoth eval --gold eval/gold.toml -k 8
```

## MCP server

`thoth-mcp` dùng JSON-RPC 2.0 qua stdio (MCP `2024-11-05`).

| Tool                    | Mô tả                        |
| ----------------------- | ---------------------------- |
| `thoth_recall`          | Hybrid recall (Mode::Zero)   |
| `thoth_index`           | Parse + index                |
| `thoth_remember_fact`   | Lưu fact                     |
| `thoth_remember_lesson` | Lưu lesson (không overwrite) |
| `thoth_memory_show`     | Đọc memory                   |
| `thoth_memory_pending`  | Danh sách pending            |
| `thoth_memory_promote`  | Approve                      |
| `thoth_memory_reject`   | Reject                       |
| `thoth_memory_history`  | Audit log                    |
| `thoth_memory_forget`   | TTL + cleanup                |
| `thoth_lesson_outcome`  | Track success/failure        |
| `thoth_request_review`  | Yêu cầu review               |
| `thoth_skill_propose`   | Đề xuất skill                |
| `thoth_skills_list`     | List skill                   |

## Domain memory (business rule)

Loại bộ nhớ thứ sáu của Thoth (xem [ADR 0001](./docs/adr/0001-domain-memory.md))
bắt *cái "vì sao"* — business rule, invariant, workflow, glossary — những
thứ nằm ngoài AST. Code path tách biệt khỏi memory còn lại, có chủ đích:

- **Chỉ sync khi gõ lệnh.** `thoth domain sync` pull từ remote đã chọn.
  `recall()` không bao giờ đi mạng — Mode::Zero vẫn deterministic.
- **Snapshot-based.** Mỗi rule thành một file markdown riêng với TOML
  frontmatter (`id`, `source`, `source_hash`, `context`, `kind`,
  `last_synced`, `status`). `source_hash` (blake3) khiến re-sync khi
  upstream không đổi là no-op.
- **Suggest-only merge.** Ingestor ghi vào `## Proposed`. Con người
  (hoặc CODEOWNERS) mới được promote lên `## Accepted` qua PR. Retrieval
  xếp `Accepted` lên đầu.
- **Redact trước tiên.** JWT, provider token (`sk-`, `xoxb-`, `ghp_`, …),
  thẻ 16 số, AWS access key bị scan trước khi ghi đĩa; trúng pattern là
  drop cả rule và đếm vào counter `redacted`.

Cargo feature trong `thoth-cli` (tất cả opt-in, mặc định không bật):

```bash
cargo install --git https://github.com/unknown-studio-dev/thoth \
  thoth-cli --features "notion,asana,notebooklm"
# hoặc: thoth-cli --features full   (bật tất cả)
```

Adapter:

| Adapter | Feature | Auth | Ghi chú |
|---|---|---|---|
| `file` | luôn bật | — | đọc `*.toml` trong thư mục; dùng cho air-gapped và test |
| `notion` | `notion` | `NOTION_TOKEN` | query một database; route theo property `Thoth.Context` |
| `asana` | `asana` | `ASANA_TOKEN` | query một project; route theo custom field `Thoth.Context` |
| `notebooklm` | `notebooklm` | — | stub, chờ MCP; tạm export → dùng adapter `file` |

Route rule về bounded context bằng cách set property / custom field
`Thoth.Context` phía nguồn; rule không có context sẽ bị drop (counter
`unmapped`). Đây là cam kết của ADR 0001: **PM chủ động opt record nào
vào Thoth**, không auto-ingest hết.

## Release flow

* Tag `vX.Y.Z`
* Build multi-platform binary
* Upload artifact + checksum + plugin
* Update Homebrew + npm package

## Dùng như library

Xem phần *Embedding as a library* trong README tiếng Anh.

## Đóng góp

Chào đón mọi đóng góp: bug, feature, translation, PR.
Xem `CONTRIBUTING.vi.md`.

## Trạng thái

**Alpha.** Core design đã ổn định. M1–M6 hoàn thành. **M7 — Domain
memory** (crate `thoth-domain` với adapter file / Notion / Asana /
NotebookLM và lệnh `thoth domain sync`) đã landed trong 0.0.1-alpha;
MCP-universal ingestor vẫn nằm trong roadmap.

## License

Dual license: Apache 2.0 hoặc MIT.


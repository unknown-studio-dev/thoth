<p align="center">
  <img src="thoth.png" alt="Thoth" width="200" />
</p>

<h1 align="center">Thoth</h1>
<p align="center"><em>"Thoth — thần ghi chép, người canh giữ tri thức."</em></p>

<p align="center">Bộ nhớ dài hạn cho coding agent. Nhúng trực tiếp, viết bằng Rust, hiểu code.</p>

<p align="center"><a href="./README.md">🇬🇧 English</a> · <strong>🇻🇳 Tiếng Việt</strong></p>

<p align="center">
  <a href="https://github.com/unknown-studio-dev/thoth/actions/workflows/ci.yml"><img src="https://github.com/unknown-studio-dev/thoth/actions/workflows/ci.yml/badge.svg" alt="ci" /></a>
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/Rust-1.91%2B-orange?logo=rust" alt="Rust" /></a>
  <a href="https://github.com/unknown-studio-dev/thoth/releases"><img src="https://img.shields.io/badge/version-0.0.1--alpha-blue" alt="version" /></a>
  <a href="./LICENSE-MIT"><img src="https://img.shields.io/badge/license-MIT%2FApache--2.0-green" alt="license" /></a>
</p>

---

## Tổng quan

Thoth là một thư viện Rust (kèm CLI, MCP server, và plugin Claude Code /
Cowork) cho phép coding agent có bộ nhớ *lâu dài*, có *kỷ luật*, về một
codebase. Dự án gồm hai tầng:

1. **Engine** — ba binary `thoth`, `thoth-mcp`, `thoth-gate`.
2. **Plugin** — `thoth-discipline` (hooks + skills + MCP wiring) — biến
   session Claude Code / Cowork thành vòng lặp *thực sự sử dụng* bộ nhớ
   trên mỗi lượt.

Bốn loại bộ nhớ, một store duy nhất:

- **Semantic** — mọi symbol, call, import, reference được tree-sitter phân tích.
- **Episodic** — mọi query / answer / outcome được ghi vào FTS5 log.
- **Procedural** — skill tái sử dụng, lưu dạng folder tương thích `agentskills.io`.
- **Reflective** — bài học rút ra từ sai lầm, có confidence score trong
  `LESSONS.md`, tự động bị "cách ly" (quarantine) khi gây hại nhiều hơn lợi.

Hai chế độ hoạt động:

- **`Mode::Zero`** — hoàn toàn offline, deterministic. Không LLM, không
  embedding API. Tìm symbol, duyệt graph, BM25 qua tantivy, RRF fusion.
- **`Mode::Full`** — gắn thêm `Embedder` (Voyage / OpenAI / Cohere) và/hoặc
  `Synthesizer` (Anthropic Claude) để vector search và để LLM tự chắt lọc
  bộ nhớ (flow "nudge"). Vector backend là SQLite flat cosine index — không
  cần hạ tầng gì thêm.

## Cài đặt

Thoth phát hành ba binary: `thoth` (CLI), `thoth-mcp` (MCP server),
`thoth-gate` (hook strict mode). Chọn kênh nào cũng được — cả ba cùng
trả về một bộ file.

### Homebrew (macOS + Linux)

```bash
brew tap unknown-studio-dev/thoth
brew install thoth
```

### npm

```bash
npm install -g thoth-memory
# hoặc:
npx thoth-memory setup
```

npm publish `thoth-memory` cùng 4 subpackage theo platform
(`thoth-memory-{darwin-arm64,darwin-x64,linux-arm64,linux-x64}`);
`optionalDependencies` sẽ khiến npm tự chọn đúng binary.

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

`thoth setup` hỏi những knob quan trọng — enforcement mode, memory mode,
gate window — rồi ghi file `config.toml` có comment để bạn tinh chỉnh
sau. Thêm `--show` để in config hiện tại, `--accept-defaults` để bootstrap
không tương tác (ví dụ trong CI).

Plugin Cowork / Claude Code (`thoth-discipline`) là phần biến binary
thành vòng lặp recall *được ép buộc*:

- Tải [`thoth-discipline-x.y.z.plugin`](https://github.com/unknown-studio-dev/thoth/releases)
  từ GitHub Release khớp version binary của bạn.
- Cài qua Cowork plugin picker, hoặc `claude plugin install` với Claude Code.
- Chi tiết: [`plugins/thoth-discipline/README.md`](./plugins/thoth-discipline/README.md).

⚠️ **Cài plugin KHÔNG đủ.** Hook gọi `thoth-gate`, MCP entry khởi chạy
`thoth-mcp` — cài binary trước đã.

## Cấu hình

`thoth setup` ghi toàn bộ; nếu muốn sửa tay, `<root>/config.toml` trông như:

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

| Kịch bản              | `mode`   | `gate_require_nudge` | `memory_mode` |
|-----------------------|----------|----------------------|---------------|
| Solo, ít ma sát       | `soft`   | `false`              | `auto`        |
| Solo, cẩn thận        | `strict` | `false`              | `auto`        |
| Team, thử nghiệm      | `strict` | `true`               | `review`      |
| Team, hậu v1          | `strict` | `true`               | `auto`        |

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
  │   thoth-gate  ─ đọc SQLite (chỉ đọc) ─► episodes.db                │
  │   (command hook ở PreToolUse, block nếu thiếu recall / nudge)      │
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
  │   episodes.db           event log (query_issued, nudge_invoked…)   │
  │   graph.redb            symbol / import / call graph               │
  │   fts.tantivy/          BM25 index                                 │
  │   vectors.db            flat cosine vector index (Mode::Full)      │
  │   MEMORY.md             fact khai báo                              │
  │   LESSONS.md            bài học đang áp dụng                       │
  │   LESSONS.quarantined.md  bài học bị tự động hạ cấp                │
  │   MEMORY.pending.md, LESSONS.pending.md  staged trong chế độ review│
  │   memory-history.jsonl  audit trail có phiên bản                   │
  │   skills/               procedural skills                          │
  └────────────────────────────────────────────────────────────────────┘
```

Ba tầng ép buộc, xếp theo độ khó bypass tăng dần:

1. **Prompt + skill** — SessionStart hook dump lessons vào context;
   skill `memory-discipline` dẫn agent qua vòng recall/nudge/act/reflect.
2. **Hook prompt** — PreToolUse/PostToolUse hook đẩy reminder ngắn,
   khó bỏ qua nhưng bản chất vẫn là text.
3. **`thoth-gate`** (strict mode) — binary native chạy trên mỗi
   `Write` / `Edit` / `Bash` PreToolUse. Nó query `episodes.db` trực tiếp
   tìm event `query_issued` (và optionally `nudge_invoked`) còn mới, rồi
   **chặn** tool call nếu thiếu. Model không thể "tự thuyết phục" qua
   kết luận `{"decision":"block"}`.

`thoth-gate` **fail open** khi gặp lỗi (thiếu DB, không đọc được config)
— gate hỏng không bao giờ làm đơ editor, đổi lại là âm thầm hạ về soft
mode. Nếu strict có vẻ yếu, xem stderr trước.

## CLI cheatsheet

```bash
# lifecycle
thoth setup                               # wizard config tương tác
thoth setup --show                        # in config hiện tại
thoth init                                # tạo .thoth/
thoth index .                             # parse + index
thoth watch .                             # nằm resident, reindex khi đổi
thoth query "how does the nudge flow work"

# memory
thoth memory show
thoth memory fact "Auth tokens expire after 15m" --tags auth,jwt
thoth memory lesson --when "touching db/migrations" "run make db-check"
thoth memory pending                      # queue chờ review (chế độ review)
thoth memory promote lesson 0
thoth memory reject  fact   2 --reason "trùng"
thoth memory log --limit 50               # audit trail từ memory-history.jsonl
thoth memory forget                       # TTL + quarantine pass
thoth --synth anthropic memory nudge      # LLM chắt lọc lesson

# Wire vào Claude Code
thoth install                             # skill + hook + MCP, scope project
thoth install --scope user                # global
thoth uninstall                           # gỡ trong scope đó

# eval
thoth eval --gold eval/gold.toml -k 8
```

Chạy `thoth --help` để xem toàn bộ.

## MCP server

`thoth-mcp` nói JSON-RPC 2.0 qua stdio (MCP version `2024-11-05`).
Tool công bố:

| Tool                     | Tác dụng                                                     |
|--------------------------|--------------------------------------------------------------|
| `thoth_recall`           | Mode::Zero hybrid recall                                     |
| `thoth_index`            | Walk + parse + index một path                                |
| `thoth_remember_fact`    | Append / stage một fact                                      |
| `thoth_remember_lesson`  | Append / stage một lesson (không ghi đè âm thầm)             |
| `thoth_memory_show`      | Đọc hai file markdown                                        |
| `thoth_memory_pending`   | Liệt kê entry đang stage                                     |
| `thoth_memory_promote`   | Chấp nhận entry staged                                       |
| `thoth_memory_reject`    | Bỏ entry staged kèm lý do                                    |
| `thoth_memory_history`   | Tail `memory-history.jsonl`                                  |
| `thoth_memory_forget`    | TTL + capacity eviction + auto-quarantine                    |
| `thoth_lesson_outcome`   | Tăng counter success/failure của lesson                      |
| `thoth_request_review`   | Gắn cờ cần human audit                                       |
| `thoth_skill_propose`    | Draft skill mới từ ≥5 lesson đã consolidate                  |
| `thoth_skills_list`      | Liệt kê skill đã cài                                         |

Cộng 2 resource (`thoth://memory/MEMORY.md`, `thoth://memory/LESSONS.md`)
và 2 prompt (`thoth.nudge`, `thoth.reflect`) — prompt nudge ghi event
`NudgeInvoked` để gate hai-event của strict mode có thể kiểm tra.

## Release flow

- Tag `vX.Y.Z` trên `main`.
- `.github/workflows/release.yml` build `aarch64-apple-darwin`,
  `x86_64-apple-darwin`, `aarch64-unknown-linux-gnu`,
  `x86_64-unknown-linux-gnu`, upload tarball + sha256 + plugin bundle
  vào GitHub Release.
- `packaging/homebrew/bump.sh vX.Y.Z` dập SHA mới vào formula — copy
  output vào tap's `Formula/thoth.rb` rồi push.
- `packaging/npm/publish.sh vX.Y.Z` pack lại tarball thành npm package
  và publish (có `DRY_RUN=1` tuỳ chọn).

## Dùng như library

Mode::Zero và Mode::Full xem mục ["Embedding as a library" trong bản tiếng Anh](./README.md#embedding-as-a-library)
— code Rust giống hệt, chỉ khác ngôn ngữ chú thích.

## Đóng góp

Bug report, feature request, memory-drift report, dịch, và PR đều
được chào đón. Xem [`CONTRIBUTING.vi.md`](./CONTRIBUTING.vi.md) để biết
workflow, code style, và template issue.

## Trạng thái

**Alpha.** Thiết kế chốt trong [`DESIGN.md`](./DESIGN.md). Milestone
M1–M6 (parse + store + graph + retrieve + CLI + MCP + Mode::Full +
plugin discipline) đã xong.

## License

Dual-licensed: [Apache License 2.0](./LICENSE-APACHE) hoặc
[MIT](./LICENSE-MIT), tuỳ bạn.

Trừ khi bạn nói rõ khác, mọi contribution bạn gửi lên repo được hiểu là
dual-licensed như trên, không có điều khoản thêm.

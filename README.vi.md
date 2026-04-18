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

Thoth là một thư viện Rust (kèm CLI, MCP server và bootstrap one-shot
cho Claude Code) giúp coding agent có **bộ nhớ dài hạn, có tổ chức**
cho một codebase. Ba binary lo hết:

1. **`thoth`** — CLI: setup wizard, indexer, query, eval, memory ops.
2. **`thoth-mcp`** — MCP stdio server cho Claude Code (qua `mcpServers`).
3. **`thoth-gate`** — `PreToolUse` hook ép "search trước khi write".

`thoth setup` là **lệnh duy nhất** cài hooks, đăng ký MCP, copy skills,
seed `.thoth/`. Không có plugin riêng phải cài.

Một store duy nhất, năm loại bộ nhớ:

- **Semantic** — toàn bộ symbol, call, import, reference được phân tích bằng tree-sitter.
- **Episodic** — log toàn bộ query / answer / outcome bằng FTS5.
- **Procedural** — các skill tái sử dụng, lưu dưới dạng folder tương thích `agentskills.io`.
- **Reflective** — các bài học rút ra từ sai lầm, có confidence score trong `LESSONS.md`, tự động bị “cách ly” (quarantine) nếu gây hại nhiều hơn lợi.
- **Domain** — business rule, invariant, workflow và glossary được sync về từ Notion / Asana / NotebookLM / file cục bộ, snapshot dưới dạng markdown trong `domain/<context>/` để review qua git. Trả lời được câu hỏi *"vì sao chỗ này giới hạn refund $500?"* — đây là khoảng trống giữa *hiểu code* và *hiểu codebase*. Xem [ADR 0001](./docs/adr/0001-domain-memory.md).

Hai chế độ hoạt động:

- **`Mode::Zero`** — chạy hoàn toàn offline, deterministic. Không dùng LLM, không embedding API. Truy vấn bằng symbol search, graph traversal, BM25 (tantivy), kết hợp RRF.
- **`Mode::Full`** — bổ sung `Embedder` (Voyage / OpenAI / Cohere) và/hoặc `Synthesizer` (Anthropic Claude) để vector search và cho phép LLM tự tinh lọc bộ nhớ (flow "nudge"). Vector backend mặc định dùng SQLite flat cosine (`vectors.db`), không cần thêm hạ tầng; build `--features lance` để chuyển sang LanceDB (`chunks.lance/`) cho corpus lớn — API y hệt.

## Cài đặt

**Một lệnh.** Còn lại tự lo runtime.

```bash
# Zero-config: drop thẳng vào setup wizard, xong in ra bước tiếp theo.
npx @unknownstudio/thoth
```

Lệnh duy nhất đó:

1. Tải prebuilt binary (`thoth`, `thoth-mcp`, `thoth-gate`) đúng platform
   của bạn qua npm.
2. Chạy `thoth setup` — wizard tương tác ghi `.claude/settings.json`
   (MCP + hooks), copy skills vào `.claude/skills/`, seed `.thoth/`
   với `config.toml`, `MEMORY.md`, `LESSONS.md`.
3. Báo bạn xem lại `.thoth/config.toml`, rồi chạy `thoth index .`.

Chạy lại `npx @unknownstudio/thoth` trên project đã bootstrap sẽ phát
hiện cài cũ và mời reinstall hooks, reconfigure, hoặc tự self-heal
phần thiếu.

Channel khác (cùng binary, không cần Node):

```bash
brew install unknown-studio-dev/thoth/thoth
# hoặc
cargo install --git https://github.com/unknown-studio-dev/thoth thoth-cli thoth-mcp
# rồi:
thoth setup
```

## Lần đầu dùng

Setup xong, project đã wire đủ Claude Code nhưng index còn trống.
Một lệnh nạp:

```bash
cd your-project
thoth index .            # lần đầu; sau đó incremental
```

Mở Claude Code trong project → `SessionStart` nạp `LESSONS.md` /
`MEMORY.md` → `PreToolUse(Write|Edit|Bash|NotebookEdit)` bắn
`thoth-gate` → `Stop` chạy `thoth.reflect` để persist lesson.

Các knob (`mode`, `gate_relevance_threshold`, …) ở `.thoth/config.toml`.
Chạy lại `thoth setup` bất cứ lúc nào để revisit wizard; default
chạy ngon nên skip cũng được.

### Verify

```bash
thoth --version
thoth-gate < /dev/null    # phải in {"decision":"approve",...}
# trong Claude Code:
/mcp                      # → thoth  ✓ connected
```

## Cấu hình

`thoth setup` sẽ generate toàn bộ. Nếu chỉnh tay, file `<root>/config.toml`:

```toml
[memory]
episodic_ttl_days = 30
enable_nudge      = true

[discipline]
# Công tắc chính — đặt `false` để tắt hẳn gate.
nudge_before_write       = true
# Fallback sang ~/.thoth khi project không có .thoth/.
global_fallback          = true
# `end` (chỉ khi Stop) hoặc `every` (sau mỗi tool call).
reflect_cadence          = "end"
# `auto` ghi thẳng vào MEMORY.md/LESSONS.md.
# `review` stage vào *.pending.md — user phải promote/reject.
memory_mode              = "auto"

# --- gate v2 ---------------------------------------------------------
# Verdict khi relevance miss:
#   "off"    — tắt hoàn toàn (pass yên lặng).
#   "nudge"  — pass + stderr warning.  [mặc định]
#   "strict" — block.
mode                     = "nudge"
# Recency shortcut — recall trong window này pass luôn không cần relevance.
# Giữ ngắn để chặn "recall một lần, edit mãi mãi".
gate_window_short_secs   = 60
# Pool relevance — gate nhìn lại các recall trong window này khi
# scoring topic overlap cho edit sắp tới.
gate_window_long_secs    = 1800
# Containment ratio [0.0, 1.0] — 0 tắt relevance, 0.30 cân bằng,
# 0.50 strict. Xem comment block trong config.toml generate ra.
gate_relevance_threshold = 0.30
# Append mọi quyết định vào .thoth/gate.jsonl — dùng để calibrate.
gate_telemetry_enabled   = false

# Optional: các prefix Bash luôn bypass gate (cộng thêm vào built-in
# mặc định như `cargo test`, `git status`, `grep`).
# gate_bash_readonly_prefixes = ["pnpm lint", "just check"]

# Policy theo actor. `THOTH_ACTOR` env var chọn policy; glob khớp
# đầu tiên thắng. Hữu ích khi muốn Claude Code chạy strict,
# orchestrator chạy nudge, CI chạy off — cùng một gate binary.
# [[discipline.policies]]
# actor = "hoangsa/*"                # worker của orchestrator
# mode = "nudge"
# window_short_secs = 300
# relevance_threshold = 0.20
#
# [[discipline.policies]]
# actor = "ci-*"                     # automation đáng tin
# mode = "off"

grounding_check          = false
quarantine_failure_ratio = 0.66
quarantine_min_attempts  = 5
```

| Kịch bản                                           | `mode`   | `gate_relevance_threshold` | `memory_mode` |
|----------------------------------------------------|----------|----------------------------|---------------|
| Solo, ít friction (chỉ cần reminder)               | `nudge`  | `0.30`                     | `auto`        |
| Solo, an toàn hơn (block edit lạc chủ đề)          | `strict` | `0.30`                     | `auto`        |
| Team, review mọi write vào memory                  | `strict` | `0.30`                     | `review`      |
| Warning lỏng                                       | `nudge`  | `0.15`                     | `auto`        |
| Discipline chặt (ép recall đúng trọng tâm)         | `strict` | `0.50`                     | `auto`        |
| Automation / CI                                    | `off`    | —                          | `auto`        |

**Field cũ** (`mode = "soft"`, `gate_window_secs`, `gate_require_nudge`)
vẫn parse được để giữ backward compat — `soft` map sang `nudge`,
`gate_window_secs` thành `window_short_secs`, `gate_require_nudge` in
deprecation hint. Chạy lại `thoth setup` để migrate sang schema v2.

## Background review

Thoth tự động review session coding và lưu facts/lessons/skills — không
cần bạn yêu cầu. Lấy cảm hứng từ
[Hermes Agent](https://github.com/nousresearch/hermes-agent), nhưng tiết
kiệm token hơn ~10 lần: Thoth build context từ event log có cấu trúc
(~1k token) thay vì copy toàn bộ hội thoại (~5-50k token).

**Cách hoạt động:**

1. Hook `PostToolUse` đếm mutations (Write/Edit) kể từ lần review cuối.
2. Khi đạt `background_review_interval`, spawn process `thoth review` ngầm.
3. `thoth review` tổng hợp context từ `episodes.db`, `gate.jsonl`,
   `git diff --stat`, và `MEMORY.md` / `LESSONS.md` hiện tại.
4. Một lần gọi LLM duy nhất (qua `claude` CLI hoặc Anthropic API) trả
   về JSON chứa facts/lessons/skills.
5. Kết quả được dedup với memory hiện tại rồi ghi vào.
6. Watermark `.last-review` reset bộ đếm mutation.

**Bật trong `config.toml`:**

```toml
[discipline]
background_review          = true                # opt-in (mặc định false)
background_review_interval = 50                  # mutations giữa các lần review
background_review_min_secs = 600                 # cooldown thời gian (giây)
background_review_backend  = "auto"              # "auto" | "cli" | "api"
background_review_model    = "claude-haiku-4-5"  # model truyền xuống backend
gate_telemetry_enabled     = true                # bắt buộc (đếm mutation từ gate.jsonl)
```

| Backend | Cách | Khi nào |
|---------|------|---------|
| `cli`   | `claude --print --model <name>` qua stdin | Mặc định — dùng subscription Claude |
| `api`   | POST trực tiếp tới `api.anthropic.com` | Khi có `ANTHROPIC_API_KEY` |
| `auto`  | API nếu có key, ngược lại CLI | Khuyến nghị |

> **Quan trọng:** CLI backend luôn truyền `--model`. Không có flag này,
> subprocess `claude` sẽ kế thừa model mặc định của session tương tác
> (thường là Opus), khiến mỗi lần review đốt token premium cho task mà
> Haiku thừa sức. `background_review_interval`/`_min_secs` gate tốc độ
> spawn bằng **cả** ngưỡng mutation **lẫn** ngưỡng thời gian để burst
> edit nhanh không thể fire review liên tiếp.

Chạy thủ công: `thoth review --backend cli` (flag fallback về config)

## Compact (cô đọng memory)

`thoth review` **chỉ append** — không bao giờ xoá/merge. Qua nhiều
session, MEMORY.md / LESSONS.md tích tụ các phiên bản viết lại của cùng
một insight. Dùng `thoth compact` để gộp chúng:

```bash
thoth compact --dry-run     # xem LLM đề xuất gộp gì trước
thoth compact               # rewrite MEMORY.md + LESSONS.md tại chỗ
```

- Đọc toàn bộ entries, yêu cầu LLM gộp các phiên bản viết lại thành
  entry chuẩn, rồi **ghi đè** cả hai file.
- Backup bản gốc vào `.thoth/MEMORY.md.bak-<unix>` và
  `.thoth/LESSONS.md.bak-<unix>` trước khi ghi (cùng timestamp để
  rollback theo cặp).
- Tái dùng config `background_review_backend` / `background_review_model`
  (mặc định Haiku — rẻ; không cần config riêng).
- Từ chối chạy nếu LLM trả về rỗng hoặc shrink >95% (gần chắc chắn
  response hỏng, không phải compact thật).
- An toàn để chạy định kỳ; thiết kế là lệnh user-invoked, không trigger
  tự động qua hook.

## Status line

`thoth setup` cài status line vào Claude Code hiển thị:

```
⚡ debt:5 | 📝 12F/8L | 🔄 2m ago
```

| Mục | Ý nghĩa |
|-----|---------|
| `debt:N` | Reflection debt trong session (mutations trừ remembers) |
| `NF/NL` | Tổng facts trong MEMORY.md / lessons trong LESSONS.md |
| `🔄 Xm ago` | Thời gian từ lần background review cuối (hoặc "never") |

Script nằm ở `.claude/thoth-statusline.sh`, refresh mỗi 5 giây.

## Kiến trúc

```
  ┌── Cowork / Claude Code ────────────────────────────────────────────┐
  │                                                                    │
  │   .claude/settings.json     do `thoth setup` cài                   │
  │   ├── hooks                  SessionStart / PreToolUse /           │
  │   │                          PostToolUse / Stop                    │
  │   ├── mcpServers.thoth       khởi chạy `thoth-mcp`                 │
  │   └── .claude/skills/        memory-discipline + thoth-reflect     │
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
  │            thoth.grounding_check                                   │
  │   resources thoth://memory/MEMORY.md, thoth://memory/LESSONS.md    │
  └────────────────────────┬───────────────────────────────────────────┘
                           │
                           ▼
  ┌── `.thoth/` store ─────────────────────────────────────────────────┐
  │   episodes.db           event log (query_issued, nudge_invoked…)   │
  │   graph.redb            symbol graph (Calls, Imports, Extends,     │
  │                         References, DeclaredIn edges)              │
  │   fts.tantivy/          BM25 index                                 │
  │   vectors.db            flat cosine vector index (Mode::Full)      │
  │   chunks.lance/         LanceDB index (Mode::Full + `lance`)       │
  │   MEMORY.md             facts                                      │
  │   LESSONS.md            lessons                                    │
  │   LESSONS.quarantined.md  lessons bị loại                          │
  │   MEMORY.pending.md, LESSONS.pending.md  staged (review mode)      │
  │   memory-history.jsonl  audit trail                                │
  │   gate.jsonl            gate decision (khi telemetry bật)          │
  │   domain/<ctx>/DOMAIN.md        business rule đã accepted          │
  │   domain/<ctx>/_remote/<src>/*  snapshot ingestor ghi (proposed)   │
  │   skills/               procedural skills                          │
  └────────────────────────────────────────────────────────────────────┘
```

Ba lớp enforcement (tăng dần độ khó bypass):

1. **Prompt + skill** — SessionStart hook inject lessons vào context;
   skill `memory-discipline` dẫn dắt flow recall → nudge → act → reflect.
2. **Hook prompt** — reminder ngắn ở Pre/PostToolUse; khó miss nhưng vẫn là text.
3. **`thoth-gate`** — binary native chạy mọi PreToolUse của
   `Write` / `Edit` / `Bash` / `NotebookEdit`, quyết định theo 3 yếu tố:
   - **Intent.** Bash read-only (cargo test / git status / grep / rg /
     ls / cat / ...) bypass yên lặng. Tool mutation đi tiếp bước 2.
   - **Recency.** Nếu có event `query_issued` trong `gate_window_short_secs`,
     pass luôn không check relevance. Window ngắn (60s mặc định) chặn
     được kiểu "recall một lần, edit mãi mãi".
   - **Relevance.** Qua short window rồi thì gate tokenize edit context
     (tên file, old/new string, body diff) và scoring containment
     với mọi recall trong `gate_window_long_secs`. Score ≥
     `gate_relevance_threshold` → pass. Miss thì `mode` quyết:
     `off` → pass yên lặng, `nudge` → pass + stderr warning,
     `strict` → `{"decision":"block"}`.

   Stderr message **có hành động cụ thể**: liệt kê edit tokens, top
   recall gần nhất + overlap score, và gợi ý query `thoth_recall`
   dựng từ các token chưa recall cover được. Agent copy-paste là unblock.

   **Actor-aware policy** (`THOTH_ACTOR` env + `[[discipline.policies]]`
   glob pattern) cho phép một gate binary xử lý nhiều caller khác nhau —
   Claude Code interactive strict, worker orchestrator nudge, CI off.

   **Telemetry** opt-in (`gate_telemetry_enabled = true`) ghi mọi
   quyết định vào `.thoth/gate.jsonl` để calibrate threshold bằng
   dữ liệu thật thay vì đoán.

`thoth-gate` **fail-open** khi có lỗi (thiếu DB, config hỏng, SQLite
corrupted) — revert về `nudge` yên lặng chứ không brick editor. Check
stderr nếu thấy gate yếu bất thường.

## CLI cheatsheet

```bash
# lifecycle
thoth setup
thoth setup --status
thoth init
thoth index .
thoth watch .
thoth query "how does the nudge flow work"

# phân tích dựa trên code graph (do `thoth index` build)
thoth impact  "module::symbol" --direction up -d 3         # blast radius
thoth context "module::symbol"                             # 360° một symbol
thoth changes --from -                                      # diff truyền qua stdin
thoth changes                                               # mặc định `git diff HEAD`

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
thoth domain sync --source file       --from ./specs/             # offline / test
thoth domain sync --source notion     --project-id <database-id>  # cần NOTION_TOKEN
thoth domain sync --source asana      --project-id <gid>          # cần ASANA_TOKEN
thoth domain sync --source notebooklm                             # stub; export → file

# background review
thoth review                              # chạy 1 lần (auto backend, model từ config)
thoth review --backend cli                # ép dùng claude CLI (subscription)
thoth review --backend api                # ép dùng Anthropic API (cần key)
thoth review --model claude-haiku-4-5     # override model cho lần chạy này

# compact memory (gộp near-dup, ghi đè MEMORY/LESSONS)
thoth compact --dry-run                   # xem đề xuất gộp trước khi ghi
thoth compact                             # rewrite cả 2 file (kèm .bak-<ts>)

# Claude Code
thoth install                             # skills + hooks + MCP + statusline
thoth install --scope user
thoth uninstall

# eval — precision@k / MRR / latency p50·p95; ablation Zero vs Full
thoth eval --gold eval/gold.toml -k 8
thoth eval --gold eval/gold.toml --mode both --embedder voyage
```

## MCP server

`thoth-mcp` dùng JSON-RPC 2.0 qua stdio (MCP `2024-11-05`).

| Tool                      | Mô tả                                                              |
| ------------------------- | ------------------------------------------------------------------ |
| `thoth_recall`            | Hybrid recall Mode::Zero (symbol + BM25 + graph + markdown, RRF)   |
| `thoth_index`             | Parse + index                                                      |
| `thoth_impact`            | Blast radius — ai vỡ nếu đổi `fqn` (BFS depth-grouped)             |
| `thoth_symbol_context`    | 360° một symbol: callers / callees / extends / extended_by / siblings |
| `thoth_detect_changes`    | Parse unified diff → symbol bị đụng + upstream blast radius        |
| `thoth_remember_fact`     | Lưu fact                                                           |
| `thoth_remember_lesson`   | Lưu lesson (không overwrite)                                       |
| `thoth_memory_show`       | Đọc memory                                                         |
| `thoth_memory_pending`    | Danh sách pending                                                  |
| `thoth_memory_promote`    | Approve                                                            |
| `thoth_memory_reject`     | Reject                                                             |
| `thoth_memory_history`    | Audit log                                                          |
| `thoth_memory_forget`     | TTL + cleanup                                                      |
| `thoth_episode_append`    | Append event từ hook                                               |
| `thoth_lesson_outcome`    | Track success/failure                                              |
| `thoth_request_review`    | Yêu cầu review                                                     |
| `thoth_skill_propose`     | Đề xuất skill                                                      |
| `thoth_skills_list`       | List skill                                                         |

## Phân tích theo code graph

`thoth index` build ra symbol graph với cạnh `Calls`, `Imports`,
`Extends`, `References`, `DeclaredIn`. Ba MCP tool (và CLI tương ứng)
expose graph trực tiếp — không cần qua hybrid recall — hữu ích khi
agent đã biết symbol muốn tìm hiểu.

| Câu hỏi                                               | Tool / CLI                               |
|--------------------------------------------------------|------------------------------------------|
| *"Đụng `Foo::bar` thì vỡ những gì?"*                   | `thoth_impact` / `thoth impact`          |
| *"Xem mọi thứ quanh `Foo::bar`"*                       | `thoth_symbol_context` / `thoth context` |
| *"PR này đụng symbol nào, downstream gì cần re-test?"* | `thoth_detect_changes` / `thoth changes` |

- **`thoth impact`** BFS từ symbol — `--direction up` (mặc định) đi
  theo cạnh `Calls`, `References`, `Extends` ngược về để trả lời
  "ai phụ thuộc mình"; `--direction down` đi xuôi cho "mình phụ thuộc
  ai". Kết quả group theo depth, direct callers tách khỏi transitive.
- **`thoth context`** trả về 360° view phân loại: callers, callees,
  parent type, subtype, references, sibling cùng file, và import
  chưa resolve được (để dependency third-party vẫn hiện ra mà không
  cần inject stub node).
- **`thoth changes`** parse unified diff (từ `--from <file>`,
  `--from -` qua stdin, hoặc mặc định `git diff HEAD`), giao line
  range của hunk với declaration span của symbol đã index, trả về
  symbol bị đụng + upstream blast radius. Tiện cho PR pre-check:
  "7 function này cần re-test vì anh đã sửa X".

Indexer giờ resolve call target qua **bản đồ file-local** dựng từ
import alias (`use foo::Bar as Baz` / `import { a as b }` /
`from x import y as z` / alias trong Go) và symbol cùng file, nên
cạnh `Calls` nối được xuyên module chứ không dead-end ở tên bare.
Thừa kế class / trait emit cạnh `Extends` cho Rust `impl Trait for
Type`, TS/JS `extends` / `implements`, Python multi-inheritance.

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

## Benchmarks

### Microbench `graph_bfs`

Criterion bench chạy trên synthetic 4-ary tree (~341 nodes, 5 levels),
BFS depth 8. Chạy bằng `cargo bench -p thoth-store --bench graph_bfs`.

| Direction | Điểm bắt đầu | Median | Ghi chú |
|-----------|--------------|-------:|---------|
| `Out` | root | **1.74 ms** | walk toàn bộ cây (340 nodes reachable) |
| `In` | leaf sâu nhất | **13.5 µs** | leo 5 ancestor qua reverse index |
| `Both` | leaf sâu nhất | **500 µs** | union walk — quay lại full tree |

Reverse-edge index (`edges_by_dst`) là lý do `In` xuống còn chục
microsecond: trước đó, mỗi bước frontier theo chiều ngược phải scan
toàn bộ bảng `edges`. Số của `Out` bị chi phối bởi số lượng neighbour
phải deserialize — mọi node reachable đều bị decode một lần.

Số đo chi tiết cho các flow khác (indexing, recall, eval) nằm ở phần
*Benchmarks* trong README tiếng Anh.

## Release flow

* Tag `vX.Y.Z`
* Build multi-platform binary
* Upload artifact + checksum
* Update Homebrew + npm package

## Dùng như library

Xem phần *Embedding as a library* trong README tiếng Anh.

## Đóng góp

Chào đón mọi đóng góp: bug, feature, translation, PR.
Xem [`CONTRIBUTING.md`](./CONTRIBUTING.md).

## Trạng thái

**Alpha.** Core design đã ổn định. M1–M7 hoàn thành. Từ 0.0.1-alpha
trở đi đã landed thêm:

- **M8 — Discipline v2**: `thoth-gate` 3-factor decision (intent /
  recency / relevance), actor-aware policy, JSONL telemetry.
- **M9 — Graph-centric tools**: `thoth_impact`, `thoth_symbol_context`,
  `thoth_detect_changes` (MCP + CLI), import-alias resolution +
  `Extends` edge trong indexer.
- **M10 — Eval hardening**: `thoth eval` báo P@k + MRR + latency
  p50/p95; flag `--mode zero|full|both` cho ablation.

MCP-universal ingestor vẫn nằm trong roadmap.

## License

Dual license: Apache 2.0 hoặc MIT.


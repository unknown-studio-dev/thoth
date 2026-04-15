# thoth-discipline

[🇬🇧 English](./README.md) · **🇻🇳 Tiếng Việt**

Plugin Claude Code / Cowork bọc session của bạn trong một vòng lặp kỷ
luật-bộ-nhớ. Nó biến [Thoth](https://github.com/unknown-studio-dev/thoth)
MCP server chạy local thành bộ nhớ phản chiếu (reflective memory) lâu
dài cho coding agent — để Claude thôi bịa API, thôi học lại convention
mỗi session, và không phớt lờ bài học đã học hôm qua.

> **Cần binary Thoth.** Plugin gọi `thoth-mcp` và `thoth-gate` qua hooks
> và `.mcp.json`. Cài binary trước — `brew install thoth` hoặc
> `npm i -g thoth-memory` — rồi mới cài plugin. Chi tiết xem
> [README chính](../../README.vi.md#cài-đặt).

## Plugin làm gì

Mỗi session:

1. **SessionStart** — load `LESSONS.md` + `MEMORY.md` vào context.
2. **Trước mọi Write/Edit** — ép một pass `thoth.nudge` nêu rõ từng
   bài học áp dụng trước khi file bị thay đổi.
3. **Sau mọi Bash** — ghi kết quả quan sát được (test, commit, error)
   vào episodic log.
4. **Stop (cuối lượt)** — chạy `thoth.reflect`, đề nghị persist fact /
   lesson lâu dài.

Không dùng API key trả phí. Toàn bộ vòng lặp chạy trên Thoth daemon
local, dùng subscription Claude Code để suy luận.

## Thành phần

| Loại     | Tên                 | Mục đích                                        |
| -------- | ------------------- | ----------------------------------------------- |
| Skill    | `memory-discipline` | Loop mỗi lần act: recall → nudge → act → log    |
| Skill    | `thoth-reflect`     | Reflection + persist cuối session               |
| Hooks    | `hooks/hooks.json`  | Gắn skill vào SessionStart / PreToolUse / PostToolUse / Stop |
| MCP      | `thoth` (stdio)     | Khởi chạy binary `thoth-mcp` local              |

## Cài đặt

### 1. Cài binary Thoth

Khuyến nghị nhất — prebuilt qua brew hoặc npm:

```bash
brew tap unknown-studio-dev/thoth && brew install thoth
# hoặc
npm install -g thoth-memory
```

Build from source (cần Rust toolchain):

```bash
cargo install --path crates/thoth-cli
cargo install --path crates/thoth-mcp
```

`thoth-mcp` crate ship 2 binary: MCP stdio server và `thoth-gate` — hook
ép buộc của strict mode. Cả ba phải nằm trên `$PATH`. Kiểm tra:

```bash
thoth --version
thoth-mcp --version
thoth-gate < /dev/null   # phải in {"decision":"approve", ...}
```

Không cần Python, Node hay runtime nào khác — chỉ hai binary Rust.

### 2. Index project

Ở project root:

```bash
thoth setup    # wizard cấu hình mode / memory_mode / gate_window_secs
thoth index .
```

Lệnh tạo `.thoth/` gồm code graph, BM25 index, file markdown (`MEMORY.md`,
`LESSONS.md`), và episodic SQLite log.

### 3. Cài plugin

Thả file `.plugin` vào Cowork (hoặc `claude plugin install …` với Claude
Code). MCP server tự khởi chạy khi session bắt đầu.

### 4. (Tuỳ chọn) Cấu hình enforcement

`thoth setup` đã ghi sẵn, bạn có thể sửa tay `<project>/.thoth/config.toml`:

```toml
[memory]
episodic_ttl_days  = 30
enable_nudge       = true

[discipline]
mode                      = "soft"      # "soft" hoặc "strict"
global_fallback           = true        # fallback ~/.thoth nếu project không có
reflect_cadence           = "end"       # "end" hoặc "every"
nudge_before_write        = true
grounding_check           = false
gate_window_secs          = 180         # tuổi tối đa của recall còn có giá trị

# v2 knobs ---------------------------------------------------------------
memory_mode               = "auto"      # "auto" (commit) hoặc "review" (stage)
gate_require_nudge        = false       # strict mode còn yêu cầu thoth.nudge
quarantine_failure_ratio  = 0.66        # ngưỡng failure_count / attempts
quarantine_min_attempts   = 5           # attempts tối thiểu trước khi quarantine
```

**Preset gợi ý:**

| Kịch bản            | `mode`   | `gate_require_nudge` | `memory_mode` |
|---------------------|----------|----------------------|---------------|
| Solo, ít ma sát     | `soft`   | `false`              | `auto`        |
| Solo, cẩn thận      | `strict` | `false`              | `auto`        |
| Team, thử nghiệm    | `strict` | `true`               | `review`      |
| Team, hậu v1        | `strict` | `true`               | `auto`        |

## Soft vs. strict mode

| Khía cạnh                        | `soft` (mặc định)         | `strict`                          |
| -------------------------------- | ------------------------- | --------------------------------- |
| Agent skip loop recall/nudge?    | Reminder append vào lượt  | Tool call **bị chặn** bởi hook    |
| Cơ chế ép buộc                   | Hook dạng prompt          | Hook prompt + binary `thoth-gate` |
| Nguồn sự thật của gate           | Self-reflection của Claude | `.thoth/episodes.db` (SQLite)     |
| Agent có thể tự thuyết phục qua? | Có                        | Không — hook fail trước tool      |

**Strict mode hoạt động thế nào.** Mỗi call `thoth_recall` append event
`query_issued` vào `episodes.db`. PreToolUse hook chạy binary
`thoth-gate` — một executable standalone nhỏ ship cùng `thoth-mcp`. Nó
mở `episodes.db` read-only, query event đó gần nhất. Nếu recall cuối
già hơn `gate_window_secs` (hoặc chưa bao giờ có), hook trả:

```json
{"decision": "block",
 "reason": "Thoth discipline: last `thoth_recall` was 420s ago ..."}
```

Claude Code hiện reason đó, tool call bị abort, agent buộc phải gọi
`thoth_recall` trước khi retry. Vì gate đọc SQLite chứ không hỏi model,
không thể bypass bằng self-talk.

Gate **fail open** khi `.thoth/` không tồn tại hoặc file SQLite thiếu —
gate hỏng phải không bao giờ làm đơ editor. Trong trường hợp đó
`stderr` có warning 1 dòng, tool call vẫn chạy tiếp.

Bật strict mode sau khi đã có đủ LESSONS.md để đáng chịu ma sát — bắt
đầu bằng `soft` để quan sát mà không bị chặn.

### Strict mode hai-event

Set `gate_require_nudge = true` cùng `mode = "strict"` biến gate thành
kiểm tra hai-event: trước mọi `Write`/`Edit`/`Bash`, agent phải đã log
**cả** `query_issued` (từ `thoth_recall`) **và** `nudge_invoked` (từ
expand prompt `thoth.nudge`) trong `gate_window_secs`. Điều này đóng
lỗ hổng "agent chạy recall lấy lệ rồi đi tiếp" — prompt nudge không
thể gian lận vì server chỉ log `nudge_invoked` khi `prompts/get` thực
sự được serve cho `thoth.nudge`.

## Chế độ review cho memory

`memory_mode = "review"` đổi nơi fact/lesson mới landing:

- `thoth_remember_fact` ghi vào `MEMORY.pending.md` (không phải MEMORY.md).
- `thoth_remember_lesson` ghi vào `LESSONS.pending.md`.
- User (hoặc reviewer) promote explicit:

```bash
thoth memory pending           # list với index
thoth memory promote lesson 0  # chuyển lesson [0] vào LESSONS.md
thoth memory reject fact 2 --reason "trùng với §4 MEMORY.md"
thoth memory log --limit 50    # xem audit trail
```

**Conflict detection.** Kể cả trong `auto` mode, server không âm thầm
ghi đè lesson đã tồn tại: nếu `trigger` đã có, entry mới được stage
thay vì append, và tool trả về block `conflict`. Agent phải flag cho
user (`thoth_request_review`) thay vì đoán mò.

**Tự sửa chữa.** Forget pass auto-move lesson có
failure_count / attempts > `quarantine_failure_ratio` (sau
`quarantine_min_attempts` lần thử) vào `LESSONS.quarantined.md`.
Không xoá — giữ lại để con người khôi phục bằng tay nếu muốn.

**Versioning.** Mỗi mutation của memory (stage / promote / reject /
quarantine / propose / request_review) append một dòng JSONL vào
`<root>/memory-history.jsonl` kèm timestamp, actor, reason. Đây là
audit trail — `thoth memory log` tail nó.

**Skill tự hoàn thiện.** Khi agent thấy cùng pattern trong ≥5 lesson,
nó có thể gọi `thoth_skill_propose { slug, body, source_triggers }`.
Draft landing dưới `.thoth/skills/<slug>.draft/` và một entry `propose`
được thêm vào history log. Promote bằng
`thoth skills install .thoth/skills/<slug>.draft`.

## Biến môi trường

| Biến         | Mặc định | Ý nghĩa                                   |
| ------------ | -------- | ----------------------------------------- |
| `THOTH_ROOT` | `.thoth` | Root directory của Thoth store            |
| `RUST_LOG`   | `info`   | Log level `thoth-mcp` (`debug`, `trace`)  |

## Ví dụ dùng

Skill và hook đều tự chạy — bạn chỉ việc dùng Claude như bình thường.
Nhưng cũng có thể gọi trực tiếp:

```
> /memory-discipline
```

…ép loop chạy ngay trên lượt hiện tại.

```
> /thoth-reflect
```

…ép reflection cuối session ngay, không chờ hook Stop.

## Khác gì một MCP thường

Một MCP server vanilla chỉ cho Claude *tool*. Plugin này còn cho
Claude **prompt** (template ép self-reflection) và **hook** (trigger
loop tự động). Kết hợp cả ba là thứ biến "thỉnh thoảng nhớ" thành
thói quen.

## Troubleshooting

- **"thoth-mcp not found"** — binary chưa trên `$PATH`. Chạy lại
  `cargo install --path crates/thoth-mcp` (hoặc `brew install thoth`).
- **"no recall hits for X"** — bạn chưa index repo. Chạy `thoth index .`
  ở project root.
- **Hook thấy ồn** — set `reflect_cadence = "end"` và
  `grounding_check = false` trong `config.toml`.
- **Strict mode block edit** — đọc block reason, chạy
  `thoth_recall` + `thoth.nudge` explicit, rồi retry.
- **Gate không trigger** — đảm bảo `thoth-gate` trên `$PATH`
  (`cargo install --path crates/thoth-mcp` cài kèm `thoth-mcp`).
  Chạy `thoth-gate < /dev/null` tay để xem verdict; xem stderr.
- **Gate fail open bất ngờ** — stderr nói lý do (parse error,
  permission, thiếu file). Binary luôn fail open nên gate hỏng không
  chặn bạn — nhưng misconfiguration cũng âm thầm hạ về soft mode.
  Nếu strict có vẻ yếu, check stderr.

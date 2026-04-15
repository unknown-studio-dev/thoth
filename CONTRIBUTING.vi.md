# Đóng góp cho Thoth

[🇬🇧 English](./CONTRIBUTING.md) · 🇻🇳 Tiếng Việt

Cảm ơn bạn dành thời gian — Thoth còn rất mới, và mỗi reproducer, PR,
comment thiết kế đều có giá trị. Tài liệu này nêu cách dựng repo, những
quy ước code chúng tôi giữ, và một đóng góp tốt trông ra sao.

## Chạy nhanh

```bash
git clone https://github.com/unknown-studio-dev/thoth.git
cd thoth
cargo build --workspace
cargo test  --workspace --all-targets

# Smoke test end-to-end trên chính repo này
make demo
```

Yêu cầu:

- **Rust** ≥ 1.91 (pin trong `rust-toolchain.toml`)
- **Git** ≥ 2.30
- Với `Mode::Zero` là đủ. `Mode::Full` cần thêm `VOYAGE_API_KEY` /
  `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` tuỳ provider.

## Layout repo

Xem [`README.md`](./README.md). Tóm tắt: 1 Cargo workspace gồm 11 crate,
plugin Claude Code / Cowork trong `plugins/thoth-discipline/`, và
scaffolding packaging trong `packaging/`.

## Workflow

1. **Mở issue trước** cho bất cứ thay đổi nào không nhỏ. Bug thì dùng
   template [Bug report][bug] để có reproducer. Feature dùng
   [Feature request][feat] — template hỏi đúng câu về scope và impact.
   Report "memory drift" (agent nói sai đáng ra `LESSONS.md` đã chặn)
   đi vào template [Memory drift][drift].
2. **Branch từ `master`**. Tên ngắn, rõ: `feat/two-event-gate`,
   `fix/forget-pass-underflow`, `docs/readme-vi`.
3. **Commit theo ý tưởng.** Nhỏ, self-contained, mỗi commit một ý. Chúng
   tôi không squash khi merge — history của bạn hiện trên `git log`.
4. **Chạy đủ check trước khi push:**

   ```bash
   cargo fmt --all
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test  --workspace --all-targets
   ```

5. **Mở PR.** Template [PR](./.github/pull_request_template.md) có
   checklist. Link issue(s) bạn đang xử lý. CI chạy `fmt-check` /
   `clippy -D warnings` / `cargo test` / một precision@k eval — cả bốn
   phải xanh.

[bug]:   https://github.com/unknown-studio-dev/thoth/issues/new?template=bug_report.yml
[feat]:  https://github.com/unknown-studio-dev/thoth/issues/new?template=feature_request.yml
[drift]: https://github.com/unknown-studio-dev/thoth/issues/new?template=memory_drift.yml

## Code style

- **Rust 2024 edition**, `rustfmt` default, clippy sạch với `-D warnings`.
- **Không `unwrap()` trong library code.** Lỗi dự đoán được đi qua
  `thiserror`; lỗi bất ngờ đi qua `anyhow::Context` ở biên CLI/MCP.
- **Async** là `tokio` + `async-trait`. Không trộn runtime.
- **Docs**: mọi public item cần doc comment. Ví dụ dùng ````text` khi
  chỉ để minh hoạ, ````rust` chỉ khi `cargo test --doc` thực sự compile.
- **Test** đặt cạnh code trong `mod tests` (unit) và dưới `tests/`
  (integration). Integration test gọi provider thật được gate sau feature
  (`voyage`, `openai`, `anthropic`) để CI không cần secret.
- **Commit**: imperative mood, subject ≤ 72 ký tự. Body wrap ở 80. Tham
  chiếu issue bằng `#N` khi liên quan.

## Mảng hoan nghênh đóng góp

- **Thêm grammar ngôn ngữ** trong `thoth-parse` — bất cứ gì tree-sitter
  hỗ trợ. Tăng coverage semantic recall.
- **Embedder adapter** trong `thoth-embed` — đặc biệt hoan nghênh model
  tự host / chạy local.
- **Gold set** trong `eval/` — recall chỉ tốt bằng những gì ta đo được.
- **Plugin docs + ví dụ** — show `LESSONS.md` thật đang tiến hoá trên
  project thật.
- **Dịch** — nếu bạn review/mở rộng được docs tiếng Việt hoặc thêm ngôn
  ngữ khác, mở PR vào `docs/<lang>/`.

## Thay đổi thiết kế

Bất cứ gì đụng đến [`DESIGN.md`](./DESIGN.md) hoặc đổi layout trên đĩa
của `.thoth/` cần một PR "design note" trước. Thảo luận xong mới code.
Điều này để tránh đổi format store làm vỡ project có sẵn — một trong
những thứ memory layer cố tình tránh.

## Release (maintainer)

1. Bump `version` trong workspace `Cargo.toml` + `plugins/thoth-discipline/.claude-plugin/plugin.json`.
2. Cập nhật `CHANGELOG.md`.
3. Tag `vX.Y.Z` trên `main`. CI publish binary + `.plugin`.
4. `packaging/homebrew/bump.sh vX.Y.Z` → commit vào tap `homebrew-thoth`.
5. `packaging/npm/publish.sh vX.Y.Z` → publish lên npm.

## Code of conduct

Tử tế. Chính xác. Bất đồng thì nói sớm, nói thẳng. Ship thứ sống được
khi va vào codebase thật.

## License

Khi đóng góp, bạn đồng ý code của mình được dual-licensed dưới
[Apache 2.0](./LICENSE-APACHE) và [MIT](./LICENSE-MIT), khớp với license
của dự án.

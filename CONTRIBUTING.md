# Contributing to Thoth

Thanks for taking the time — Thoth is a young project and every reproducer,
PR, and design comment helps. This doc covers how to get the repo running,
the conventions we hold the code to, and what a good contribution looks
like.

## Quick start

```bash
git clone https://github.com/unknown-studio-dev/thoth.git
cd thoth
cargo build --workspace
cargo test  --workspace --all-targets

# End-to-end smoke test against this very repo
make demo
```

Requirements:

- **Rust** ≥ 1.91 (pin in `rust-toolchain.toml`)
- **Git** ≥ 2.30
- That's it for `Mode::Zero`. For `Mode::Full` you'll also want `VOYAGE_API_KEY`
  / `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` as appropriate.

## Repo layout

See [`README.md`](./README.md). In short: one Cargo workspace with 10
crates. The Claude Code wiring (MCP config, hooks, skills) is bundled
inside the CLI binary at compile time — source-of-truth files live in
`crates/thoth-cli/assets/` and `thoth setup` writes them into
`.claude/settings.json`. Packaging scaffolding (Homebrew formula, npm
wrapper) sits under `packaging/`.

## Workflow

1. **Open an issue first** for anything non-trivial. For bug reports, use
   the [Bug report][bug] template so we have a reproducer. For features,
   the [Feature request][feat] template asks the right questions about
   scope and impact. A "memory drift" report (the agent said something
   that should have been corrected by `LESSONS.md`) goes to the
   [Memory drift][drift] template.
2. **Branch off `master`**. Short, descriptive names: `feat/two-event-gate`,
   `fix/forget-pass-underflow`, `docs/readme-vi`.
3. **Keep commits logical.** Small, self-contained, one idea per commit.
   We don't squash on merge — your history shows up in `git log`.
4. **Run the full check before pushing:**

   ```bash
   cargo fmt --all
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test  --workspace --all-targets
   ```

5. **Open a PR.** The [PR template](./.github/pull_request_template.md)
   covers the checklist. Link the issue(s) you're addressing. CI runs
   `fmt-check` / `clippy -D warnings` / `cargo test` / a precision@k eval
   — all four must be green.

[bug]:   https://github.com/unknown-studio-dev/thoth/issues/new?template=bug_report.yml
[feat]:  https://github.com/unknown-studio-dev/thoth/issues/new?template=feature_request.yml
[drift]: https://github.com/unknown-studio-dev/thoth/issues/new?template=memory_drift.yml

## Code style

- **Rust 2024 edition**, `rustfmt` default config, clippy-clean with
  `-D warnings`.
- **No `unwrap()` in library code.** Expected errors go through `thiserror`;
  unexpected ones through `anyhow::Context` at the CLI/MCP boundary.
- **Async** is `tokio` + `async-trait`. Don't mix runtimes.
- **Docs**: every public item needs a doc comment. Examples use ````text`
  if they're illustrative, ````rust` only if `cargo test --doc` will
  actually compile them.
- **Tests** live next to the code in `mod tests` (unit) and under
  `tests/` (integration). Integration tests that hit real providers are
  gated behind their feature (`voyage`, `openai`, `anthropic`) so CI
  doesn't need secrets.
- **Commits**: imperative mood, ≤ 72 chars on the subject. Body wraps at
  80. Reference issues by `#N` where relevant.

## Areas that welcome help

- **More language grammars** in `thoth-parse` — anything tree-sitter
  supports. Adds semantic recall coverage.
- **Embedder adapters** in `thoth-retrieve` — self-hosted / local models
  are especially welcome (feature-gated modules).
- **Gold sets** in `eval/` — recall is only as good as we can measure.
- **Setup-flow polish and docs** — show off real `LESSONS.md` evolving
  over a real project; improve the `thoth setup` wizard prompts; better
  walkthroughs of hook / skill behavior.
- **Translations** — if you can review/extend the Vietnamese docs or
  add another language, open a `docs/<lang>/` PR.

## Design changes

Anything that changes the on-disk layout of `.thoth/` needs a
design-note PR first. We'll discuss, then implement. This is to avoid
store-format churn that breaks existing projects — one of the things the
memory layer is specifically designed not to do.

## Releasing (maintainers)

1. Bump `version` in the `Cargo.toml` workspace and
   `packaging/npm/thoth/package.json` (wrapper) +
   `packaging/npm/platform-stubs/template/package.json` (stub).
2. Update `CHANGELOG.md`.
3. Tag `vX.Y.Z` on `main`. CI builds + uploads platform tarballs + sha256s.
4. `packaging/homebrew/bump.sh vX.Y.Z` → commit to `homebrew-thoth` tap.
5. `packaging/npm/publish.sh vX.Y.Z` → publishes wrapper + 4 platform stubs to npm.

## Code of conduct

Be kind. Be precise. Disagree openly and early. Ship things that survive
contact with real codebases.

## License

By contributing you agree that your code is dual-licensed under
[Apache 2.0](./LICENSE-APACHE) and [MIT](./LICENSE-MIT), matching the
project itself.

<!--
  Thanks for the PR! Fill in the sections below. Delete any that don't
  apply. If this is a design-note / docs-only PR, keep the heading and
  a one-line note — the reviewer can skip the rest.
-->

## Summary

<!-- 1–3 sentences on what this changes and why. -->

## Related issues

<!-- Closes #NNN / Refs #NNN. Required for anything non-trivial. -->

## Type of change

- [ ] Bug fix (non-breaking change that fixes an issue)
- [ ] New feature (non-breaking change that adds capability)
- [ ] Breaking change (on-disk layout, CLI flags, MCP tool signatures)
- [ ] Docs / README only
- [ ] Refactor / internal cleanup (no behaviour change)
- [ ] Packaging / CI

## How was this tested?

<!--
  Every PR needs some evidence the change works. Prefer automated tests;
  manual repros are fine for now but note them here.
-->

- [ ] `cargo test --workspace --all-targets` passes locally
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` is clean
- [ ] `cargo fmt --all --check` is clean
- [ ] New/changed code has unit or integration tests
- [ ] Ran `make demo` end-to-end (for retrieval / plugin changes)
- [ ] Smoke-tested strict-mode gate (for `thoth-gate` changes)

## Discipline impact

<!--
  Does this change the memory lifecycle? Leave empty if no. Otherwise
  call out:
    - Breaking MEMORY.md / LESSONS.md / episodes.db format changes
    - New event kinds (needs exhaustive-match audit)
    - New config.toml knobs (needs README + `thoth setup` wizard update)
    - Any change that affects `thoth-gate` verdicts
-->

## Checklist

- [ ] README (EN + VI) updated if behaviour changed
- [ ] `CHANGELOG.md` bumped
- [ ] If a new config knob: added to `thoth setup` wizard
- [ ] If a new event kind: every exhaustive match over `Event` handles it
- [ ] If a new MCP tool: added to `tools/list` catalog + both READMEs

## Notes for reviewer

<!-- Anything else worth flagging — open questions, follow-ups, risks. -->

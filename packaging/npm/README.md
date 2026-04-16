# npm packaging for Thoth

Thoth is a Rust binary. We ship it through npm using the **platform
subpackages** pattern popularised by esbuild and Biome, all scoped under
`@unknownstudio`:

- `@unknownstudio/thoth-cc` — the package users install. Pure JS: a
  `postinstall` script and three shim entrypoints (`thoth`, `thoth-mcp`,
  `thoth-gate`).
- `@unknownstudio/thoth-cc-darwin-arm64` / `-darwin-x64` /
  `-linux-arm64` / `-linux-x64` — one per platform, each contains just
  the prebuilt binaries under `bin/`.

npm's `optionalDependencies` + the `os`/`cpu` fields make npm skip every
subpackage that doesn't match the host, so the user only downloads
~5–10 MB instead of all four.

## End-user install

```bash
npm install -g @unknownstudio/thoth-cc
# or
npx @unknownstudio/thoth-cc setup
```

Both commands should Just Work on macOS (arm64/x86_64) and Linux
(arm64/x86_64). For other platforms the postinstall prints install
instructions and exits cleanly (npm install stays green).

## Publishing (maintainer)

After `release.yml` has uploaded the tarballs to a GitHub Release:

```bash
gh auth status           # must be logged in
npm whoami               # must be logged in + have publish rights on @unknownstudio
./packaging/npm/publish.sh v0.2.0

# Dry run first — pack but don't publish
DRY_RUN=1 ./packaging/npm/publish.sh v0.2.0
```

The script downloads the tarballs, re-packs each as a platform
subpackage, then publishes the main wrapper last. All packages are
scoped — the script passes `--access public` explicitly so the first
publish does not get defaulted to `restricted`. If something fails
mid-way you can re-run; npm will reject already-published versions,
which is the desired behaviour.

## Layout

```
packaging/npm/
  thoth-cc/                  main wrapper package (published as
                             @unknownstudio/thoth-cc)
    package.json
    bin/
      _shim.js               shared resolver/exec logic
      thoth.js               entrypoint: thoth
      thoth-mcp.js           entrypoint: thoth-mcp
      thoth-gate.js          entrypoint: thoth-gate
    scripts/
      postinstall.js         optional-dep guard + fallback downloader
  platform-stubs/
    template/package.json    templated per-platform package
                             (published as
                             @unknownstudio/thoth-cc-<platform>)
  publish.sh                 release helper
```

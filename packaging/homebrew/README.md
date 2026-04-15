# Homebrew tap for Thoth

This directory holds the canonical `thoth.rb` formula. It's published via a
separate tap repo — `homebrew-thoth` — because Homebrew requires formulas to
live in their own repository.

## Setting up the tap (one-time, maintainer)

```bash
gh repo create unknown-studio-dev/homebrew-thoth --public
cd homebrew-thoth
mkdir -p Formula
cp ../thoth/packaging/homebrew/thoth.rb Formula/
git add Formula/thoth.rb
git commit -m "chore: initial formula for thoth v0.2.0"
git push -u origin main
```

## Updating after a release

After `.github/workflows/release.yml` finishes and uploads tarballs,
run the helper to stamp fresh SHA256s into the formula:

```bash
./packaging/homebrew/bump.sh v0.2.0
# copy the output into homebrew-thoth/Formula/thoth.rb and commit
```

## End-user install

```bash
brew tap unknown-studio-dev/thoth
brew install thoth

thoth setup              # pick soft/strict, memory_mode, etc.
thoth index .            # index the current project
thoth install            # wire up hooks + MCP + skill
```

No Rust toolchain required — Homebrew downloads prebuilt binaries from the
GitHub Release.

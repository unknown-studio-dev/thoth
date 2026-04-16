# Homebrew formula for Thoth.
#
# This file is meant to live in a Homebrew tap (e.g. `homebrew-thoth`), not
# in this source repo. Copy it to `Formula/thoth.rb` in your tap, replace
# the SHA256 placeholders after each release, and users can:
#
#   brew tap unknown-studio-dev/thoth https://github.com/unknown-studio-dev/homebrew-thoth
#   brew install thoth
#
# Shipping model: prebuilt binaries uploaded to GitHub Releases by
# `.github/workflows/release.yml`. No Rust toolchain needed on the user's
# machine.
class Thoth < Formula
  desc "Long-term memory & disciplined recall loop for coding agents"
  homepage "https://github.com/unknown-studio-dev/thoth"
  version "0.0.1"
  license "MIT OR Apache-2.0"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/unknown-studio-dev/thoth/releases/download/v#{version}/thoth-#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_SHA256_FROM_RELEASE_ASSET"
    else
      url "https://github.com/unknown-studio-dev/thoth/releases/download/v#{version}/thoth-#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_SHA256_FROM_RELEASE_ASSET"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/unknown-studio-dev/thoth/releases/download/v#{version}/thoth-#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "REPLACE_WITH_SHA256_FROM_RELEASE_ASSET"
    else
      url "https://github.com/unknown-studio-dev/thoth/releases/download/v#{version}/thoth-#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "REPLACE_WITH_SHA256_FROM_RELEASE_ASSET"
    end
  end

  def install
    bin.install "thoth", "thoth-mcp", "thoth-gate"
  end

  def caveats
    <<~EOS
      Thoth is installed. To wire it into Claude Code in the current project,
      run the one-shot bootstrap from your repo root:

        thoth setup              # interactive wizard: hooks + MCP + skills + .thoth/

      Re-running `thoth setup` on a project that's already wired up will
      detect the existing install, offer to reinstall hooks, or self-heal
      missing pieces. After setup, edit `.thoth/config.toml` to taste, then:

        thoth index .

      `thoth-mcp` and `thoth-gate` are the daemons this formula provides;
      `thoth setup` registers them in `.claude/settings.json` for you.
    EOS
  end

  test do
    assert_match "thoth", shell_output("#{bin}/thoth --version")
    assert_match "thoth-mcp", shell_output("#{bin}/thoth-mcp --version")
    # thoth-gate fails open on a missing DB; exit code should be 0.
    system "#{bin}/thoth-gate", in: "{}"
  end
end

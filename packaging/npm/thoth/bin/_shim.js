// Shared shim for thoth / thoth-mcp / thoth-gate entrypoints.
//
// Resolution order:
//   1. The platform-specific optional dependency
//      (`@unknownstudio/thoth-<platform>-<arch>/bin/<exe>`).
//   2. The prebuilt binary downloaded by postinstall into
//      `<this-package>/bin-native/thoth-<ver>-<triple>/`.
//   3. Hard error with install instructions.
//
// Exec-replaces the current process — Ctrl-C, stdio, exit code all pass
// through as if the user ran the native binary directly.
//
// Special case: `npx @unknownstudio/thoth` with no args defaults to
// `thoth setup` — the one-shot bootstrap wizard. Subcommands always win.

const fs = require("node:fs");
const path = require("node:path");
const { spawnSync } = require("node:child_process");

const PLAT = `${process.platform}-${process.arch}`;
const EXE_SUFFIX = process.platform === "win32" ? ".exe" : "";

function fromOptionalDep(exe) {
  try {
    const pkg = require.resolve(`@unknownstudio/thoth-${PLAT}/package.json`);
    const p = path.join(path.dirname(pkg), "bin", exe + EXE_SUFFIX);
    return fs.existsSync(p) ? p : null;
  } catch {
    return null;
  }
}

function fromFallback(exe) {
  const dir = path.join(__dirname, "..", "bin-native");
  if (!fs.existsSync(dir)) return null;
  // Find a `thoth-<ver>-<triple>/<exe>` inside.
  for (const sub of fs.readdirSync(dir)) {
    const p = path.join(dir, sub, exe + EXE_SUFFIX);
    if (fs.existsSync(p)) return p;
  }
  return null;
}

function resolve(exe) {
  return fromOptionalDep(exe) || fromFallback(exe);
}

// `npx @unknownstudio/thoth` with no args drops the user into `thoth setup`.
// We only do this for the `thoth` entrypoint — `thoth-mcp` and `thoth-gate`
// are stdio servers that must accept zero args without rewriting.
function defaultArgs(exe, argv) {
  if (exe === "thoth" && argv.length === 0) {
    return ["setup"];
  }
  return argv;
}

function run(exe) {
  const bin = resolve(exe);
  if (!bin) {
    console.error(
      `@unknownstudio/thoth: no native binary for ${PLAT}. ` +
      `Reinstall via \`npm i -g @unknownstudio/thoth\` or use Homebrew: ` +
      `\`brew tap unknown-studio-dev/thoth && brew install thoth\``
    );
    process.exit(127);
  }
  const args = defaultArgs(exe, process.argv.slice(2));
  const r = spawnSync(bin, args, { stdio: "inherit" });
  if (r.error) {
    console.error(r.error.message);
    process.exit(1);
  }
  process.exit(r.status ?? 0);
}

module.exports = { run, resolve };

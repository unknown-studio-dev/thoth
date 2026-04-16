#!/usr/bin/env node
// Verify the platform-specific optional dependency got resolved. If npm
// skipped all of them (e.g. unsupported platform), fall back to downloading
// the tarball directly from GitHub Releases.
//
// Side-effect-free when the matching optional dep is already present.

const fs = require("node:fs");
const path = require("node:path");
const https = require("node:https");
const zlib = require("node:zlib");
const { pipeline } = require("node:stream/promises");

const VERSION = require("../package.json").version;
const REPO = "unknown-studio-dev/thoth";

const TRIPLES = {
  "darwin-arm64": "aarch64-apple-darwin",
  "darwin-x64":   "x86_64-apple-darwin",
  "linux-arm64":  "aarch64-unknown-linux-gnu",
  "linux-x64":    "x86_64-unknown-linux-gnu",
};

function platformKey() {
  return `${process.platform}-${process.arch}`;
}

function installedBinDir() {
  const key = platformKey();
  const pkg = `@unknownstudio/thoth-${key}`;
  try {
    const res = require.resolve(`${pkg}/package.json`);
    return path.join(path.dirname(res), "bin");
  } catch {
    return null;
  }
}

async function downloadFallback() {
  const key = platformKey();
  const triple = TRIPLES[key];
  if (!triple) {
    console.error(
      `[@unknownstudio/thoth] Unsupported platform: ${key}. ` +
      `Supported: ${Object.keys(TRIPLES).join(", ")}. ` +
      `Install from source: cargo install --git https://github.com/${REPO} thoth-cli thoth-mcp`
    );
    process.exit(0); // don't fail npm install
  }

  const tarball = `thoth-${VERSION}-${triple}.tar.gz`;
  const url = `https://github.com/${REPO}/releases/download/v${VERSION}/${tarball}`;
  const outDir = path.join(__dirname, "..", "bin-native");
  fs.mkdirSync(outDir, { recursive: true });
  const outFile = path.join(outDir, tarball);

  console.log(`[@unknownstudio/thoth] fetching ${url}`);
  await new Promise((resolve, reject) => {
    const file = fs.createWriteStream(outFile);
    https.get(url, (res) => {
      if (res.statusCode === 302 || res.statusCode === 301) {
        https.get(res.headers.location, (r2) => r2.pipe(file).on("finish", resolve)).on("error", reject);
      } else if (res.statusCode === 200) {
        res.pipe(file).on("finish", resolve);
      } else {
        reject(new Error(`HTTP ${res.statusCode}`));
      }
    }).on("error", reject);
  });

  // Extract with `tar` — every supported OS ships one.
  const { spawnSync } = require("node:child_process");
  const r = spawnSync("tar", ["-xzf", outFile, "-C", outDir], { stdio: "inherit" });
  if (r.status !== 0) {
    console.error("[@unknownstudio/thoth] tar extraction failed");
    process.exit(0);
  }
  fs.unlinkSync(outFile);
  console.log(`[@unknownstudio/thoth] installed prebuilt binaries to ${outDir}`);
}

(async () => {
  if (installedBinDir()) {
    // Optional dep resolved — nothing to do, shim scripts in bin/ will find it.
    return;
  }
  try {
    await downloadFallback();
  } catch (e) {
    console.error(`[@unknownstudio/thoth] postinstall: ${e.message}`);
    process.exit(0); // keep npm install green
  }
})();

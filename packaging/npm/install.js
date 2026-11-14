#!/usr/bin/env node
// The install-time downloader: picks the `joy` archive for (process.platform, process.arch) off
// GitHub Releases, unpacks the binary into this package's bin/. On failure it gives instructions
// you can act on, rather than leaving npm stuck on an error nobody can read.
//
// The release asset naming (release.yml's job; the two sides have to agree):
//   joy-v<version>-<target>.tar.gz
//     target ∈ { aarch64-apple-darwin, x86_64-apple-darwin,
//                x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu }

"use strict";

const { execFileSync } = require("node:child_process");
const { createWriteStream, existsSync, mkdirSync, rmSync } = require("node:fs");
const path = require("node:path");
const { get } = require("node:https");
const { pipeline } = require("node:stream/promises");

const REPO = "ChrisVip001/joyczl-agent";
const version = require("./package.json").version;

const TARGETS = {
  "darwin-arm64": "aarch64-apple-darwin",
  "darwin-x64": "x86_64-apple-darwin",
  "linux-x64": "x86_64-unknown-linux-gnu",
  "linux-arm64": "aarch64-unknown-linux-gnu",
};

const target = TARGETS[`${process.platform}-${process.arch}`];
if (!target) {
  console.error(
    `(joyczl-agent) unsupported platform: ${process.platform}-${process.arch}. ` +
      `Joy supports macOS and Linux (on Windows use WSL2, see docs/operations.md). ` +
      `or install from source: cargo install --path joy-rs/joyczl-cli`
  );
  process.exit(1);
}

const url = `https://github.com/${REPO}/releases/download/v${version}/joy-v${version}-${target}.tar.gz`;
const outDir = path.join(__dirname, "bin");
const tarball = path.join(outDir, "joy.tar.gz");

function fail(why) {
  console.error(`(joyczl-agent) download failed: ${why}\n  ${url}`);
  console.error(
    "  download it by hand, put it at " +
      path.join(outDir, "joy") +
      " and chmod +x; or install from source: cargo install --path joy-rs/joyczl-cli"
  );
  process.exit(1);
}

function fetch(url, redirects = 0) {
  if (redirects > 5) return fail("too many redirects");
  return new Promise((resolve, reject) => {
    get(url, { headers: { "user-agent": "joyczl-agent-npm-shim" } }, (res) => {
      if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
        return resolve(fetch(res.headers.location, redirects + 1));
      }
      if (res.statusCode !== 200) {
        return reject(new Error(`HTTP ${res.statusCode}`));
      }
      resolve(res);
    }).on("error", reject);
  });
}

(async () => {
  if (!existsSync(outDir)) mkdirSync(outDir, { recursive: true });
  rmSync(tarball, { force: true });
  console.log(`(joyczl-agent) downloading ${url}`);
  try {
    const response = await fetch(url);
    await pipeline(response, createWriteStream(tarball));
  } catch (error) {
    return fail(error.message);
  }
  try {
    // The system tar is enough: the archive holds the joy binary (LICENSE/README optional).
    execFileSync("tar", ["-xzf", tarball, "-C", outDir, "joy"], { stdio: "pipe" });
    execFileSync("chmod", ["+x", path.join(outDir, "joy")]);
  } catch (error) {
    return fail(`unpacking failed (${error.message})`);
  }
  rmSync(tarball, { force: true });
  console.log(`(joyczl-agent) ready: ${path.join(outDir, "joy")}`);
})();

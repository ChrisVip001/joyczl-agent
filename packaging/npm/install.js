#!/usr/bin/env node
// 安装期下载器：按 (process.platform, process.arch) 从 GitHub Releases 拉对应的
// `joy` 压缩包，解包出二进制放进本包的 bin/ 下。失败时给可执行的指引，不让
// npm 卡在一个看不懂的报错上。
//
// Release 资产名约定（release.yml 负责，两边必须一致）：
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
    `(joyczl-agent) 不支持的平台：${process.platform}-${process.arch}。` +
      `Joy 支持 macOS 与 Linux（Windows 用 WSL2，见 docs/operations.md）。` +
      `也可以从源码安装：cargo install --path joy-rs/joyczl-cli`
  );
  process.exit(1);
}

const url = `https://github.com/${REPO}/releases/download/v${version}/joy-v${version}-${target}.tar.gz`;
const outDir = path.join(__dirname, "bin");
const tarball = path.join(outDir, "joy.tar.gz");

function fail(why) {
  console.error(`(joyczl-agent) 下载失败：${why}\n  ${url}`);
  console.error(
    "  可手动下载后放到 " +
      path.join(outDir, "joy") +
      " 并 chmod +x；或从源码安装：cargo install --path joy-rs/joyczl-cli"
  );
  process.exit(1);
}

function fetch(url, redirects = 0) {
  if (redirects > 5) return fail("重定向过多");
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
  console.log(`(joyczl-agent) 下载 ${url}`);
  try {
    const response = await fetch(url);
    await pipeline(response, createWriteStream(tarball));
  } catch (error) {
    return fail(error.message);
  }
  try {
    // 系统自带 tar 就够：包里是 joy 二进制（可选 LICENSE/README）。
    execFileSync("tar", ["-xzf", tarball, "-C", outDir, "joy"], { stdio: "pipe" });
    execFileSync("chmod", ["+x", path.join(outDir, "joy")]);
  } catch (error) {
    return fail(`解包失败（${error.message}）`);
  }
  rmSync(tarball, { force: true });
  console.log(`(joyczl-agent) 就绪：${path.join(outDir, "joy")}`);
})();

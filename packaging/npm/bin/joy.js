#!/usr/bin/env node
// `joy` 的命令行入口：转发全部参数给随包下载的二进制。垫片零逻辑——
// 参数、退出码、信号都原样过去。
"use strict";

const { spawn } = require("node:child_process");
const path = require("node:path");

const binary = path.join(__dirname, process.platform === "win32" ? "joy.exe" : "joy");
const child = spawn(binary, process.argv.slice(2), { stdio: "inherit" });
child.on("error", (error) => {
  console.error(
    `(joyczl-agent) 启动失败：${error.message}。重装本包（npm i -g joyczl-agent）` +
      `或从源码安装：cargo install --path joy-rs/joyczl-cli`
  );
  process.exit(1);
});
child.on("exit", (code, signal) => {
  if (signal) process.kill(process.pid, signal);
  else process.exit(code ?? 0);
});

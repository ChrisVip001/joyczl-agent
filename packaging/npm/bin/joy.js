#!/usr/bin/env node
// The `joy` command-line entry: forwards every argument to the binary that came with the
// package. Zero logic in the shim — arguments, exit codes and signals all pass straight through.
"use strict";

const { spawn } = require("node:child_process");
const path = require("node:path");

const binary = path.join(__dirname, process.platform === "win32" ? "joy.exe" : "joy");
const child = spawn(binary, process.argv.slice(2), { stdio: "inherit" });
child.on("error", (error) => {
  console.error(
    `(joyczl-agent) failed to start: ${error.message}. reinstall this package (npm i -g joyczl-agent) ` +
      `or install from source: cargo install --path joy-rs/joyczl-cli`
  );
  process.exit(1);
});
child.on("exit", (code, signal) => {
  if (signal) process.kill(process.pid, signal);
  else process.exit(code ?? 0);
});

# Joy — task entrypoint for the three-language repo.
#
# Install just:  brew install just      (or cargo install just)
#
# Convention: this file only orchestrates; business logic lives in joy-rs/.

python := if os_family() == "windows" { "python" } else { "python3" }
rust_dir := justfile_directory() / "joy-rs"
ts_dir := justfile_directory() / "joy-ts"

# List all tasks
help:
    @just -l

# ---- Protocol: single source of truth ----------------------------------------------------
# Regenerate TypeScript and JSON Schema from the Rust types in joy-rs/joyczl-protocol.
# Run this whenever the protocol changes, or the frontend and Python SDK silently drift.
write-app-server-schema *args:
    {{ python }} {{ justfile_directory() }}/scripts/write_schema.py {{args}}

# Only check whether the generated artifacts are stale (CI; writes nothing)
check-app-server-schema:
    {{ python }} {{ justfile_directory() }}/scripts/write_schema.py --check

# ---- Docs: one number, three places ------------------------------------------------------
# The tool count appears in the code, in docs/internals/06-tools.md §6.0 (list + heading) and
# in both READMEs. Three copies drift; this is the one that notices. Why a script and not a
# cargo test: these Markdown files are not inside a crate, and a packaged crate should not go
# red because a repository document was reworded.
check-tool-docs:
    {{ python }} {{ justfile_directory() }}/scripts/check_tool_docs.py

# ---- Rust ------------------------------------------------------------------
fmt:
    cd {{ rust_dir }} && cargo fmt --all

fmt-check:
    cd {{ rust_dir }} && cargo fmt --all -- --check

# Reclaim build debris without losing compiled dependencies: incremental sessions
# (one directory per compilation unit per rebuild) and cross-target artefacts.
# Keeps `target/debug/deps`, so the next build only redoes what actually changed.
# `find -delete` rather than `rm -rf`: some environments wrap `rm` with a bulk-delete
# confirmation that a Makefile-style recipe cannot answer.
tidy:
    @find {{ rust_dir }}/target/debug/incremental -depth -delete 2>/dev/null || true
    @rm -rf {{ rust_dir }}/target/x86_64-* {{ rust_dir }}/target/aarch64-* 2>/dev/null || true
    @du -sh {{ rust_dir }}/target 2>/dev/null || true

clippy *args:
    cd {{ rust_dir }} && cargo clippy --workspace --all-targets {{args}}

fix *args:
    cd {{ rust_dir }} && cargo clippy --fix --allow-dirty --workspace --all-targets {{args}}

build *args:
    cd {{ rust_dir }} && cargo build --workspace {{args}}

# Regular tests. Protocol export tests are #[ignore]d (they write files); not run here.
test *args:
    cd {{ rust_dir }} && cargo test --workspace {{args}}

# Only the protocol crate
test-protocol *args:
    cd {{ rust_dir }} && cargo test -p joyczl-protocol {{args}}

# End to end: really start app-server, feed JSON-RPC via stdin, check every reply.
smoke: build
    {{ justfile_directory() }}/scripts/smoke.sh

# ---- Evals: the diamond before a release --------------------------------------------------
# Deterministic eval: scripted models + assertions, offline 0/1. **Must pass 100%** —
# this is the release gate: exit 0 = releasable. Cases live in evals/deterministic/.
eval: build
    # 用例按相对路径 `evals/…` 加载，所以必须从仓库根跑（不是 joy-rs）。
    cd {{ justfile_directory() }} && cargo run -q --manifest-path {{ rust_dir }}/Cargo.toml --bin joy -- eval

# Long-task benchmarks: same scripted harness, asks "did autonomous task work get better or
# worse". **Records, never blocks** — exit 0 even when cases fail. Cases live in evals/long/.
bench: build
    cd {{ justfile_directory() }} && {{ rust_dir }}/target/debug/joy bench

# judge: a real model answers one turn; a small judge model scores 0-10 by rubric. Needs a key;
# the score doesn't block releases — it answers "how good", not "can we ship".
judge: build
    cd {{ rust_dir }} && cargo run -q --bin joy -- judge

# ---- TypeScript ------------------------------------------------------------
# Install TS-side deps (npm workspace: client, gateways and dashboard frontend in one go)
ts-install:
    cd {{ ts_dir }} && npm install

# Type check and unit tests for the TS side. Generated artifacts are how TS consumes the protocol,
# so this also catches protocol drift — though the drift check above is the real guard.
ts-check: ts-install
    cd {{ ts_dir }} && npm run typecheck
    cd {{ ts_dir }} && npm test

# End to end: really start app-server, have the TS gateway ask it one thing.
smoke-gateway: build
    {{ justfile_directory() }}/scripts/smoke-gateway.sh

# Start the Telegram gateway. Needs TELEGRAM_BOT_TOKEN; without TELEGRAM_ALLOW anyone can use it.
gateway-telegram *args:
    cd {{ ts_dir }} && npm run gateway:telegram {{args}}

# Start the Discord gateway. Needs DISCORD_BOT_TOKEN; you must also enable
# Message Content Intent in the developer portal, or the connection is cut (4013).
gateway-discord *args:
    cd {{ ts_dir }} && npm run gateway:discord {{args}}

# Start the WeChat gateway. Needs WECHAT_TOKEN and WECHAT_APP_ID; WeChat wants a public address
# (only 80 / 443), so run local tunnels first for local runs.
gateway-wechat *args:
    cd {{ ts_dir }} && npm run gateway:wechat {{args}}

# Start the Lark gateway. Needs LARK_APP_ID and LARK_APP_SECRET; LARK_ALLOW takes open_ids
# (the ou_… string, not the App ID); add LARK_DOMAIN=lark for the international Lark.
# The app also needs bot capability enabled, a subscription to im.message.receive_v1, and
# **long connection as the subscription mode** — miss one and it "connects but never responds".
gateway-lark *args:
    cd {{ ts_dir }} && npm run gateway:lark {{args}}

# ---- Dashboard -----------------------------------------------------------------
# Build the frontend: type check (tsc) + bundle (esbuild) + copy static files; output lands in
# joy-ts/packages/dashboard/dist, read directly by `joy dashboard`.
# Browsers can't read .ts, so this is the only TS package that needs building.
dashboard-build: ts-install
    cd {{ ts_dir }} && npm run build --workspace @joy/dashboard

# 桌面壳（Tauri）：先出驾驶舱 dist，再编译壳。构建产物在 desktop/src-tauri/target/。
# debug 开发窗口用 `desktop-dev`；发布二进制/安装包用 `desktop-build`（去 --no-bundle 打安装包）。
@desktop-dev: dashboard-build
    cd desktop/src-tauri && cargo tauri dev
@desktop-build: dashboard-build
    cd desktop/src-tauri && cargo tauri build --no-bundle

# Start the dashboard → http://localhost:7777 (JOY_PORT changes the port).
dashboard *args:
    cd {{ rust_dir }} && cargo run --bin joy -- dashboard {{args}}

# End to end: the email connector against a scripted SMTP server, through the shipped binary.
smoke-connector: build
    {{ justfile_directory() }}/scripts/smoke-connector.sh

# End to end: start a real dashboard, use curl as a browser against both endpoints.
smoke-dashboard: build dashboard-build
    {{ justfile_directory() }}/scripts/smoke-dashboard.sh

# End to end: the business form — task API (/api/run), inbound webhooks, tier routing.
smoke-business: build dashboard-build
    {{ justfile_directory() }}/scripts/smoke-business.sh

# ---- Python SDK -------------------------------------------------------------
# Set up the Python SDK dev environment (uv builds the venv from pyproject, installs locked deps)
py-install:
    cd {{ justfile_directory() }}/sdk/python && uv sync

# Type check and unit tests for the Python SDK. Generated artifacts are also how Python consumes
# the protocol, so this catches drift too — though the drift check above is the real guard.
py-check: py-install
    cd {{ justfile_directory() }}/sdk/python && uv run mypy src/joyczl_agent tests
    cd {{ justfile_directory() }}/sdk/python && uv run pytest

# Build the binary wheel (`joyczl-agent-bin`): compile the release binary → pack → verify.
# The last step can't be skipped: without the binary, packing **doesn't fail** — it quietly
# makes an empty pure-Python wheel, worse than none (see scripts/check_python_bin_wheel.py).
build-python-bin:
    cd {{ rust_dir }} && cargo build --release -p joyczl-cli
    cd {{ justfile_directory() }}/sdk/python-bin && uv build --wheel
    {{ python }} {{ justfile_directory() }}/scripts/check_python_bin_wheel.py {{ justfile_directory() }}/sdk/python-bin/dist

# Refresh the Homebrew formula's url/sha256 for a tag (after `git tag v0.8.0 && git push --tags`).
# Computes the checksum from the same archive URL Homebrew fetches. See packaging/RELEASING.md.
release-hash *args:
    {{ justfile_directory() }}/packaging/release-hash.sh {{args}}

# End to end: really start app-server and have the Python SDK ask it. Unit tests swap the
# transport, so only this proves "the subprocess starts, and the process exits when closed".
smoke-sdk-python: build
    {{ justfile_directory() }}/scripts/smoke-sdk-python.sh

# ---- Run all checks in one command ----------------------------------------------
# gate sits last: beyond green tests and lint, the deterministic eval must also pass 100%.
check: fmt-check check-app-server-schema check-tool-docs ts-check py-check eval
    cd {{ rust_dir }} && cargo clippy --workspace --all-targets -- -D warnings
    cd {{ rust_dir }} && cargo test --workspace

# Joy — 三语言工程的任务入口。
#
# 安装 just：  brew install just      （或 cargo install just）
#
# 约定：本文件只做「编排」，业务逻辑全在 joy-rs/ 里。

python := if os_family() == "windows" { "python" } else { "python3" }
rust_dir := justfile_directory() / "joy-rs"
ts_dir := justfile_directory() / "joy-ts"

# 列出所有任务
help:
    @just -l

# ---- 协议：单一事实来源 ----------------------------------------------------
# 从 joy-rs/joyczl-protocol 的 Rust 类型重新生成 TypeScript 与 JSON Schema。
# 改了协议就必须跑这个，否则前端和 Python SDK 会静默漂移。
write-app-server-schema *args:
    {{ python }} {{ justfile_directory() }}/scripts/write_schema.py {{args}}

# 只检查生成物是否过期（CI 用，不写文件）
check-app-server-schema:
    {{ python }} {{ justfile_directory() }}/scripts/write_schema.py --check

# ---- Rust ------------------------------------------------------------------
fmt:
    cd {{ rust_dir }} && cargo fmt --all

fmt-check:
    cd {{ rust_dir }} && cargo fmt --all -- --check

clippy *args:
    cd {{ rust_dir }} && cargo clippy --workspace --all-targets {{args}}

fix *args:
    cd {{ rust_dir }} && cargo clippy --fix --allow-dirty --workspace --all-targets {{args}}

build *args:
    cd {{ rust_dir }} && cargo build --workspace {{args}}

# 常规测试。协议导出测试被 #[ignore] 标记（会写文件），不走这里。
test *args:
    cd {{ rust_dir }} && cargo test --workspace {{args}}

# 只跑协议 crate
test-protocol *args:
    cd {{ rust_dir }} && cargo test -p joyczl-protocol {{args}}

# 端到端：真的起 app-server，从 stdin 喂 JSON-RPC，检查每种应答。
smoke: build
    {{ justfile_directory() }}/scripts/smoke.sh

# ---- 评测：发版前的那颗钻石 --------------------------------------------------
# 确定性 eval：脚本化模型 + 断言，离线 0/1。**必须 100% 通过** ——
# 这就是 release gate：exit 0 = 可发布。用例在 evals/deterministic/。
eval: build
    cd {{ rust_dir }} && cargo run -q --bin joy -- eval

# judge：真模型答一轮，裁判小模型按 rubric 打 0-10 分。要 key；
# 出分不拦发版 —— 它回答「答得好不好」，不回答「能不能发」。
judge: build
    cd {{ rust_dir }} && cargo run -q --bin joy -- judge

# ---- TypeScript ------------------------------------------------------------
# 装 TS 侧依赖（npm 工作区，一次装齐客户端、网关和驾驶舱前端）
ts-install:
    cd {{ ts_dir }} && npm install

# TS 侧的类型检查与单测。生成物是 TS 消费协议的地方，所以这里也顺带
# 兜住「Rust 改了协议但 TS 没跟上」——不过真正兜住它的是上面的漂移检查。
ts-check: ts-install
    cd {{ ts_dir }} && npm run typecheck
    cd {{ ts_dir }} && npm test

# 端到端：真的起 app-server，让 TS 网关问它一句。
smoke-gateway: build
    {{ justfile_directory() }}/scripts/smoke-gateway.sh

# 起 Telegram 网关。要 TELEGRAM_BOT_TOKEN；TELEGRAM_ALLOW 不设就是谁都能用。
gateway-telegram *args:
    cd {{ ts_dir }} && npm run gateway:telegram {{args}}

# 起 Discord 网关。要 DISCORD_BOT_TOKEN；还得在开发者门户里把
# Message Content Intent 打开，否则连接会被直接掐掉（4013）。
gateway-discord *args:
    cd {{ ts_dir }} && npm run gateway:discord {{args}}

# 起微信网关。要 WECHAT_TOKEN 和 WECHAT_APP_ID；微信要一个公网可达的地址
# （只认 80 / 443），本地跑得先配内网穿透。
gateway-wechat *args:
    cd {{ ts_dir }} && npm run gateway:wechat {{args}}

# 起飞书 / Lark 网关。要 LARK_APP_ID 和 LARK_APP_SECRET；LARK_ALLOW 填 open_id
# （ou_… 那串，不是 App ID）；连国际版 Lark 加 LARK_DOMAIN=lark。
# 这个应用还得在开发者后台开机器人能力、订阅 im.message.receive_v1、
# 并且**订阅方式选「长连接」**—— 缺一件都是「连上了却永远没反应」。
gateway-lark *args:
    cd {{ ts_dir }} && npm run gateway:lark {{args}}

# ---- 驾驶舱 -----------------------------------------------------------------
# 构建前端：类型检查（tsc）+ 打包（esbuild）+ 搬静态文件，产物在
# joy-ts/packages/dashboard/dist，`joy dashboard` 直接从那儿读。
# 浏览器读不懂 .ts，所以这是 TS 侧唯一需要构建的包。
dashboard-build: ts-install
    cd {{ ts_dir }} && npm run build --workspace @joy/dashboard

# 起驾驶舱 → http://localhost:7777（JOY_PORT 换端口）。
dashboard *args:
    cd {{ rust_dir }} && cargo run --bin joy -- dashboard {{args}}

# 端到端：起一个真的驾驶舱，用 curl 当浏览器把两个端点各打一遍。
smoke-dashboard: build dashboard-build
    {{ justfile_directory() }}/scripts/smoke-dashboard.sh

# ---- Python SDK -------------------------------------------------------------
# 装 Python SDK 的开发环境（uv 按 pyproject 建 venv、装锁定的依赖）
py-install:
    cd {{ justfile_directory() }}/sdk/python && uv sync

# Python SDK 的类型检查与单测。生成物也是 Python 消费协议的地方，所以这里顺带
# 兜住「Rust 改了协议但 Python 没跟上」——不过真正兜住它的是上面的漂移检查。
py-check: py-install
    cd {{ justfile_directory() }}/sdk/python && uv run mypy src/joyczl_agent tests
    cd {{ justfile_directory() }}/sdk/python && uv run pytest

# 打二进制轮子（`joyczl-agent-bin`）：编 release 二进制 → 打包 → 开箱确认。
# 最后那步不能省：没找到二进制时打包**不会失败**，只会安静地打出个空的纯 Python
# 轮子，而那个轮子发出去比不发更坏（理由写在 scripts/check_python_bin_wheel.py）。
build-python-bin:
    cd {{ rust_dir }} && cargo build --release -p joyczl-cli
    cd {{ justfile_directory() }}/sdk/python-bin && uv build --wheel
    {{ python }} {{ justfile_directory() }}/scripts/check_python_bin_wheel.py {{ justfile_directory() }}/sdk/python-bin/dist

# 端到端：真的起 app-server，让 Python SDK 去问它。单测把传输层换成了假的，
# 所以「子进程起没起来、关掉之后进程退不退」只有这里能验。
smoke-sdk-python: build
    {{ justfile_directory() }}/scripts/smoke-sdk-python.sh

# ---- 一条命令跑完所有检查 --------------------------------------------------
# gate 挂在最后：单测、lint 全绿之外，确定性 eval 也必须 100% 通过。
check: fmt-check check-app-server-schema ts-check py-check eval
    cd {{ rust_dir }} && cargo clippy --workspace --all-targets -- -D warnings
    cd {{ rust_dir }} && cargo test --workspace

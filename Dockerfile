# Joy 的容器镜像：多阶段构建，运行镜像里只留二进制、沙箱与 CA 证书。
#
#   docker build -t joy .
#   docker run --rm -p 7777:7777 -v joy-state:/home/joy/.joy -e ANTHROPIC_API_KEY=… joy
#   docker run --rm -it -v joy-state:/home/joy/.joy -e ANTHROPIC_API_KEY=… joy app-server
#
# 状态（state.db、SOUL.md、技能、outbox、traces）全在 JOY_HOME 里，
# 所以一个 volume 就能带着走。

# ---- 构建 ---------------------------------------------------------------
FROM rust:1.98-slim AS builder
WORKDIR /src
# 先只拷 manifests：依赖层能被 Docker 缓存住，改代码不必重编依赖。
COPY joy-rs/Cargo.toml joy-rs/Cargo.lock ./joy-rs/
COPY joy-rs/joyczl-app-server/Cargo.toml ./joy-rs/joyczl-app-server/
COPY joy-rs/joyczl-cli/Cargo.toml ./joy-rs/joyczl-cli/
COPY joy-rs/joyczl-config/Cargo.toml ./joy-rs/joyczl-config/
COPY joy-rs/joyczl-eval/Cargo.toml ./joy-rs/joyczl-eval/
COPY joy-rs/joyczl-graph/Cargo.toml ./joy-rs/joyczl-graph/
COPY joy-rs/joyczl-loop/Cargo.toml ./joy-rs/joyczl-loop/
COPY joy-rs/joyczl-mcp/Cargo.toml ./joy-rs/joyczl-mcp/
COPY joy-rs/joyczl-memory/Cargo.toml ./joy-rs/joyczl-memory/
COPY joy-rs/joyczl-ops/Cargo.toml ./joy-rs/joyczl-ops/
COPY joy-rs/joyczl-protocol/Cargo.toml ./joy-rs/joyczl-protocol/
COPY joy-rs/joyczl-protocol-noop-macros/Cargo.toml ./joy-rs/joyczl-protocol-noop-macros/
COPY joy-rs/joyczl-provider/Cargo.toml ./joy-rs/joyczl-provider/
COPY joy-rs/joyczl-state/Cargo.toml ./joy-rs/joyczl-state/
COPY joy-rs/joyczl-tools/Cargo.toml ./joy-rs/joyczl-tools/
RUN mkdir -p joy-rs/joyczl-cli/src && echo 'fn main() {}' > joy-rs/joyczl-cli/src/main.rs \
    && cargo build --release --manifest-path joy-rs/Cargo.toml -p joyczl-cli || true
# 再把真源码盖上去。
COPY joy-rs ./joy-rs
RUN touch joy-rs/joyczl-cli/src/main.rs \
    && cargo build --release --manifest-path joy-rs/Cargo.toml -p joyczl-cli

# ---- 运行 ---------------------------------------------------------------
FROM debian:bookworm-slim
# bubblewrap = Linux 上的沙箱后端。run_command 没它就不执行任何命令
# （这是刻意的：绝不在沙箱之外跑），所以想在容器里用执行工具就必须装它。
# ca-certificates 给 provider 与 MCP 的 HTTPS。
RUN apt-get update \
    && apt-get install -y --no-install-recommends bubblewrap ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/joy-rs/target/release/joy /usr/local/bin/joy

# 非 root 跑：容器里的提权没有任何理由。
RUN useradd --create-home --shell /bin/bash joy
USER joy
ENV JOY_HOME=/home/joy/.joy
VOLUME /home/joy/.joy
EXPOSE 7777

ENTRYPOINT ["joy"]
CMD ["dashboard"]

#
# 备份管理器：Rust 单二进制 + 网页配置。
#
# 构建阶段固定在 BUILDPLATFORM 上跑，按 TARGETARCH 交叉编译：
#   docker build --platform linux/arm64 .
# 这样 arm64 镜像不需要 QEMU 模拟编译，在 x86 机器上也能快速构建。

ARG BUILDER_BASE=rust:1-bookworm
ARG RUNTIME_BASE=debian:bookworm-slim

FROM --platform=$BUILDPLATFORM ${BUILDER_BASE} AS builder

ARG TARGETARCH
ARG CARGO_BUILD_JOBS=4
# 可选 crates.io 镜像，例如 sparse+https://rsproxy.cn/index/
ARG CARGO_REGISTRY_MIRROR=""

ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS} \
    CARGO_TERM_COLOR=always \
    CARGO_NET_RETRY=5

RUN if [ -n "${CARGO_REGISTRY_MIRROR}" ]; then \
        printf '[source.crates-io]\nreplace-with = "mirror"\n\n[source.mirror]\nregistry = "%s"\n' \
            "${CARGO_REGISTRY_MIRROR}" > /usr/local/cargo/config.toml; \
    fi

RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential pkg-config \
    && rm -rf /var/lib/apt/lists/*

# 按目标架构准备工具链：arm64 用交叉 gcc；protoc 由 protoc-bin-vendored 提供
# （构建机是 x86，proto 编译在构建阶段完成，不需要目标架构的 protoc）。
RUN case "${TARGETARCH}" in \
        amd64) \
            echo "x86_64-unknown-linux-gnu" > /rust-target ;; \
        arm64) \
            apt-get update && apt-get install -y --no-install-recommends \
                gcc-aarch64-linux-gnu libc6-dev-arm64-cross \
            && rm -rf /var/lib/apt/lists/* \
            && rustup target add aarch64-unknown-linux-gnu \
            && echo "aarch64-unknown-linux-gnu" > /rust-target ;; \
        *) \
            echo "不支持的架构: ${TARGETARCH}" >&2; exit 1 ;; \
    esac

ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc

WORKDIR /build
COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/build/target,sharing=locked \
    RUST_TARGET="$(cat /rust-target)" \
    && cargo build --locked --release --target "$RUST_TARGET" --bin quantumark \
    && install -Dm0755 "target/${RUST_TARGET}/release/quantumark" /usr/local/bin/quantumark

# `docker build --target binary --output ./out .` 只导出二进制
FROM scratch AS binary
COPY --from=builder /usr/local/bin/quantumark /quantumark

FROM ${RUNTIME_BASE} AS runtime

# 运行时需要的外部工具：
#   mariadb-client — mariadb-dump / mariadb 客户端
#   zstd           — 归档与转储压缩
#   age            — 归档与转储加密（也用来生成密钥对）
#   curl           — 健康检查
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl tzdata mariadb-client zstd age \
    && rm -rf /var/lib/apt/lists/*

# 不以 root 运行；默认用 NAS 上常见的第一个用户，可用构建参数覆盖。
ARG QM_UID=1000
ARG QM_GID=1000
RUN groupadd -g "${QM_GID}" qmgroup \
    && useradd -u "${QM_UID}" -g qmgroup -d /data -s /usr/sbin/nologin qmui \
    && install -d -m 0700 -o qmui -g qmgroup /data

COPY --from=builder /usr/local/bin/quantumark /usr/local/bin/quantumark

ENV QM_DATA_DIR=/data \
    QM_BIND=0.0.0.0:8790 \
    QM_LOG=quantumark=info

USER qmui
WORKDIR /data
EXPOSE 8790

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -fsS -o /dev/null "http://127.0.0.1:8790/health" || exit 1

ENTRYPOINT ["/usr/local/bin/quantumark"]

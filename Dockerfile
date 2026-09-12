# syntax=docker/dockerfile:1
#
# Distroless image for ores-otel-sidecar.
# Prefer linux/arm64:
#   docker buildx build --platform linux/arm64 -t ores-otel-sidecar:dev .
#
# This is the ores-otel loopback probe helper (127.0.0.1:9090 — /healthz
# /readyz /metrics), NOT an OTLP collector. Pair it in the same pod as the
# app. The app exports OTLP in-process to
# dd-otel-collector.observability.svc.cluster.local:4318 (HTTP) or :4317 (gRPC).
#
# No ores-sops in this image: secrets stay on the app container
# (env/enc + sops-entrypoint) or a k8s Secret. The native launcher logs through
# ores-otel and execs the application without adding a runtime shell.
#
# k8s contract (see ores-otel/ores-otel-sidecar.rs/k8s/container.yaml):
#   - bind ORES_OTEL_SIDECAR_BIND=127.0.0.1:9090 (loopback only)
#   - livenessProbe exec ["/ores-otel-sidecar", "probe"]
#   - no readinessProbe
#   - do not publish :9090 on a Service
#   - do not EXPOSE 4317/4318

# Build for the target architecture; do not force BUILDPLATFORM here.
FROM rust:1.90-bookworm AS launcher-build
WORKDIR /launcher-source
COPY docker/ores-launcher.rev ./ores-launcher.rev
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=cargo-registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,id=cargo-git,sharing=locked \
    grep -Eq '^[0-9a-f]{40}$' ores-launcher.rev \
    && test "$(wc -l < ores-launcher.rev)" -eq 1 \
    && cargo install --locked \
        --git https://github.com/ores-otel/ores.otel.log.git \
        --rev "$(cat ores-launcher.rev)" \
        --features launcher --bin ores-launcher --root /launcher \
        oresoftware-next-loggers \
    && strip /launcher/bin/ores-launcher

FROM rust:1.95-bookworm AS build
ARG TARGETARCH
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=cargo-registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,id=cargo-git,sharing=locked \
    --mount=type=cache,target=/src/target,id=ores-otel-sidecar-target-${TARGETARCH},sharing=locked \
    cargo test --all-targets --locked \
    && cargo build --release --locked --bin ores-otel-sidecar \
    && strip "target/release/ores-otel-sidecar" \
    && cp "target/release/ores-otel-sidecar" "/usr/local/bin/ores-otel-sidecar"

FROM gcr.io/distroless/cc-debian12:nonroot AS runtime
WORKDIR /
# Keep the application path stable for direct invocations and kubelet probes.
# The flags-2-env schema and sidecar runtime policy are immutable image inputs;
# no shell, curl, package manager, or writable root filesystem is required.
COPY --from=build --chown=65532:65532 "/usr/local/bin/ores-otel-sidecar" "/ores-otel-sidecar"
COPY --from=build --chown=65532:65532 --chmod=0444 "/src/.cli-flags.toml" "/.cli-flags.toml"
COPY --from=build --chown=65532:65532 --chmod=0444 "/src/.ores-sidecar.toml" "/.ores-sidecar.toml"
COPY --from=launcher-build --chmod=0555 /launcher/bin/ores-launcher /ores-launcher
ENV ORES_OTEL_SIDECAR_BIND=127.0.0.1:9090 \
    OTEL_SERVICE_NAME=ores-otel-sidecar
USER 65532:65532
ENTRYPOINT ["/ores-launcher", "/ores-otel-sidecar"]
CMD []

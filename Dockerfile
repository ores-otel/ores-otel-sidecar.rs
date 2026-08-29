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
# (env/enc + sops-entrypoint) or a k8s Secret. Distroless has no shell.
#
# k8s contract:
#   - bind ORES_OTEL_SIDECAR_BIND=127.0.0.1:9090 (loopback only)
#   - livenessProbe exec ["/usr/local/bin/ores-otel-sidecar", "probe"]
#   - no readinessProbe
#   - do not publish :9090 on a Service
#   - do not EXPOSE 4317/4318

FROM rust:1.90-bookworm AS build
ARG TARGETARCH
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=cargo-registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,id=cargo-git,sharing=locked \
    --mount=type=cache,target=/src/target,id=ores-otel-sidecar-target-${TARGETARCH},sharing=locked \
    cargo build --release --locked --bin ores-otel-sidecar \
    && strip "target/release/ores-otel-sidecar" \
    && cp "target/release/ores-otel-sidecar" "/usr/local/bin/ores-otel-sidecar"

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build --chown=65532:65532 "/usr/local/bin/ores-otel-sidecar" "/usr/local/bin/ores-otel-sidecar"
ENV ORES_OTEL_SIDECAR_BIND=127.0.0.1:9090 \
    ORES_OTEL_SIDECAR_BIND=127.0.0.1:9090 \
    OTEL_SERVICE_NAME=ores-otel-sidecar
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/ores-otel-sidecar"]

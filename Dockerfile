FROM rust:alpine AS builder

# Build-stage-only toolchain deps; versions track the rust:alpine base, so
# pinning (DL3018) would break on every base image refresh.
# hadolint ignore=DL3018
RUN apk add --no-cache musl-dev g++ make

WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
COPY src/ src/
COPY ui/dist/ ui/dist/

RUN cargo build --release

FROM scratch

COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
COPY --from=builder /app/target/release/featherbit /gateway
COPY config/ /etc/gateway/

EXPOSE 8080 9090

# Non-root (distroless "nonroot" uid); both listeners bind unprivileged ports.
USER 65532:65532

ENTRYPOINT ["/gateway"]
CMD ["--system-config", "/etc/gateway/system.yaml", "--gateway-config", "/etc/gateway/gateway.yaml"]

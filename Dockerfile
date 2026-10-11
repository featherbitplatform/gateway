FROM rust:bookworm AS builder

# The static-link check below pipes ldd into grep: fail on any stage (DL4006).
SHELL ["/bin/bash", "-o", "pipefail", "-c"]

# cargo-auditable embeds the crate dependency list into the binary; without it
# an SBOM of the FROM scratch runtime image would be nearly empty, and syft/
# grype/trivy could not inventory the shipped image (see docs.yml/docker.yml).
RUN cargo install cargo-auditable --locked

WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
COPY src/ src/
COPY ui/dist/ ui/dist/
# Docs pages embedded into the binary by src/mcp/docs.rs (rust-embed): served
# to agents as MCP resources in every build variant, headless included.
COPY website/docs/ website/docs/

# Headless variant: CARGO_FLAGS="--no-default-features --features mimalloc"
# compiles the UI out; keep `mimalloc`, which beats glibc's malloc by 8-24% at
# 4 cores (bench/gateways/featherbit-glibc-sysmalloc). Word-splitting of the
# flags is intentional.
ARG CARGO_FLAGS=""
# A fully static *glibc* binary (it replaced static musl: Luau scripting ~3x and
# 1k-route matching ~1.4x faster at 4 cores in the benchmark suite).
# - crt-static on the -gnu target links glibc statically; the explicit --target
#   keeps RUSTFLAGS off build scripts and proc-macros (which cannot be static).
# - Nothing in the tree dlopen()s: TLS is ring-only (no OpenSSL/aws-lc),
#   mimalloc and ring link statically, and Luau's libstdc++ is linked in.
# - The one runtime lookup is getaddrinfo() (upstream/redis/etcd hostnames):
#   glibc >= 2.34 builds the `files` and `dns` NSS backends into libc, so the
#   static binary resolves without libnss_*.so (the linker still warns).
# - rustc emits a static-PIE (ldd: "statically linked", exit 0; a non-PIE
#   static binary gets "not a dynamic executable", exit 1); the grep fails the
#   build if anything is still dynamic.
# `dist` = release + fat LTO + one codegen unit (Cargo.toml).
RUN TARGET="$(rustc -vV | sed -n 's/^host: //p')" \
 && RUSTFLAGS="-C target-feature=+crt-static -C link-arg=-static-libstdc++" \
    cargo auditable build --profile dist --target "$TARGET" ${CARGO_FLAGS} \
 && cp "target/$TARGET/dist/featherbit" /featherbit \
 && { ldd /featherbit 2>&1 || true; } | grep -qE "statically linked|not a dynamic executable" \
 && mkdir -p /rootfs/etc \
 && printf 'hosts: files dns\n' > /rootfs/etc/nsswitch.conf

FROM scratch

COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
# Without nsswitch.conf glibc's default hosts order is not `files dns`.
COPY --from=builder /rootfs/etc/nsswitch.conf /etc/
COPY --from=builder /featherbit /gateway
COPY config/ /etc/gateway/

EXPOSE 8080 9090

# Non-root (distroless "nonroot" uid); both listeners bind unprivileged ports.
USER 65532:65532

ENTRYPOINT ["/gateway"]
CMD ["--system-config", "/etc/gateway/system.yaml", "--gateway-config", "/etc/gateway/gateway.yaml"]

# Load generators, pinned: wrk (throughput ceiling), wrk2 (fixed-rate,
# coordinated-omission-corrected HdrHistogram latency), oha (HTTP/2, with
# --latency-correction). Also curl (upstream readiness) and openssl (bench
# certificates). linux/amd64 only: wrk2's bundled LuaJIT does not build on arm64.
FROM debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251 AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends build-essential git ca-certificates libssl-dev zlib1g-dev unzip \
 && rm -rf /var/lib/apt/lists/*
ARG WRK_REF=4.2.0
ARG WRK2_REF=44a94c17d8e6a0bac8559b53da76848e430cb7a7
RUN git clone https://github.com/wg/wrk.git /src/wrk \
 && git -C /src/wrk checkout "${WRK_REF}" \
 && make -C /src/wrk -j"$(nproc)" WITH_OPENSSL=/usr \
 && cp /src/wrk/wrk /usr/local/bin/wrk
# wrk2 times requests with gettimeofday(); a backwards wall-clock step (host time
# sync, routine under WSL2/VMs) underflows its unsigned latency and aborts on an
# HdrHistogram assertion. Latency must come from a monotonic clock: patch time_us().
RUN git clone https://github.com/giltene/wrk2.git /src/wrk2 \
 && git -C /src/wrk2 checkout "${WRK2_REF}" \
 && sed -i '1i #include <time.h>' /src/wrk2/src/wrk.c \
 && sed -i '/^static uint64_t time_us() {/,/^}/c\static uint64_t time_us() {\n    struct timespec t;\n    clock_gettime(CLOCK_MONOTONIC, \&t);\n    return ((uint64_t) t.tv_sec * 1000000) + (t.tv_nsec / 1000);\n}' /src/wrk2/src/wrk.c \
 && grep -q CLOCK_MONOTONIC /src/wrk2/src/wrk.c \
 && make -C /src/wrk2 -j"$(nproc)" \
 && cp /src/wrk2/wrk /usr/local/bin/wrk2

FROM debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl libssl3 openssl zlib1g \
 && rm -rf /var/lib/apt/lists/*
ARG OHA_VERSION=1.16.0
ADD https://github.com/hatoo/oha/releases/download/v${OHA_VERSION}/oha-linux-amd64 /usr/local/bin/oha
RUN chmod 0755 /usr/local/bin/oha
COPY --from=build /usr/local/bin/wrk /usr/local/bin/wrk2 /usr/local/bin/
# Nothing here needs root: the tools only open client sockets, and certificate
# generation writes to /tmp. (docker run --ulimit raises nofile for any user.)
USER 65534:65534
CMD ["sleep", "infinity"]

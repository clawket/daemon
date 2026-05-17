# syntax=docker/dockerfile:1.7

# Build stage — Debian slim (glibc). musl/alpine fails to compile sqlite-vec
# because the bundled C source uses BSD-style integer aliases (u_int8_t,
# u_int16_t, u_int64_t) that musl does not export. Switching to glibc keeps
# the build clean and the runtime image small via distroless/cc.
FROM rust:1-slim-bookworm AS builder
WORKDIR /build
RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config build-essential \
 && rm -rf /var/lib/apt/lists/*

# Layer-cache dependencies before bringing in source.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
 && echo 'fn main() {}' > src/main.rs \
 && cargo build --release --locked \
 && rm -rf src target/release/clawketd target/release/deps/clawketd*

COPY src ./src
COPY migrations ./migrations
COPY schemas ./schemas
RUN cargo build --release --locked \
 && strip target/release/clawketd

# Runtime stage — distroless/cc carries glibc + a minimal CA bundle and
# nothing else (no shell, no package manager). Image stays under ~30 MiB.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /build/target/release/clawketd /usr/local/bin/clawketd

# Container-friendly XDG paths under /data — mount this as a volume to
# persist the SQLite database, vector index, and daemon state across
# restarts. HOME is set so any unforced fallback also lands under /data.
ENV CLAWKET_DATA_DIR=/data/share \
    CLAWKET_CACHE_DIR=/data/cache \
    CLAWKET_CONFIG_DIR=/data/config \
    CLAWKET_STATE_DIR=/data/state \
    HOME=/data

VOLUME ["/data"]
EXPOSE 19400

ENTRYPOINT ["/usr/local/bin/clawketd"]
CMD ["start", "--host", "0.0.0.0", "--port", "19400"]

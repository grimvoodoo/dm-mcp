# syntax=docker/dockerfile:1.7

# ─── STAGE 1: Build dm-mcp as a statically-linked musl binary ────────────────
FROM rust:1 AS builder

# musl toolchain. rusqlite's "bundled" feature compiles SQLite from C source,
# so the musl C compiler is required for the x86_64-unknown-linux-musl target.
RUN apt-get update \
    && apt-get install -y --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/*

RUN rustup target add x86_64-unknown-linux-musl

WORKDIR /app

# Single-crate project, so no workspace cache dance. `content/` is embedded
# into the binary at compile time via include_dir!, so it must be present
# here — but it is NOT needed in the final image.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY content ./content

RUN cargo build --release --target x86_64-unknown-linux-musl --bin dm-mcp

# ─── STAGE 2: scratch runtime ────────────────────────────────────────────────
FROM scratch

COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/dm-mcp /dm-mcp

# HTTP transport binds 0.0.0.0:3000 by default and exposes /healthz. Override
# with DMMCP_HTTP_BIND / DMMCP_HTTP_PORT. stdio transport does not use the
# network.
EXPOSE 3000

# One campaign per process — mount a volume for the SQLite file and point
# DMMCP_DB_PATH at it (default is ./campaign.db, which will be inside the
# container's writable layer and lost on restart).

# Default to HTTP for k8s / networked deploys. Override with `stdio` for
# local DM-agent subprocess use.
ENTRYPOINT ["/dm-mcp"]
CMD ["http"]

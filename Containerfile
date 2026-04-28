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

# COPY --chown lets the binary be readable+executable by the non-root UID
# below even though scratch has no /etc/passwd to resolve a symbolic name.
# 65532 is the conventional non-root UID/GID used by distroless and ubi-micro
# images — sticking with that number keeps mounted-volume permissions
# predictable across the ecosystem.
COPY --from=builder --chown=65532:65532 /app/target/x86_64-unknown-linux-musl/release/dm-mcp /dm-mcp

# Drop root. A compromise of the dm-mcp process can no longer trivially write
# arbitrary container paths or attempt mount-namespace tricks. Volumes mounted
# for the campaign DB must be writable by UID 65532 — the README "Run" section
# documents this.
USER 65532:65532

# HTTP transport binds 0.0.0.0:3000 by default and exposes /healthz. Override
# with DMMCP_HTTP_BIND / DMMCP_HTTP_PORT. stdio transport does not use the
# network.
EXPOSE 3000

# The `healthcheck` subcommand opens a TCP connection + GETs /healthz on the
# configured bind+port and exits 0 on 200, non-zero otherwise. scratch has no
# shell, so a binary subcommand is the only viable HEALTHCHECK path. Tunable
# from the docker run side via --health-interval / --health-retries.
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD ["/dm-mcp", "healthcheck"]

# One campaign per process — mount a volume for the SQLite file and point
# DMMCP_DB_PATH at it (default is ./campaign.db, which will be inside the
# container's writable layer and lost on restart). Volume must be writable
# by UID 65532.

# Default to HTTP for k8s / networked deploys. Override with `stdio` for
# local DM-agent subprocess use.
ENTRYPOINT ["/dm-mcp"]
CMD ["http"]

# ─── dm-mcp Development Makefile ─────────────────────────────────────────────
#
#   make dev         Build + start the server over HTTP for manual testing
#   make run-stdio   Attach the stdio transport to this terminal
#   make demo        Walkthrough: spawn stdio, handshake, list tools, call server.info
#   make clean       Stop the dev server, wipe build artefacts and logs
#   make logs        Tail the dev server log
#   make test        Run the full automated test suite (cargo test)
#   make check       Run all CI quality gates locally — matches ci.yaml
#   make audit       cargo audit + verify rustls-only — matches commit.yaml
#   make build       Release build (scratch-container target)
#   make reset       Wipe the campaign SQLite file for a fresh run
#   make health      Smoke test: curl /healthz on the dev server
#
# Requires: cargo, curl
# ─────────────────────────────────────────────────────────────────────────────

# ── Runtime config (mirrors the DMMCP_ env var contract; see README) ──────────
HTTP_BIND   := 127.0.0.1
HTTP_PORT   := 3000
DB_PATH     := ./campaign.db
LOG_LEVEL   := info

# ── Directory layout ──────────────────────────────────────────────────────────
LOG_DIR     := .logs
PID_DIR     := .pids

# ─────────────────────────────────────────────────────────────────────────────
.PHONY: dev run-stdio demo clean logs test check audit build reset health \
        _build-debug _service-start _service-stop _sweep-stale \
        _check-fmt _check-clippy _check-no-openssl

# ─────────────────────────────────────────────────────────────────────────────
## dev: Start the HTTP transport for manual testing
##      Builds the debug binary, spawns it in the background on HTTP_PORT,
##      waits for /healthz to respond, and hands control back. Use an MCP
##      client (Claude / Cursor / MCP Inspector) pointed at /mcp to exercise
##      tool calls.
# ─────────────────────────────────────────────────────────────────────────────
dev: _build-debug _service-start
	@echo ""
	@echo "┌─────────────────────────────────────────────┐"
	@echo "│  dm-mcp dev server is running               │"
	@echo "│                                             │"
	@echo "│  Transport  →  HTTP                         │"
	@echo "│  Bind       →  http://$(HTTP_BIND):$(HTTP_PORT)         │"
	@echo "│  MCP path   →  /mcp                         │"
	@echo "│  Probe      →  /healthz                     │"
	@echo "│  Campaign   →  $(DB_PATH)                 │"
	@echo "│                                             │"
	@echo "│  make health  smoke-test /healthz           │"
	@echo "│  make logs    tail the server log           │"
	@echo "│  make clean   stop + wipe artefacts         │"
	@echo "└─────────────────────────────────────────────┘"

# ─────────────────────────────────────────────────────────────────────────────
## run-stdio: Attach the stdio transport to this terminal
##      Useful when piping MCP JSON-RPC frames by hand, or when a local client
##      (e.g. Claude Code) is configured to spawn dm-mcp as a child process.
##      Ctrl-C or stdin EOF to stop.
# ─────────────────────────────────────────────────────────────────────────────
run-stdio:
	@DMMCP_DB_PATH=$(DB_PATH) \
	 DMMCP_LOG_LEVEL=$(LOG_LEVEL) \
	 cargo run --quiet -- stdio

# ─────────────────────────────────────────────────────────────────────────────
## demo: Manual-test walkthrough — spawn stdio, handshake, call server.info
##      Builds the debug binary, then runs examples/manual_demo, which acts as
##      a real MCP client:
##        1. spawns dm-mcp as an MCP stdio child process
##        2. completes the MCP handshake
##        3. lists the registered tools
##        4. calls server.info and pretty-prints the JSON response
##        5. cancels the session cleanly
##      Every step prints what the client sees — use this when you want to
##      eyeball the server's responses without running the full test suite.
##
##      To see the raw JSON-RPC frames on stderr:
##        RUST_LOG=rmcp=trace make demo
# ─────────────────────────────────────────────────────────────────────────────
demo: _build-debug
	@cargo run --quiet --example manual_demo

# ─────────────────────────────────────────────────────────────────────────────
## clean: Stop the dev server and wipe build artefacts
##      Does not drop the campaign database. Use `make reset` for that.
# ─────────────────────────────────────────────────────────────────────────────
clean: _service-stop
	@cargo clean
	@rm -rf -- "$(LOG_DIR)" "$(PID_DIR)"
	@echo "✓ Cleaned"

# ─────────────────────────────────────────────────────────────────────────────
## logs: Tail the dev server log
# ─────────────────────────────────────────────────────────────────────────────
logs:
	@if [ ! -f "$(LOG_DIR)/dm-mcp.log" ]; then \
		echo "No log file at $(LOG_DIR)/dm-mcp.log"; \
		echo "Run 'make dev' first."; \
		exit 1; \
	fi
	@echo "Tailing: $(LOG_DIR)/dm-mcp.log"
	@tail -f $(LOG_DIR)/dm-mcp.log

# ─────────────────────────────────────────────────────────────────────────────
## test: Run the full test suite
##      Unit tests (src/**/tests) + integration tests (tests/*.rs — spawn
##      the compiled binary and drive it through real MCP protocol over
##      stdio and HTTP). No external services.
# ─────────────────────────────────────────────────────────────────────────────
test:
	@echo "→ Running tests (unit + integration)..."
	@cargo test
	@echo "✓ Tests passed"

# ─────────────────────────────────────────────────────────────────────────────
## check: Run all CI quality gates locally
##      Mirrors .github/workflows/ci.yaml so if this passes, CI will pass.
##      Run before raising a PR.
##
##      Steps (in order):
##        1. cargo fmt --all -- --check
##        2. cargo clippy --all-targets -- -D warnings
##        3. cargo tree -i openssl|openssl-sys|native-tls (must be empty)
##        4. cargo test
##
##      Release build (cargo build --release) is verified in CI but not
##      here — it costs minutes on a Pi. Run `make build` explicitly if you
##      want to reproduce that locally.
# ─────────────────────────────────────────────────────────────────────────────
check: _check-fmt _check-clippy _check-no-openssl test
	@echo ""
	@echo "┌─────────────────────────────────────────────┐"
	@echo "│  All quality gates passed.                  │"
	@echo "│  This branch is ready to PR.                │"
	@echo "└─────────────────────────────────────────────┘"

# ─────────────────────────────────────────────────────────────────────────────
## audit: Security audit — matches .github/workflows/commit.yaml
##      Re-verifies the rustls-only rule and runs `cargo audit` against the
##      RustSec advisory database. Installs cargo-audit on demand; a cold
##      install is slow on ARM (~30 min). CI caches the binary.
# ─────────────────────────────────────────────────────────────────────────────
audit: _check-no-openssl
	@echo "→ Running cargo audit..."
	@command -v cargo-audit > /dev/null || { \
		echo "  cargo-audit not found — installing (this may take a while)..."; \
		cargo install cargo-audit --locked; \
	}
	@cargo audit
	@echo "✓ No advisories"

# ─────────────────────────────────────────────────────────────────────────────
## build: Release build
##      Statically-linked binary suitable for the scratch container image.
##      See target/release/dm-mcp after the build completes.
# ─────────────────────────────────────────────────────────────────────────────
build:
	@echo "→ Building release binary..."
	@cargo build --release
	@echo "✓ $$(ls -lh target/release/dm-mcp | awk '{print $$5, $$9}')"

# ─────────────────────────────────────────────────────────────────────────────
## reset: Wipe the campaign database
##      Stops the dev server if running, removes $(DB_PATH) and its WAL
##      sidecars, leaves everything else intact.
##
##      CAUTION: destroys all in-campaign state.
# ─────────────────────────────────────────────────────────────────────────────
reset: _service-stop
	@echo "→ Removing $(DB_PATH) (+ WAL sidecars)..."
	@rm -f -- "$(DB_PATH)" "$(DB_PATH)-wal" "$(DB_PATH)-shm"
	@echo "✓ Campaign database wiped"
	@echo "  Run 'make dev' to start with a fresh campaign."

# ─────────────────────────────────────────────────────────────────────────────
## health: GET /healthz on the dev server
##      Smoke test that the HTTP transport is up and answering.
# ─────────────────────────────────────────────────────────────────────────────
health:
	@echo "→ GET http://$(HTTP_BIND):$(HTTP_PORT)/healthz"
	@curl -fsS http://$(HTTP_BIND):$(HTTP_PORT)/healthz || { \
		echo ""; \
		echo "✗ /healthz failed. Is 'make dev' running?"; \
		exit 1; \
	}
	@echo ""
	@echo "✓ /healthz OK"

# ══════════════════════════════════════════════════════════════════════════════
#  Internal helper targets (not intended to be called directly)
# ══════════════════════════════════════════════════════════════════════════════

_build-debug:
	@echo "→ Building debug binary..."
	@cargo build --quiet
	@echo "✓ Build complete"

# ── Service management ────────────────────────────────────────────────────────
#
# Single binary, single port. PID is written to $(PID_DIR)/dm-mcp.pid and
# stdout/stderr go to $(LOG_DIR)/dm-mcp.log. Start sweeps any stale process
# from this checkout before spawning (see _sweep-stale).

_service-start: _sweep-stale
	@mkdir -p $(LOG_DIR) $(PID_DIR)
	@echo "→ Starting dm-mcp on http://$(HTTP_BIND):$(HTTP_PORT)..."
	@DMMCP_HTTP_BIND="$(HTTP_BIND)" \
	 DMMCP_HTTP_PORT="$(HTTP_PORT)" \
	 DMMCP_DB_PATH="$(DB_PATH)" \
	 DMMCP_LOG_LEVEL="$(LOG_LEVEL)" \
	 ./target/debug/dm-mcp http > $(LOG_DIR)/dm-mcp.log 2>&1 & \
	 echo $$! > $(PID_DIR)/dm-mcp.pid
	@printf "  Waiting for /healthz"
	@for i in $$(seq 1 50); do \
		if curl -sf http://$(HTTP_BIND):$(HTTP_PORT)/healthz > /dev/null 2>&1; then \
			echo " ready"; \
			echo "  pid: $$(cat $(PID_DIR)/dm-mcp.pid) — log: $(LOG_DIR)/dm-mcp.log"; \
			exit 0; \
		fi; \
		if ! kill -0 "$$(cat $(PID_DIR)/dm-mcp.pid)" 2>/dev/null; then \
			echo ""; \
			echo "✗ dm-mcp exited before /healthz came up. Last log lines:"; \
			tail -n 20 $(LOG_DIR)/dm-mcp.log; \
			rm -f $(PID_DIR)/dm-mcp.pid; \
			exit 1; \
		fi; \
		printf '.'; sleep 0.2; \
	done; \
	echo ""; \
	echo "✗ Server did not come up within 10s. See $(LOG_DIR)/dm-mcp.log"; \
	exit 1

_service-stop: _sweep-stale
	@if [ -f "$(PID_DIR)/dm-mcp.pid" ]; then \
		pid=$$(cat "$(PID_DIR)/dm-mcp.pid"); \
		if kill -0 "$$pid" 2>/dev/null; then \
			kill "$$pid" && echo "✓ Stopped dm-mcp (pid: $$pid)"; \
		fi; \
		rm -f "$(PID_DIR)/dm-mcp.pid"; \
	fi

# Catch-all: kill any dm-mcp binary from THIS checkout left behind by an earlier
# session (crash, SSH disconnect, $(PID_DIR) wiped). Match by process name (-x
# means exact — pgrep doesn't see the make subshell), then filter by
# /proc/<pid>/cwd so we never touch a dm-mcp started from a different checkout
# on a shared machine. SIGTERM → wait 1s → SIGKILL any holdouts. Silent when
# nothing is running. Linux-only (uses /proc).
_sweep-stale:
	@candidates=$$(pgrep -x dm-mcp 2>/dev/null || true); \
	pids=""; \
	for pid in $$candidates; do \
		cwd=$$(readlink /proc/$$pid/cwd 2>/dev/null || true); \
		if [ "$$cwd" = "$(CURDIR)" ]; then \
			pids="$$pids $$pid"; \
		fi; \
	done; \
	pids=$$(echo $$pids | xargs); \
	if [ -n "$$pids" ]; then \
		echo "→ Sweeping stale dm-mcp processes: $$pids"; \
		kill $$pids 2>/dev/null || true; \
		sleep 1; \
		stubborn=""; \
		for pid in $$pids; do \
			if kill -0 $$pid 2>/dev/null; then stubborn="$$stubborn $$pid"; fi; \
		done; \
		stubborn=$$(echo $$stubborn | xargs); \
		if [ -n "$$stubborn" ]; then \
			echo "  forcing: $$stubborn"; \
			kill -9 $$stubborn 2>/dev/null || true; \
		fi; \
		echo "✓ Stale processes cleared"; \
	fi

# ── Quality gates (matches .github/workflows/ci.yaml) ─────────────────────────

_check-fmt:
	@echo "→ Checking formatting..."
	@cargo fmt --all -- --check || { \
		echo ""; \
		echo "✗ Formatting check failed."; \
		echo "  Run: cargo fmt --all"; \
		exit 1; \
	}
	@echo "✓ Formatting OK"

_check-clippy:
	@echo "→ Running clippy (warnings as errors)..."
	@cargo clippy --all-targets -- -D warnings || { \
		echo ""; \
		echo "✗ Clippy check failed."; \
		echo "  Fix all warnings before raising a PR."; \
		exit 1; \
	}
	@echo "✓ Clippy OK"

_check-no-openssl:
	@echo "→ Verifying rustls-only TLS (no openssl / openssl-sys / native-tls)..."
	@fail=0; \
	for crate in openssl openssl-sys native-tls; do \
		if cargo tree -i "$$crate" > /dev/null 2>&1; then \
			echo "  ✗ $$crate is in the dependency tree:"; \
			cargo tree -i "$$crate" | sed 's/^/    /'; \
			fail=1; \
		fi; \
	done; \
	if [ "$$fail" = "1" ]; then \
		echo ""; \
		echo "✗ OpenSSL / native-tls detected. Project rule: rustls only."; \
		echo "  Swap the offender for a rustls variant, or disable its default features."; \
		exit 1; \
	fi
	@echo "✓ No OpenSSL / native-tls in tree"

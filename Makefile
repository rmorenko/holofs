# holofs — dev cluster harness.
#
# One-shot developer + tester workflow: `make dev` kills any prior
# daemon, wipes storage, rebuilds the release binary if it's stale,
# starts a fresh 40-node embedded cluster on 127.0.0.1:8787, and
# populates it with a variety of test data — simple synthetic
# images, real 512×512 photos from picsum.photos, short + longer
# audio, opaque binary blobs, and a handful of loose files at the
# catalog root.
#
# Individual steps are also targets (see `make help`), so testers
# can e.g. re-seed without restarting or tail logs while the
# daemon runs.
#
# Overrides:
#   make dev STORAGE=/other/dir PORT=8788
#   make dev BINARY=/some/other/path/holofs-web

STORAGE ?= /tmp/holofs-dev-storage
FETCH   ?= /tmp/holofs-fetch
PORT    ?= 8787
BASE    := http://127.0.0.1:$(PORT)
BINARY  ?= ./target/release/holofs-web
LOG     := /tmp/holofs-dev.log
PID     := /tmp/holofs-dev.pid

.DEFAULT_GOAL := help

.PHONY: help
help:  ## show this help
	@printf "\033[1mholofs dev cluster targets\033[0m\n\n"
	@awk 'BEGIN{FS=":.*?## "} /^[a-zA-Z_-]+:.*?## / {printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@echo ""
	@echo "  BASE = $(BASE), STORAGE = $(STORAGE)"

.PHONY: dev
dev: dev-kill dev-clean dev-start dev-seed dev-status  ## one-shot: kill + clean + start + populate + report

.PHONY: dev-kill
dev-kill:  ## kill any running holofs-web daemon
	@if pgrep -f "release/holofs-web" > /dev/null 2>&1; then \
	    echo "[dev-kill] stopping daemon"; \
	    pkill -f "release/holofs-web" || true; \
	    sleep 3; \
	    if pgrep -f "release/holofs-web" > /dev/null 2>&1; then \
	        echo "[dev-kill] still alive, SIGKILL"; \
	        pkill -9 -f "release/holofs-web" || true; \
	        sleep 1; \
	    fi; \
	fi
	@rm -f $(PID)

.PHONY: dev-clean
dev-clean:  ## wipe $(STORAGE) and the download scratch dir
	@rm -rf $(STORAGE) $(FETCH)
	@mkdir -p $(STORAGE) $(FETCH)
	@echo "[dev-clean] wiped $(STORAGE) and $(FETCH)"

.PHONY: dev-build
dev-build:  ## rebuild the release binary if missing
	@if [ ! -x $(BINARY) ]; then \
	    echo "[dev-build] release binary missing — building (~2.5 min)"; \
	    cargo build --release --features ssr --bin holofs-web; \
	fi

.PHONY: dev-start
dev-start: dev-build  ## start daemon in background, wait for /api/stats to answer
	@echo "[dev-start] launching daemon on $(BASE)"
	@HOLOFS_STORAGE_DIR=$(STORAGE) \
	    HOLOFS_LOG=warn HOLOFS_LOG_FORMAT=text \
	    HOLOFS_ADMIN_UNAUTHENTICATED=1 \
	    HOLOFS_NO_SEED=true \
	    LEPTOS_SITE_ADDR=127.0.0.1:$(PORT) \
	    nohup $(BINARY) --enable-embed --enable-versions \
	    > $(LOG) 2>&1 & echo $$! > $(PID)
	@for i in 1 2 3 4 5 6 7 8 9 10; do \
	    if curl -sSf $(BASE)/api/stats > /dev/null 2>&1; then \
	        echo "[dev-start] up after $${i}s (pid $$(cat $(PID)))"; \
	        exit 0; \
	    fi; \
	    sleep 1; \
	done; \
	echo "[dev-start] daemon didn't come up within 10s"; \
	tail -30 $(LOG); \
	exit 1

.PHONY: dev-seed
dev-seed:  ## populate the running cluster with a variety of test data
	@BASE=$(BASE) FETCH=$(FETCH) SAMPLE_PNG=$(CURDIR)/assets/sample.png \
	    bash $(CURDIR)/deploy/dev-seed.sh

.PHONY: dev-stop
dev-stop: dev-kill  ## alias for dev-kill

.PHONY: dev-status
dev-status:  ## show /api/stats and pid
	@echo "[dev-status] cluster: $(BASE)"
	@if [ -f $(PID) ]; then echo "  pid: $$(cat $(PID))"; fi
	@printf "  stats: "
	@curl -sS $(BASE)/api/stats 2>/dev/null | head -c 400 || echo "(unreachable)"
	@echo ""

.PHONY: dev-logs
dev-logs:  ## tail the daemon log
	@tail -f $(LOG)

.PHONY: dev-open
dev-open:  ## open the web UI in the default browser (macOS)
	@open $(BASE) || xdg-open $(BASE) || echo "$(BASE)"

# --- style / lint --------------------------------------------------------
# Non-Rust style linters. Rust code is covered by `cargo fmt` +
# `cargo clippy` in CI; these catch prose, TOML, shell drift.
#
#   brew install typos-cli markdownlint-cli2 taplo shellcheck
SHELL_SCRIPTS := deploy/dev-seed.sh scripts/spawn-cluster.sh \
                 tools/test-data/run-tests.sh \
                 tools/test-data/fetch-real-landscapes.sh \
                 tools/test-data/clean-cluster.sh \
                 tools/test-data/upload-samples.sh \
                 crates/holofs-e2e/scripts/run-tests.sh

.PHONY: lint
lint: lint-typos lint-md lint-toml lint-sh  ## run all non-Rust style linters

.PHONY: lint-typos
lint-typos:  ## spellcheck source + docs (config: _typos.toml)
	@typos

.PHONY: lint-md
lint-md:  ## Markdown style (config: .markdownlint.jsonc)
	@markdownlint-cli2

.PHONY: lint-toml
lint-toml:  ## TOML formatting (config: .taplo.toml)
	@taplo fmt --check

.PHONY: lint-sh
lint-sh:  ## shell script analysis
	@shellcheck $(SHELL_SCRIPTS)

.PHONY: lint-fix
lint-fix:  ## apply auto-fixes for typos + TOML formatting
	@typos --write-changes || true
	@taplo fmt

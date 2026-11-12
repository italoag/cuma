# CUMA — a universal control plane for coding agents.
#
# `make help` lists every target. Sections:
#   build & run · verification (what CI runs) · live sandbox tests
#   skills signing · install · misc

BINARY := cuma
CLI    := crates/cuma-cli
MSRV   := 1.88.0

# Extra arguments for `make run`, `make setup`, etc.
ARGS   ?=

# --- skills signing -------------------------------------------------------
# `make skill-sign DIR=skills/foo KEY_ID=me KEY_FILE=/secure/sk.key`
DIR      ?=
KEY_ID   ?=
KEY_FILE ?=

# --- live sandbox tests ---------------------------------------------------
# Each needs its runtime and its CUMA_LIVE_* variables; unset means skipped.
LIVE_TESTS := docker microsandbox wasmer agentos kubernetes opensandbox e2b arcbox firecracker

.PHONY: all help \
	build release run setup doctor doc \
	check ci fmt fmt-check lint lint-otel test test-crate bench bench-check \
	msrv actionlint cover \
	live $(addprefix live-,$(LIVE_TESTS)) \
	skill-keygen skill-sign skill-verify \
	install reinstall clean legacy-build

all: check

help: ## List every target with its description.
	@grep -hE '^[a-zA-Z0-9_-]+:.*?## ' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-18s\033[0m %s\n", $$1, $$2}'

# --- build & run ----------------------------------------------------------

build: ## Debug build, the whole workspace.
	cargo build --workspace

release: ## Optimized build, the whole workspace.
	cargo build --release --workspace

run: build ## Run the CLI: `make run ARGS='doctor'`.
	./target/debug/$(BINARY) $(ARGS)

setup: build ## Configure CUMA interactively (`make setup ARGS='--yes --project'`).
	./target/debug/$(BINARY) setup $(ARGS)

doctor: build ## Check the installation.
	./target/debug/$(BINARY) doctor

doc: ## Build the crate documentation.
	cargo doc --workspace --no-deps --document-private-items

# --- verification: what CI runs --------------------------------------------

check: fmt-check lint test lint-otel bench-check ## Everything .github/workflows/ci.yml gates on.
ci: check msrv actionlint ## CI parity, including MSRV and workflow lint.

fmt: ## Format everything.
	cargo fmt --all

fmt-check: ## Fail if anything is unformatted.
	cargo fmt --all -- --check

lint: ## Clippy, warnings denied — run `rustup update` first, CI uses latest stable.
	cargo clippy --workspace --all-targets -- -D warnings

lint-otel: ## Clippy with the optional `otel` feature enabled.
	cargo clippy -p cuma-cli --features otel --all-targets -- -D warnings

test: ## The whole test suite.
	cargo test --workspace

test-crate: ## One crate: `make test-crate CRATE=cuma-router` (ARGS for a filter).
ifeq ($(CRATE),)
	@echo "usage: make test-crate CRATE=<name> [ARGS=<filter>]" >&2; exit 2
else
	cargo test -p $(CRATE) $(ARGS)
endif

bench: ## Run the benchmarks (harness, store, integrity).
	cargo bench --workspace

bench-check: ## CI only compiles the benches — shared runners make timings noise.
	cargo bench --workspace --no-run

msrv: ## Prove the workspace builds on its declared MSRV.
	cargo +$(MSRV) check --workspace

actionlint: ## Lint the GitHub workflows (needs `actionlint` on PATH).
	actionlint

cover: ## Coverage via cargo-llvm-cov (`cargo install cargo-llvm-cov`).
	cargo llvm-cov --workspace

# --- live sandbox tests -----------------------------------------------------
# A real agent in a real sandbox: a turn is completed inside it and its work
# lands back in the workspace. Without the variables the test passes having
# done nothing, so `make live` is always safe — it just exercises less.
# Variables per kind: docs/DEVELOPMENT.md#live-sandbox-tests.

live: $(addprefix live-,$(LIVE_TESTS)) ## Every live sandbox test its environment supports.

live-docker: ## CUMA_LIVE_DOCKER_IMAGE — an agent turn in a container.
	cargo test -p cuma-sandbox --test live_docker

live-microsandbox: ## CUMA_LIVE_MICROSANDBOX_IMAGE — `msb` microVMs.
	cargo test -p cuma-sandbox --test live_microsandbox

live-wasmer: ## CUMA_LIVE_WASMER_PACKAGE (e.g. wasmer/bash) [+ _PROGRAM].
	cargo test -p cuma-sandbox --test live_wasmer

live-agentos: ## CUMA_LIVE_AGENTOS_MODULES — AgentOS V8 sandbox.
	cargo test -p cuma-sandbox --test live_agentos

live-kubernetes: ## CUMA_LIVE_KUBERNETES_IMAGE/_NAMESPACE/_CONTEXT.
	cargo test -p cuma-sandbox --test live_kubernetes

live-opensandbox: ## CUMA_LIVE_OPENSANDBOX_URL/_IMAGE/_KEY_REF + CUMA_LIVE_CUMA_BIN.
	cargo test -p cuma-sandbox --test live_opensandbox

live-e2b: ## CUMA_LIVE_E2B_* + CUMA_LIVE_CUMA_BIN — CubeSandbox or E2B.
	cargo test -p cuma-sandbox --test live_e2b

live-arcbox: ## CUMA_LIVE_ARCBOX_IMAGE [+ _PROGRAM] — macOS virtualization.
	cargo test -p cuma-sandbox --test live_arcbox

live-firecracker: ## CUMA_LIVE_FIRECRACKER_KERNEL/_ROOTFS/_BIN — needs Linux+KVM.
	cargo test -p cuma-sandbox --test live_firecracker

# --- skills signing ---------------------------------------------------------
# ed25519 integrity for published skills: keygen prints a key id and writes
# no files; keep the base64 secret in a file outside the repository.

skill-keygen: ## Generate a signing key pair (prints key id + base64 secret).
	cargo run -p cuma-cli -- skills keygen

skill-sign: ## Sign a skill: `make skill-sign DIR=<dir> KEY_ID=<id> KEY_FILE=<file>`.
ifeq ($(and $(DIR),$(KEY_ID),$(KEY_FILE)),)
	@echo "usage: make skill-sign DIR=<dir> KEY_ID=<id> KEY_FILE=<file>" >&2; exit 2
else
	cargo run -p cuma-cli -- skills sign $(DIR) --key-id $(KEY_ID) --key-file $(KEY_FILE)
endif

skill-verify: ## Verify a skill's digest and signature: `make skill-verify DIR=<dir>`.
ifeq ($(DIR),)
	@echo "usage: make skill-verify DIR=<dir>" >&2; exit 2
else
	cargo run -p cuma-cli -- skills verify $(DIR)
endif

# --- install & misc ---------------------------------------------------------

install: ## Install `cuma` into cargo's bin dir.
	cargo install --path $(CLI)

reinstall: ## Install over the existing binary.
	cargo install --path $(CLI) --force

clean: ## Remove all build artifacts.
	cargo clean

# The previous Go product, preserved under legacy/ and still buildable.
legacy-build: ## Build the Go scanner preserved under legacy/.
	cd legacy && go build ./...

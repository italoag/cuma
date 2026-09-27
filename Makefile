BINARY := cuma

.PHONY: all build release run test cover lint fmt fmt-check check clean doc install legacy-build

all: check

build:
	cargo build --workspace

release:
	cargo build --release --workspace

run: build
	./target/debug/$(BINARY) $(ARGS)

test:
	cargo test --workspace

lint:
	cargo clippy --workspace --all-targets -- -D warnings

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

# What CI runs, and what to run before pushing.
check: fmt-check lint test

doc:
	cargo doc --workspace --no-deps --document-private-items

install:
	cargo install --path crates/cuma-cli

clean:
	cargo clean

# The previous Go product, preserved under legacy/ and still buildable.
legacy-build:
	cd legacy && go build ./...

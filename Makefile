.PHONY: check
check:
	nix fmt -- --ci
	cargo clippy --all-targets -- -D warnings
	cargo test

.PHONY: fmt
fmt:
	nix fmt

.PHONY: build
build:
	cargo build --release

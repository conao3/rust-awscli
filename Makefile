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

.PHONY: difftest
difftest:
	cargo build
	cargo run -q -p awsr-difftest -- list difftest/smoke.txt

.PHONY: difftest-full
difftest-full:
	cargo build --release
	cargo run -q --release -p awsr-difftest -- corpus --awsr-bin target/release/awsr --seeds 4 --formats json,text,table

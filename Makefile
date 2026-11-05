.PHONY: check
check:
	nix fmt -- --ci
	cargo clippy --all-targets -- -D warnings
	cargo test

.PHONY: fmt
fmt:
	nix fmt

.PHONY: help-pages
help-pages:
	cargo build --release -p awsr-helpgen
	env -i PATH="$$PATH" HOME="$$(mktemp -d)" target/release/awsr-helpgen target/help-pages.bin

.PHONY: build
build: help-pages
	AWSR_HELP_PAGES=target/help-pages.bin cargo build --release -p awsr

.PHONY: difftest
difftest:
	cargo build
	cargo run -q -p awsr-difftest -- list difftest/smoke.txt

.PHONY: difftest-full
difftest-full:
	cargo build --release
	cargo run -q --release -p awsr-difftest -- corpus --awsr-bin target/release/awsr --seeds 4 --formats json,text,table

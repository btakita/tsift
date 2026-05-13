.PHONY: check precommit test test-ignored ci-full clippy

check: clippy test

precommit: check

test:
	cargo test

test-ignored:
	cargo test -- --ignored

ci-full: clippy test test-ignored

clippy:
	cargo clippy --all-targets -- -D warnings

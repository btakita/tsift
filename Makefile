.PHONY: check precommit test clippy

check: clippy test

precommit: check

test:
	cargo test

clippy:
	cargo clippy -- -D warnings

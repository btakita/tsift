.PHONY: check precommit test ci-full clippy

check: clippy test

precommit: check

test:
	cargo test

ci-full: check

clippy:
	cargo clippy --all-targets -- -D warnings

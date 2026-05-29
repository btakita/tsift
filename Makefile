.PHONY: check precommit test ci-full clippy opencode-plugin-test

check: clippy test opencode-plugin-test

precommit: check

test:
	cargo test --workspace

ci-full: check

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

opencode-plugin-test:
	cd packages/opencode-tsift && npm test
	cd packages/opencode-tsift && npm run publish:check

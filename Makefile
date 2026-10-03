.PHONY: check precommit test test-all-languages ci-full clippy opencode-plugin-test

check: clippy test test-all-languages opencode-plugin-test

precommit: check

test:
	cargo test --workspace

# Opt-in languages (`lang-jai`) are outside `default`, so `test` alone never
# runs their tests. This pass keeps them, and the release build's feature set,
# covered in CI.
test-all-languages:
	cargo test --workspace --features all-languages

ci-full: check

clippy:
	cargo clippy --workspace --all-targets -- -D warnings
	cargo clippy --workspace --all-targets --features all-languages -- -D warnings

opencode-plugin-test:
	cd packages/opencode-tsift && npm test
	cd packages/opencode-tsift && npm run pack:check

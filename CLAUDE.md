# tsift

Token-efficient CLI plugin for Claude Code — hybrid search, batch editing, SQL introspection, and model routing.

## Architecture

Single-binary Rust CLI (`src/main.rs`). All commands are subcommands via clap derive:

| Command | Purpose |
|---------|---------|
| `tsift search` | Hybrid BM25 + vector search via sift library (git dep) |
| `tsift edit` | Batch file edits from JSON (stdin or `--file`), atomic validate-then-write |
| `tsift route` | Classify task → model tier (haiku/sonnet/opus) |
| `tsift rewrite` | Shell command → tsift equivalent (for Claude Code hook integration) |
| `tsift sql` | SQLite introspection: schema overview, table detail, read-only query |

## Dependencies

- **sift** — git dep (`github.com/rupurt/sift`), not on crates.io. Provides `Sift::builder().build()` + `engine.search()`.
- **rusqlite** — `bundled` feature (no system SQLite needed).
- **clap**, **anyhow**, **serde**, **serde_json** — standard Rust CLI stack.

## Development

```bash
cargo test          # 27 tests (route, edit, rewrite, sql)
cargo clippy        # must pass clean
cargo install --path .   # install to ~/.cargo/bin/
```

## Conventions

- **Read-only SQL**: `open_db()` uses `SQLITE_OPEN_READ_ONLY` — never mutates user databases.
- **Atomic batch edits**: `cmd_edit` validates ALL edits before writing ANY (two-phase).
- **Rewrite protocol**: exit 0 + stdout = rewrite, exit 1 = pass through (matches rtk convention).
- **Search strategy default**: `lexical` for instant results. `hybrid`/`vector` require model download on first run.
- **All public logic tested**: `classify_task`, `apply_edit_op`, `rewrite_command`, SQL helpers all have unit tests.

## Hook Integration

`tsift rewrite` is wired as a Claude Code PreToolUse hook via `~/.claude/hooks/tsift-rewrite.sh`. Rewrites `rg`/`grep -r` commands to `tsift search --strategy lexical`.

## Repo

Private: `github.com/btakita/tsift`. Submodule at `src/tsift` in agent-loop.

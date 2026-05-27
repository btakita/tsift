# opencode-tsift

OpenCode command shortcuts for `tsift`.

Install the plugin from npm:

```sh
opencode plugin opencode-tsift
```

OpenCode installs npm plugins with Bun at startup and loads plugins listed in
the `plugin` config array. This package writes marker-owned project commands
into `.opencode/commands/` when the plugin loads, so the following commands are
available in a project without cloning the `tsift` repository:

- `/tsift-status`
- `/tsift-session-review`
- `/tsift-context-pack`
- `/tsift-diff-digest`
- `/tsift-test-digest`
- `/tsift-log-digest`

The commands shell out to the `tsift` binary. Install `tsift` first:

```sh
curl -fsSL https://raw.githubusercontent.com/btakita/tsift/main/scripts/install.sh | sh
```

You can also install or refresh the command files directly:

```sh
npx opencode-tsift .
```

Existing command files with the same names are only replaced when they already
contain the `tsift:opencode-command` ownership marker.

## Permissions

All commands shell out to the `tsift` binary via Bash. Without
`--dangerously-skip-permissions`, OpenCode will prompt for approval on each
shell invocation. The required permissions break down by command:

| Command | Bash execution | File read | File write |
|---|---|---|---|
| `/tsift-status` | `tsift status --fix` | `.tsift/`, source tree | `.tsift/`, AGENTS.md, CLAUDE.md |
| `/tsift-session-review` | `tsift --envelope session-review` | `.tsift/`, agent-doc logs | — |
| `/tsift-context-pack` | `tsift --envelope context-pack` | `.tsift/`, source files | — |
| `/tsift-diff-digest` | `tsift diff-digest` | `.tsift/`, git working tree | — |
| `/tsift-test-digest` | `tsift --envelope __digest-runner --kind test` | `.tsift/`, test output | — |
| `/tsift-log-digest` | `tsift --envelope __digest-runner --kind log` | `.tsift/`, build output | — |

`/tsift-status` is the only command that writes files (index, instructions).
The other commands are read-only.

## Troubleshooting

**`tsift: command not found`** — Install the tsift binary first:

```sh
curl -fsSL https://raw.githubusercontent.com/btakita/tsift/main/scripts/install.sh | sh
```

**`tsift status` reports stale index** — Run `/tsift-status` again; it passes
`--fix` which reindexes automatically.

**Command files conflict with existing files** — If `.opencode/commands/tsift-*.md`
exists without the `tsift:opencode-command` marker, the installer refuses to
overwrite it. Move or rename the conflicting file, then reinstall.

**Plugin does not install commands at startup** — Verify the plugin is listed in
your OpenCode config:

```json
{
  "plugin": ["opencode-tsift"]
}
```

OpenCode installs and loads plugins with Bun on startup. Check that Bun can
resolve the package by running `opencode plugin opencode-tsift` again.

**`npx opencode-tsift .` does nothing** — If the command files are already
current, the tool reports `already present` for each file. This is the expected
idempotent behavior.

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

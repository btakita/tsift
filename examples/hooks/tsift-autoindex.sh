#!/usr/bin/env bash
# tsift-hook-version: 2
# Claude Code UserPromptSubmit hook — auto-reindex tsift when stale.
# Install: add to .claude/settings.json under hooks.UserPromptSubmit
#
# {
#   "hooks": {
#     "UserPromptSubmit": [
#       { "matcher": "", "command": "path/to/tsift-autoindex.sh" }
#     ]
#   }
# }

command -v tsift &>/dev/null || exit 0

root="$(git rev-parse --show-toplevel 2>/dev/null)" || exit 0

# In workspace repos, one root hook can protect every initialized submodule.
if [ -f "$root/.gitmodules" ]; then
  check_cmd=(tsift index --check --exit-code --workspace "$root")
  rebuild_cmd=(tsift index --workspace "$root")
else
  check_cmd=(tsift index --check --exit-code "$root")
  rebuild_cmd=(tsift index "$root")
fi

# --check: dry-run (don't modify index)
# --exit-code: exit 1 if stale files found
if ! "${check_cmd[@]}" >/dev/null 2>&1; then
  "${rebuild_cmd[@]}" >/dev/null 2>&1
fi

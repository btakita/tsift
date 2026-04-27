#!/usr/bin/env bash
# tsift-hook-version: 1
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

# --check: dry-run (don't modify index)
# --exit-code: exit 1 if stale files found
if ! tsift index --check --exit-code . >/dev/null 2>&1; then
  tsift index . >/dev/null 2>&1
fi

#!/usr/bin/env bash
# tsift-hook-version: 2
# tsift Claude Code PreToolUse hook — rewrites high-token shell commands to
# use tsift for token savings, and routes verbose tsift commands through RTK
# for output filtering.
# Requires: tsift, jq
#
# Exit code protocol for `tsift rewrite`:
#   0 + stdout  Rewrite found → auto-allow
#   1           No tsift equivalent → pass through unchanged
#
# Install: add to .claude/settings.json under hooks.PreToolUse
#
# {
#   "hooks": {
#     "PreToolUse": [
#       { "matcher": "Bash", "command": "examples/hooks/tsift-rewrite.sh" }
#     ]
#   }
# }

if ! command -v jq &>/dev/null; then
  exit 0
fi

if ! command -v tsift &>/dev/null; then
  exit 0
fi

INPUT=$(cat)
CMD=$(echo "$INPUT" | jq -r '.tool_input.command // empty')

if [ -z "$CMD" ]; then
  exit 0
fi

emit_rewrite() {
  local rewritten="$1"
  local reason="$2"
  local original_input
  original_input=$(echo "$INPUT" | jq -c '.tool_input')
  local updated_input
  updated_input=$(echo "$original_input" | jq --arg cmd "$rewritten" '.command = $cmd')

  jq -n \
    --argjson updated "$updated_input" \
    --arg reason "$reason" \
    '{
      "hookSpecificOutput": {
        "hookEventName": "PreToolUse",
        "permissionDecision": "allow",
        "permissionDecisionReason": $reason,
        "updatedInput": $updated
      }
    }'
}

# Phase 1: tsift rewrite (rg/grep → tsift search)
REWRITTEN=$(tsift rewrite "$CMD" 2>/dev/null)
EXIT_CODE=$?

if [ $EXIT_CODE -eq 0 ] && [ "$CMD" != "$REWRITTEN" ]; then
  emit_rewrite "$REWRITTEN" "tsift auto-rewrite"
  exit 0
fi

# Phase 2: route verbose tsift commands through RTK for output filtering
if command -v rtk &>/dev/null; then
  case "$CMD" in
    tsift\ communities*|tsift\ explain*|tsift\ graph*|tsift\ index*|tsift\ search*|\
    tsift\ --compact\ communities*|tsift\ --compact\ explain*|tsift\ --compact\ graph*|\
    tsift\ --compact\ search*|tsift\ --pretty\ *)
      case "$CMD" in rtk\ *) exit 0 ;; esac
      emit_rewrite "rtk $CMD" "tsift→rtk output filter"
      exit 0
      ;;
  esac
fi

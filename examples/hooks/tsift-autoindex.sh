#!/usr/bin/env bash
# tsift-hook-version: 3
# Claude Code UserPromptSubmit hook — queue a detached auto-reindex when stale.
# Install: add to .claude/settings.json under hooks.UserPromptSubmit
#
# {
#   "hooks": {
#     "UserPromptSubmit": [
#       { "matcher": "", "command": "path/to/tsift-autoindex.sh" }
#     ]
#   }
# }

if [ "${TSIFT_AUTOINDEX_WORKER:-0}" != "1" ]; then
  TSIFT_AUTOINDEX_WORKER=1 nohup "$0" </dev/null >/dev/null 2>&1 &
  exit 0
fi

command -v tsift &>/dev/null || exit 0
root="$(git rev-parse --show-toplevel 2>/dev/null)" || exit 0
mkdir -p "$root/.tsift" || exit 0

# Coalesce prompt bursts across windows. tsift's native index.lock remains the
# fallback single-flight guard on platforms without flock.
if command -v flock &>/dev/null; then
  exec 9>"$root/.tsift/autoindex-hook.lock"
  flock -n 9 || exit 0
fi

sleep "${TSIFT_AUTOINDEX_DEBOUNCE_SECONDS:-0.25}"
max_runtime_seconds="${TSIFT_AUTOINDEX_MAX_SECONDS:-120}"
case "$max_runtime_seconds" in
''|*[!0-9]*) max_runtime_seconds=120 ;;
esac
worker_started_seconds=$SECONDS

run_tsift() {
local -a runner=()
local remaining_seconds="$max_runtime_seconds"
if command -v nice &>/dev/null; then
runner+=(nice -n 10)
fi
if [ "$max_runtime_seconds" != "0" ] && command -v timeout &>/dev/null; then
remaining_seconds=$((max_runtime_seconds - (SECONDS - worker_started_seconds)))
[ "$remaining_seconds" -gt 0 ] || return 124
runner+=(timeout --signal=TERM --kill-after=5 "${remaining_seconds}s")
fi
if [ -n "${TSIFT_AUTOINDEX_CPU_AFFINITY:-}" ] && command -v taskset &>/dev/null; then
runner+=(taskset -c "$TSIFT_AUTOINDEX_CPU_AFFINITY")
fi
command "${runner[@]}" tsift "$@"
}

# In workspace repos, one root hook can protect every initialized submodule.
if [ -f "$root/.gitmodules" ] && [ -n "${TSIFT_AUTOINDEX_FOCUS:-}" ]; then
  IFS=',' read -r -a focus_scopes <<<"$TSIFT_AUTOINDEX_FOCUS"
  for scope in "${focus_scopes[@]}"; do
    [ -n "$scope" ] || continue
    run_tsift index --check --exit-code --submodule "$scope" "$root" >/dev/null 2>&1 ||
      run_tsift index --submodule "$scope" "$root" >/dev/null 2>&1
  done
elif [ -f "$root/.gitmodules" ]; then
  run_tsift index --check --exit-code --workspace "$root" >/dev/null 2>&1 ||
    run_tsift index --workspace "$root" >/dev/null 2>&1
else
  run_tsift index --check --exit-code "$root" >/dev/null 2>&1 ||
    run_tsift index "$root" >/dev/null 2>&1
fi

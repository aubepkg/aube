#!/usr/bin/env bash
# sfw shim for __CMD__ — pre-baked into the agent image; mirrors what
# scripts/soak/external-tools.mts --shims writes on dev machines. Routes the
# real package-manager invocation through Socket Firewall; the sentinel env
# var breaks recursion when sfw re-invokes the tool, and the real binary is
# found by stripping the tool-rack bin dir out of PATH.
set -euo pipefail
CLEAN_PATH=$(printf '%s' "$PATH" | tr ':' '\n' | grep -vFx '/root/.local/share/aube/dev-tools/bin' | paste -sd ':' -)
REAL=$(PATH="$CLEAN_PATH" command -v '__CMD__' || true)
if [ -n "${__SENTINEL__:-}" ] || [ -z "$REAL" ] || ! command -v sfw >/dev/null 2>&1; then
	[ -n "$REAL" ] && exec "$REAL" "$@"
	echo "__CMD__: not found" >&2
	exit 127
fi
export __SENTINEL__=1
exec sfw '__CMD__' "$@"

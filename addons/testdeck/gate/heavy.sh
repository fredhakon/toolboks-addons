#!/usr/bin/env bash
# heavy.sh: run one heavy command (next build, full tsc, lint of a whole package) in a machine-wide test slot.
# usage: bash ~/.claude/gate/heavy.sh <command...>     e.g. bash ~/.claude/gate/heavy.sh npx next build
set -u
. "$(dirname "${BASH_SOURCE[0]}")/slots.sh"
slot_take
export JT_WORKERS="${JT_WORKERS:-$( [ -n "${GATE_EXCL_HELD:-}" ] && echo 4 || slot_workers )}"
"$@"

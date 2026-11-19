# slots.sh: machine-wide heavy-work queue (owner rule 2026-11-17). Source it; do not run it.
# 2 slots. jt.sh and heavy.sh take one slot each. gate.sh takes the whole PC: it marks "excl", waits until
# every slot is free, then holds them all until it exits. Lock order is always slot first, rdb-lock second.
# Nested calls inside a holder (GATE_EXCL_HELD=1 or JT_SLOT_HELD=1) take nothing.
# Testdeck: when the Testdeck broker answers on 127.0.0.1, slot_take and slot_excl ask it first; it mirrors its
# grants into $SLOTDIR. When it is down or dies mid-wait, the file queue below runs as before.
# TESTDECK_DISABLE=1 skips Testdeck. A request cancelled in the Testdeck window exits the caller with 3.
SLOTS="${JT_SLOTS:-2}"; SLOTDIR="${TMPDIR:-/tmp}/jt-slots"; mkdir -p "$SLOTDIR"
# The installed copy next to this file, never the Testdeck dev checkout. Missing: Testdeck is skipped.
TESTDECK_CLIENT="$(dirname "${BASH_SOURCE[0]}")/testdeck-client.sh"
[ -r "$TESTDECK_CLIENT" ] && . "$TESTDECK_CLIENT"

_slot_live() { local p; p="$(cat "$1/pid" 2>/dev/null)"; [ -n "$p" ] && kill -0 "$p" 2>/dev/null; }
_slot_reap() { [ -d "$1" ] && ! _slot_live "$1" && [ -n "$(find "$1" -maxdepth 0 -mmin +0.1 2>/dev/null)" ] && rm -rf "$1"; }

# Ask Testdeck: 0 granted (the EXIT trap releases), 1 not available (use the file queue); cancelled exits 3.
# td_acquire reaps dead slot dirs with _slot_reap before every poll.
_slot_td() {
  [ -z "${TESTDECK_DISABLE:-}" ] && declare -F td_acquire >/dev/null || return 1
  local rc
  td_acquire "$@"; rc=$?
  [ $rc -eq 3 ] && exit 3
  [ $rc -eq 0 ] || return 1
  trap 'td_release $?' EXIT
}

slot_take() {
  if [ -n "${GATE_EXCL_HELD:-}${JT_SLOT_HELD:-}" ]; then
    # A nested jt.sh (the --rdb path) points the broker at its own jest log.
    if [ -n "${TD_TICKET:-}" ] && [ -z "${GATE_EXCL_HELD:-}" ] && [ "${0##*/}" = jt.sh ]; then
      td_started "$(_td_log_glob $$)"
    fi
    return 0
  fi
  if _slot_td; then export JT_SLOT_HELD=1; return 0; fi
  local said="" i d
  MYSLOT=""
  while :; do
    _slot_reap "$SLOTDIR/excl"
    if [ ! -d "$SLOTDIR/excl" ]; then
      for i in $(seq 1 "$SLOTS"); do
        d="$SLOTDIR/$i"
        if mkdir "$d" 2>/dev/null; then echo $$ > "$d/pid"; MYSLOT="$d"; break 2; fi
        _slot_reap "$d"
      done
    fi
    [ -z "$said" ] && { echo "queue: waiting for a free test slot ($SLOTDIR)" >&2; said=1; }
    sleep 5
  done
  export JT_SLOT_HELD=1 MYSLOT
  trap 'rm -rf "$MYSLOT"' EXIT
}

slot_excl() {
  [ -n "${GATE_EXCL_HELD:-}" ] && return 0
  if _slot_td gate; then export GATE_EXCL_HELD=1; return 0; fi
  local said="" i busy
  until mkdir "$SLOTDIR/excl" 2>/dev/null; do _slot_reap "$SLOTDIR/excl"; sleep 5; done
  echo $$ > "$SLOTDIR/excl/pid"
  trap 'rm -rf "$SLOTDIR/excl"' EXIT
  while :; do
    busy=0
    for i in $(seq 1 "$SLOTS"); do _slot_reap "$SLOTDIR/$i"; [ -d "$SLOTDIR/$i" ] && busy=1; done
    [ $busy -eq 0 ] && break
    [ -z "$said" ] && { echo "queue: gate waits for running test slots to finish" >&2; said=1; }
    sleep 5
  done
  export GATE_EXCL_HELD=1
}

# Testdeck grant: its worker count. Else 4 jest workers only when the PC is otherwise quiet: no other slot busy,
# CPU under 40 %, at least 8 GB free RAM.
slot_workers() {
  [ -n "${TD_WORKERS:-}" ] && { echo "$TD_WORKERS"; return; }
  local i cpu free
  for i in $(seq 1 "$SLOTS"); do [ "$SLOTDIR/$i" != "${MYSLOT:-}" ] && [ -d "$SLOTDIR/$i" ] && { echo 2; return; }; done
  free=$(awk '/^MemFree:/{print int($2/1048576)}' /proc/meminfo 2>/dev/null); [ "${free:-0}" -ge 8 ] || { echo 2; return; }
  cpu=$(timeout 8 powershell.exe -NoProfile -Command "[int](Get-CimInstance Win32_Processor | Measure-Object LoadPercentage -Average).Average" 2>/dev/null | tr -dc 0-9)
  [ -n "$cpu" ] && [ "$cpu" -lt 40 ] && echo 4 || echo 2
}

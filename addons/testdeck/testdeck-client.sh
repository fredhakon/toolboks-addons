# testdeck-client.sh: bash client for the Testdeck broker. Source it; do not run it.
# Functions only, safe to source under `set -u`. Every network call is bounded (connect timeout 1 s), so
# nothing hangs when the broker is down or dies mid-wait: td_acquire then returns 1 and the caller falls back
# to the file queue in slots.sh. Parsing and escaping use bash builtins only (no jq, few forks: every fork is
# slow in Git Bash on a loaded PC).
#
#   td_up                    0 when the broker answers on 127.0.0.1:${TESTDECK_PORT:-47123}
#   td_acquire [kind]        0 granted (exports TD_TICKET, TD_WORKERS, JT_WORKERS; starts heartbeats),
#                            1 broker unreachable or lost, 3 cancelled in the Testdeck queue
#   td_started [glob] [pid]  updates the jest log glob and/or the Windows pid of the running ticket
#   td_release [exit code]   releases the ticket and stops the heartbeats
#   td_status                prints the broker status as one line
# Environment: TESTDECK_PORT, TESTDECK_REQUESTER, TESTDECK_DISABLE=1 (always return 1), TD_KIND, TD_COMMAND,
# TD_PRIORITY (integer).

_td_url() { printf 'http://127.0.0.1:%s' "${TESTDECK_PORT:-47123}"; }

# _td_post <path> <json> <max seconds>: prints the response body; non-zero when the broker cannot be reached.
_td_post() {
  curl -s --connect-timeout 1 --max-time "$3" -H 'Content-Type: application/json' \
    -X POST --data-binary "$2" "$(_td_url)$1" 2>/dev/null
}

# _td_get <var> <json> <key>: sets var to a flat string or integer field of the JSON, "" when missing.
_td_get() {
  local _re="\"$3\":\"([^\"]*)\"" _ren="\"$3\":(-?[0-9]+)"
  if [[ $2 =~ $_re ]] || [[ $2 =~ $_ren ]]; then
    printf -v "$1" '%s' "${BASH_REMATCH[1]}"
  else
    printf -v "$1" '%s' ""
  fi
}

# _td_esc <var> <text>: sets var to text as a JSON string body (backslashes and quotes escaped, control
# characters as spaces).
_td_esc() {
  local _s=$2 _bs='\' _q='"'
  _s=${_s//"$_bs"/"$_bs$_bs"}
  _s=${_s//"$_q"/"$_bs$_q"}
  _s=${_s//[$'\001'-$'\037']/ }
  printf -v "$1" '%s' "$_s"
}

# _td_cmdline <var>: this shell's command line with spaces for NULs (builtin read of /proc).
_td_cmdline() {
  local -a _a=()
  mapfile -d '' _a < "/proc/$$/cmdline" 2>/dev/null
  printf -v "$1" '%s' "${_a[*]}"
}

_td_kind() {
  if [ -n "${TD_KIND:-}" ]; then printf '%s' "$TD_KIND"; return; fi
  case "${0##*/}" in jt.sh|rdb-lock.sh) printf jest; return;; esac
  local cl
  _td_cmdline cl
  case " $cl " in
    *" next build"*|*cargo*) printf build;;
    *tsc*|*typecheck*) printf tsc;;
    *lint*|*eslint*) printf lint;;
    *) printf build;;
  esac
}

_td_requester() {
  if [ -n "${TESTDECK_REQUESTER:-}" ]; then printf '%s' "$TESTDECK_REQUESTER"; return; fi
  local top
  top=$(git rev-parse --show-toplevel 2>/dev/null)
  if [ -n "$top" ]; then printf '%s' "${top##*/}"; else printf '%s' "${PWD##*/}"; fi
}

# _td_log_glob <msys pid>: where a jt.sh run by that shell writes its jest log.
_td_log_glob() {
  local tmp="${TMPDIR:-/tmp}" sep=/
  if command -v cygpath >/dev/null 2>&1; then tmp=$(cygpath -w "$tmp"); sep='\'; fi
  printf '%s%sjt-%s-*%sjest.log' "$tmp" "$sep" "$1" "$sep"
}

# Reaps dead slot dirs with slots.sh's _slot_reap (when sourced by slots.sh). The broker counts every slot dir it
# did not create as busy, so a dead one would block queued clients until it is reaped.
_td_reap() {
  declare -F _slot_reap >/dev/null && [ -n "${SLOTDIR:-}" ] || return 0
  local d
  for d in "$SLOTDIR"/[0-9]* "$SLOTDIR/excl"; do _slot_reap "$d"; done
  return 0
}

td_up() {
  curl -s --connect-timeout 1 --max-time 2 "$(_td_url)/health" 2>/dev/null | grep -q '"app":"testdeck"'
}

td_acquire() {
  local kind="${1:-}" body resp st pos="" p eta w="" lg=null pr cmd req paths
  local cwd=$PWD tmp="${TMPDIR:-/tmp}" sep=/
  [ -n "${TESTDECK_DISABLE:-}" ] && return 1
  command -v curl >/dev/null 2>&1 || return 1
  [ -n "$kind" ] || kind=$(_td_kind)
  if command -v cygpath >/dev/null 2>&1; then
    paths=$(cygpath -w "$PWD" "$tmp")
    cwd=${paths%%$'\n'*}
    tmp=${paths#*$'\n'}
    sep='\'
  fi
  # Windows pid with the builtin read: an external `cat` may be rewritten by a hook in agent shells.
  { read -r w < "/proc/$$/winpid"; } 2>/dev/null
  case "${TD_PRIORITY:-0}" in ''|*[!0-9-]*) pr=0;; *) pr=${TD_PRIORITY:-0};; esac
  if [ -n "${TD_COMMAND:-}" ]; then cmd=$TD_COMMAND; else _td_cmdline cmd; fi
  req=$(_td_requester)
  _td_esc kind "$kind"
  _td_esc cwd "$cwd"
  _td_esc cmd "$cmd"
  _td_esc req "$req"
  if [ "$kind" = jest ]; then
    _td_esc lg "$tmp${sep}jt-$$-*${sep}jest.log"
    lg="\"$lg\""
  fi
  body="{\"kind\":\"$kind\",\"cwd\":\"$cwd\",\"command\":\"$cmd\",\"requester\":\"$req\",\"priority\":$pr,\"shell_pid\":$$,\"win_pid\":${w:-null},\"log_glob\":$lg"
  # RAM requeue (docs/PHASE2.md): TD_REQUEUE_OK=1 = this caller can rerun its command; TD_REQUEUE_OF = the ticket
  # this request replaces (it goes to the queue head).
  [ -n "${TD_REQUEUE_OK:-}" ] && body+=",\"requeue_ok\":true"
  case "${TD_REQUEUE_OF:-}" in ''|*[!0-9]*) ;; *) body+=",\"requeue_of\":$TD_REQUEUE_OF";; esac
  body+="}"
  TD_TICKET=""
  while :; do
    # Before the first request and every re-poll, so dead dirs cannot keep a queued request waiting.
    _td_reap
    resp=$(_td_post /acquire "$body" 30) || resp=""
    _td_get st "$resp" status
    case "$st" in
      granted)
        _td_get TD_TICKET "$resp" ticket
        _td_get TD_WORKERS "$resp" workers
        JT_WORKERS=$TD_WORKERS
        export TD_TICKET TD_WORKERS JT_WORKERS
        _td_hb_start
        return 0;;
      waiting)
        _td_get TD_TICKET "$resp" ticket
        _td_get p "$resp" position
        _td_get eta "$resp" eta_secs
        if [ "$p" != "$pos" ]; then
          echo "queue: waiting in Testdeck, position $p, about ${eta:-?} s" >&2
          pos=$p
        fi
        body="{\"ticket\":${TD_TICKET:-0}}";;
      cancelled)
        TD_TICKET=""
        echo "testdeck: request cancelled in the Testdeck queue" >&2
        return 3;;
      *)
        [ -n "$TD_TICKET" ] && echo "testdeck: lost the broker while waiting; using the file queue" >&2
        TD_TICKET=""
        return 1;;
    esac
  done
}

# Heartbeat every 5 s (the broker releases a ticket with a dead Windows pid after 12 s without one) in a
# background subshell until this shell is gone or the broker forgets the ticket.
# All output goes to /dev/null so it never holds a caller's pipe open.
_td_hb_start() {
  local t="$TD_TICKET" parent=$$ url
  url="$(_td_url)/heartbeat"
  (
    while kill -0 "$parent" 2>/dev/null; do
      sleep 5
      code=$(curl -s -o /dev/null -w '%{http_code}' --connect-timeout 1 --max-time 3 \
        -H 'Content-Type: application/json' -X POST --data-binary "{\"ticket\":$t}" "$url")
      [ "$code" = 404 ] && break
    done
  ) >/dev/null 2>&1 </dev/null &
  TD_HB_PID=$!
}

td_started() {
  [ -n "${TD_TICKET:-}" ] || return 1
  local lg=null pid=null
  if [ -n "${1:-}" ]; then _td_esc lg "$1"; lg="\"$lg\""; fi
  case "${2:-}" in ''|*[!0-9]*) ;; *) pid=$2;; esac
  _td_post /started "{\"ticket\":$TD_TICKET,\"pid\":$pid,\"log_glob\":$lg}" 3 >/dev/null
}

td_release() {
  local code="${1:-0}"
  case "$code" in ''|*[!0-9-]*) code=null;; esac
  if [ -n "${TD_HB_PID:-}" ]; then kill "$TD_HB_PID" 2>/dev/null; TD_HB_PID=""; fi
  [ -n "${TD_TICKET:-}" ] || return 0
  _td_post /release "{\"ticket\":$TD_TICKET,\"exit_code\":$code}" 3 >/dev/null
  TD_TICKET=""
  return 0
}

td_status() {
  local r line
  r=$(curl -s --connect-timeout 1 --max-time 3 "$(_td_url)/status" 2>/dev/null)
  if [ -z "$r" ]; then echo "testdeck: not running"; return 1; fi
  _td_get line "$r" line
  printf '%s\n' "$line"
}

# ---- Phase 2 helpers for jt.sh. The broker answers these in plain text (first line = status word); each prints
# the answer and fails (prints nothing) when the broker cannot be reached or is an older build without the route.

# _td_text <path> [json] [max seconds]: GET without json, else POST.
_td_text() {
  if [ -n "${2:-}" ]; then
    curl -sf --connect-timeout 1 --max-time "${3:-3}" -H 'Content-Type: application/json' \
      -X POST --data-binary "$2" "$(_td_url)$1" 2>/dev/null
  else
    curl -sf --connect-timeout 1 --max-time "${3:-3}" "$(_td_url)$1" 2>/dev/null
  fi
}

# _td_esc_nl <var> <text>: like _td_esc, but keeps newlines and tabs as JSON escapes (multi-line summaries).
_td_esc_nl() {
  local _s=$2 _bs='\' _q='"'
  _s=${_s//"$_bs"/"$_bs$_bs"}
  _s=${_s//"$_q"/"$_bs$_q"}
  _s=${_s//$'\r'/}
  _s=${_s//$'\n'/"${_bs}n"}
  _s=${_s//$'\t'/"${_bs}t"}
  _s=${_s//[$'\001'-$'\037']/ }
  printf -v "$1" '%s' "$_s"
}

# td_cache_key <jt.sh args...>: the result-cache key for a jt.sh run in $PWD: its arguments, cwd, HEAD, the
# environment variables that change what a run does (TEST_*, CUSTOM_*, REAL_DB_*, RDB_*, CI, NODE_*, JEST_*;
# sorted), and a hash of `git diff HEAD` plus every untracked file of the repo (names and contents), both from the
# top level: suites in tests/ import server/src. JT_WORKERS is left out: the worker count changes timing, not
# results. Fails outside a git work tree or when a git call fails (a failed call must not hash as a clean tree).
td_cache_key() {
  local head top files envs sum
  head=$(git rev-parse HEAD 2>/dev/null) || return 1
  top=$(git rev-parse --show-toplevel 2>/dev/null) || return 1
  files=$(git -C "$top" -c core.quotePath=false ls-files --others --exclude-standard 2>/dev/null) || return 1
  envs=$(env | grep -E '^(TEST_[A-Za-z0-9_]*|CUSTOM_[A-Za-z0-9_]*|REAL_DB_[A-Za-z0-9_]*|RDB_[A-Za-z0-9_]*|CI|NODE_[A-Za-z0-9_]*|JEST_[A-Za-z0-9_]*)=' | LC_ALL=C sort)
  sum=$(set -o pipefail; {
    printf '%s\n' "$*" "$PWD" "$head" "$envs"
    git -C "$top" diff HEAD --binary 2>/dev/null || exit 1
    printf '%s\n' "$files"
    if [ -n "$files" ]; then printf '%s\n' "$files" | git -C "$top" hash-object --stdin-paths 2>/dev/null || exit 1; fi
  } | sha1sum) || return 1
  printf '%s' "${sum%% *}"
}

# td_cache_lookup <key>: hit/running/miss/off (see src/cache.rs); a miss makes this shell the key's owner.
td_cache_lookup() {
  local w=""
  { read -r w < "/proc/$$/winpid"; } 2>/dev/null
  _td_text /cache/lookup "{\"key\":\"$1\",\"win_pid\":${w:-null}}"
}

# td_cache_wait <key>: long-polls for the result of the identical run in progress: hit/running/gone.
td_cache_wait() { _td_text /cache/wait "{\"key\":\"$1\"}" 30; }

# td_cache_store <key> [exit code] [summary file]: stores the result (the broker skips results it never caches);
# with only the key it gives up the claim, so joined runs start their own run.
td_cache_store() {
  local s=""
  if [ -n "${2:-}" ] && [ -r "${3:-}" ]; then
    _td_esc_nl s "$(<"$3")"
    _td_text /cache/store "{\"key\":\"$1\",\"exit_code\":$2,\"summary\":\"$s\n\"}" >/dev/null
  else
    _td_text /cache/store "{\"key\":\"$1\"}" >/dev/null
  fi
}

# td_requeue_pending: 0 when the broker stopped this ticket's run for low RAM and wants it rerun (one heartbeat,
# answer {"status":"requeue"}).
td_requeue_pending() {
  [ -n "${TD_TICKET:-}" ] || return 1
  local r
  r=$(_td_post /heartbeat "{\"ticket\":$TD_TICKET}" 3) || return 1
  [[ $r == *'"status":"requeue"'* ]]
}

# td_failed_first:the failing suites of the last jest job of this requester in this cwd (history), one absolute
# path per line. The cwd and requester are those td_acquire sends, so they match the history rows.
td_failed_first() {
  local cwd=$PWD req
  command -v cygpath >/dev/null 2>&1 && cwd=$(cygpath -w "$PWD")
  req=$(_td_requester)
  _td_esc cwd "$cwd"
  _td_esc req "$req"
  _td_text /failed_first "{\"requester\":\"$req\",\"cwd\":\"$cwd\"}"
}

# td_flaky_list: the flaky_suites list, one path fragment per line.
td_flaky_list() { _td_text /flaky; }

# td_known_red: the known-red-on-staging list (known-red.txt in the data folder), one entry per line.
td_known_red() { _td_text /known_red; }

# td_known_red_add <suite...>: adds suites to the known-red list (the broker skips duplicates).
td_known_red_add() {
  local s e body=""
  for s in "$@"; do _td_esc e "$s"; body+="${body:+,}\"$e\""; done
  [ -n "$body" ] && _td_text /known_red "{\"suites\":[$body]}" >/dev/null
}

# td_in_list <suite> <list>: 0 when a non-empty, non-comment line of the list is a substring of the suite path
# (backslashes compared as slashes).
td_in_list() {
  local e
  while IFS= read -r e; do
    e=${e%$'\r'}
    e=${e//\\//}
    case "$e" in ''|'#'*) continue;; esac
    [[ ${1//\\//} == *"$e"* ]] && return 0
  done <<<"$2"
  return 1
}

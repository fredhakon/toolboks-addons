#!/usr/bin/env bash
# jt.sh: run jest and print only a short summary, so agents do not pull pages of jest output into context.
# usage (from the directory that owns the jest config, e.g. tests/):
#   bash ~/.claude/gate/jt.sh [--rdb] [--full] <jest args...>
#   --rdb   run through rdb-lock.sh (real-DB lane lock); same summary
#   --full  also print the raw jest output (only when the summary is not enough)
# Output: one line per failing test "FAIL <file> > <test>: <first error line>" (max 30), suites that failed to run,
# then jest-style "Tests:" and "Test Suites:" lines. The raw log path is printed for drill-down with grep/sed.
# Exit code: jest's.
set -u
# Refuse a run with no test path or pattern: it would run the whole suite (2600+ files) in a slot.
# The full suite belongs to gate.sh; JT_ALLOW_ALL=1 overrides for a deliberate whole-suite run (owner rule 2026-11-17).
if [ -z "${JT_ALLOW_ALL:-}" ] && [ -z "${GATE_EXCL_HELD:-}" ]; then
  _jt_has_target=""; _jt_skip=""
  for _a in "$@"; do
    if [ -n "$_jt_skip" ]; then _jt_skip=""; continue; fi
    case "$_a" in
      --rdb|--full) ;;
      --config|-c|--rootDir|--selectProjects|--projects|--maxWorkers|-w|--testTimeout|--reporters|-t|--testNamePattern) _jt_skip=1 ;;
      -*) ;;
      *) _jt_has_target=1 ;;
    esac
  done
  if [ -z "$_jt_has_target" ]; then
    echo "jt: refused: no test file or path pattern given, so jest would run the whole suite. Pass the test files (find them with npx jest --listTests --findRelatedTests <src file>); the full suite runs only in gate.sh." >&2
    exit 2
  fi
fi
# Machine-wide queue (owner rule 2026-11-17, slots.sh): 2 slots shared with heavy.sh; gate.sh takes all of them.
# Take the slot BEFORE the rdb lane lock (same order as gate.sh, so they cannot deadlock).
. "$(dirname "${BASH_SOURCE[0]}")/slots.sh"
# Testdeck phase 2 (docs/PLAN.md, "jt.sh token savers"): only while the broker answers. With TESTDECK_DISABLE=1, a
# down broker or no testdeck-client.sh, jt.sh runs exactly as before. Inside a gate: no cache, no failed-first,
# no requeue. The tag lines ("jt: ...") only follow the summary.
JT_NESTED="${GATE_EXCL_HELD:-}${JT_SLOT_HELD:-}"
[ -z "$JT_NESTED" ] && JT_CACHE_KEY=""
# _jt_hit <answer> <tag>: prints a stored summary and its tag line, then exits with the stored exit code.
_jt_hit() {
  local head=${1%%$'\n'*} st code age
  read -r st code age <<<"$head"
  [ "$head" != "$1" ] && printf '%s\n' "${1#*$'\n'}"
  if [ "$2" = cached ]; then
    if [ "$age" -lt 60 ]; then age="${age}s"; elif [ "$age" -lt 3600 ]; then age="$((age / 60))m"
    else age="$((age / 3600))h$((age % 3600 / 60))m"; fi
    echo "jt: (cached $age ago; TESTDECK_NO_CACHE=1 reruns)"
  else
    echo "jt: ($2)"
  fi
  exit "${code:-1}"
}
# 1. Result cache and dedupe, before the slot: a hit or a joined identical run needs no slot. A miss makes this
# run the key's owner; the run that executes jest (this one, or the nested one on the --rdb path) stores it.
if [ -z "$JT_NESTED${TESTDECK_DISABLE:-}${TESTDECK_NO_CACHE:-}" ] && declare -F td_cache_key >/dev/null; then
  case " $* " in *" --full "*) ;; *) JT_CACHE_KEY=$(td_cache_key "$@") || JT_CACHE_KEY="";; esac
fi
if [ -z "$JT_NESTED" ] && [ -n "$JT_CACHE_KEY" ]; then
  JT_ANS=$(td_cache_lookup "$JT_CACHE_KEY")
  JT_SAID=""
  while [ "${JT_ANS%%[ $'\n']*}" = running ]; do
    [ -z "$JT_SAID" ] && { echo "jt: an identical run is in progress; joining it" >&2; JT_SAID=1; }
    JT_ANS=$(td_cache_wait "$JT_CACHE_KEY")
    case "${JT_ANS%%[ $'\n']*}" in
      hit) _jt_hit "$JT_ANS" "joined an identical run";;
      running) ;;
      *) JT_ANS=$(td_cache_lookup "$JT_CACHE_KEY");;
    esac
  done
  case "${JT_ANS%%[ $'\n']*}" in
    hit) _jt_hit "$JT_ANS" cached;;
    miss) export JT_CACHE_KEY;;
    *) JT_CACHE_KEY="";;
  esac
fi
# 5. RAM requeue (docs/PHASE2.md): outside a gate this run says it can rerun. When the broker stopped it for low RAM,
# the jt.sh that owns the ticket releases it, queues again at the head and reruns; a nested jt.sh leaves that to
# the outer one (exit 75). At most 3 requeues per run.
JT_REQUEUES=0
_jt_requeue() {
  [ -z "${GATE_EXCL_HELD:-}" ] && [ "$JT_REQUEUES" -lt 3 ] && declare -F td_requeue_pending >/dev/null \
    && td_requeue_pending || return 1
  [ -n "$JT_NESTED" ] && exit 75
  echo "testdeck: low RAM, this run was stopped and requeued" >&2
  local old=$TD_TICKET
  JT_REQUEUES=$((JT_REQUEUES + 1))
  td_release "$1"
  unset JT_SLOT_HELD
  # The last allowed requeue asks without requeue_ok: the guard then stops it as final instead of requeueing it.
  local ok=""
  [ "$JT_REQUEUES" -lt 3 ] && ok=1
  TD_REQUEUE_OK=$ok TD_REQUEUE_OF=$old slot_take
}
if [ -z "$JT_NESTED" ]; then TD_REQUEUE_OK=1 slot_take; else slot_take; fi
if [ "${1:-}" = "--rdb" ]; then
  shift
  while :; do
    RDB_JEST_GUARD=1 bash "$(dirname "${BASH_SOURCE[0]}")/rdb-lock.sh" bash "${BASH_SOURCE[0]}" "$@"
    JT_RC=$?
    _jt_requeue "$JT_RC" || break
  done
  # Gives up the claim when the nested run did not store a result (a no-op when it did).
  [ -n "$JT_CACHE_KEY" ] && td_cache_store "$JT_CACHE_KEY"
  # 75: the nested run was stopped for low RAM and printed no summary.
  [ "$JT_RC" -eq 75 ] && echo "jt: stopped by the Testdeck RAM guard and not rerun (exit 75); no test result"
  exit $JT_RC
fi

FULL=0
if [ "${1:-}" = "--full" ]; then FULL=1; shift; fi
case " $* " in *" --maxWorkers"*|*" -w "*|*" --runInBand "*|*" -i "*) ;; *) set -- --maxWorkers="${JT_WORKERS:=$( [ -n "${GATE_EXCL_HELD:-}" ] && echo 4 || slot_workers )}" "$@";; esac
echo "jt: --maxWorkers=${JT_WORKERS:-caller}" >&2

DIR="${TMPDIR:-/tmp}/jt-$$-$(date +%s)"
mkdir -p "$DIR"
JSON="$DIR/result.json"; LOG="$DIR/jest.log"
JSONP="$JSON"; command -v cygpath >/dev/null 2>&1 && JSONP="$(cygpath -w "$JSON")"
# 4. Failed-first, outside a gate: the suites that failed last time (same requester and cwd) run first. The
# sequencer only sorts, and prints the suite count for Testdeck's progress bar; skipped when the caller picks a
# sequencer, shards or only lists tests.
JT_SEQ="$(dirname "${BASH_SOURCE[0]}")/testdeck-sequencer.js"
if [ -n "${TD_TICKET:-}" ] && [ -z "${GATE_EXCL_HELD:-}" ] && [ -r "$JT_SEQ" ] && declare -F td_failed_first >/dev/null; then
  case " $* " in
    *" --testSequencer"*|*" --shard"*|*" --listTests"*) ;;
    *)
      unset TESTDECK_FIRST_FILE
      td_failed_first > "$DIR/first.txt" && [ -s "$DIR/first.txt" ] && export TESTDECK_FIRST_FILE="$DIR/first.txt"
      if command -v cygpath >/dev/null 2>&1; then
        JT_SEQ=$(cygpath -w "$JT_SEQ")
        [ -n "${TESTDECK_FIRST_FILE:-}" ] && TESTDECK_FIRST_FILE=$(cygpath -w "$TESTDECK_FIRST_FILE")
      fi
      set -- --testSequencer="$JT_SEQ" "$@";;
  esac
fi
# Testdeck counts finished suites from the PASS/FAIL lines in the log. Inside an AI agent jest 30 picks its agent
# reporter, which prints failing suites only, so ask for the default one; the summary above comes from the JSON
# result either way. Skipped when the caller picks reporters.
if [ -n "${TD_TICKET:-}" ]; then
  case " $* " in *" --reporters"*) ;; *) set -- --reporters=default "$@";; esac
fi

while :; do
[ "$JT_REQUEUES" -gt 0 ] && rm -f "$JSON"
npx jest --json --outputFile="$JSONP" "$@" > "$LOG" 2>&1
CODE=$?
[ -n "${TD_TICKET:-}" ] && _jt_requeue "$CODE" || break
done
[ "$FULL" -eq 1 ] && cat "$LOG"

if [ ! -s "$JSON" ]; then
  echo "jt: jest wrote no JSON result (exit $CODE). Last 20 log lines:"
  tail -n 20 "$LOG" | sed -E $'s/\033\\[[0-9;]*m//g'
  echo "jt: full log $LOG"
  [ -n "${JT_CACHE_KEY:-}" ] && td_cache_store "$JT_CACHE_KEY"
  exit "$CODE"
fi

{
JT_JSON="$JSONP" node -e '
const fs = require("fs"), path = require("path");
const r = JSON.parse(fs.readFileSync(process.env.JT_JSON, "utf8"));
const strip = s => String(s || "").replace(/\u001b\[[0-9;]*m/g, "");
const first = s => strip(s).split("\n").map(l => l.trim()).filter(l => l && !/^at /.test(l)).slice(0, 3).join(" | ").slice(0, 300);
const rel = f => path.relative(process.cwd(), f).replace(/\\/g, "/");
let shown = 0, more = 0;
for (const t of r.testResults || []) {
  const fails = (t.assertionResults || []).filter(a => a.status === "failed");
  if (t.status === "failed" && fails.length === 0) {
    if (shown < 30) { console.log(`SUITE FAIL ${rel(t.name)}: ${first(t.message)}`); shown++; } else more++;
  }
  for (const a of fails) {
    if (shown < 30) { console.log(`FAIL ${rel(t.name)} > ${a.fullName}: ${first((a.failureMessages || [])[0])}`); shown++; } else more++;
  }
}
if (more) console.log(`... ${more} more failures not shown`);
const n = (v, w) => v ? `${v} ${w}, ` : "";
console.log(`Test Suites: ${n(r.numFailedTestSuites, "failed")}${n(r.numPendingTestSuites, "skipped")}${r.numPassedTestSuites} passed, ${r.numTotalTestSuites} total`);
console.log(`Tests:       ${n(r.numFailedTests, "failed")}${n(r.numPendingTests + (r.numTodoTests || 0), "skipped")}${r.numPassedTests} passed, ${r.numTotalTests} total`);
'
if grep -q '"numTotalTestSuites":0' "$JSON"; then
  echo "jt: no suites ran. Last 5 log lines:"; tail -n 5 "$LOG" | sed -E $'s/\033\\[[0-9;]*m//g'
fi
[ "$CODE" -ne 0 ] && echo "jt: exit $CODE, full log $LOG"
} | tee "$DIR/summary.txt"

# Testdeck tag lines after the summary (appended to summary.txt too, so a cache hit replays them), then the store.
_jt_tag() { echo "$1"; echo "$1" >> "$DIR/summary.txt"; }
# _jt_failed <json path (Windows form)> [status]: the suites with that status (default failed), relative to $PWD
# with slashes, one per line.
_jt_failed() {
  JT_JSON="$1" JT_STATUS="${2:-failed}" node -e '
const fs = require("fs"), path = require("path");
let r = {};
try { r = JSON.parse(fs.readFileSync(process.env.JT_JSON, "utf8")); } catch (e) { process.exit(1); }
for (const t of r.testResults || []) if (t.status === process.env.JT_STATUS) console.log(path.relative(process.cwd(), t.name).replace(/\\/g, "/"));
'
}
_jt_after() {
  local -a failing=() passed=() still=() cfg=()
  local s flaky rj rjp
  # Config-selecting arguments stay in the rerun, so the suites run under the same config.
  while [ $# -gt 0 ]; do
    case "$1" in
      --config=*|--rootDir=*|--selectProjects=*|--projects=*) cfg+=("$1");;
      --config|-c|--rootDir|--selectProjects|--projects) cfg+=("$1" "${2:-}"); shift;;
    esac
    shift
  done
  [ "$CODE" -ne 0 ] && mapfile -t failing < <(_jt_failed "$JSONP")
  still=("${failing[@]}")
  # 2. Flaky rerun: when every failing suite is on the flaky list, rerun only those once, alone and in band.
  if [ ${#failing[@]} -gt 0 ] && flaky=$(td_flaky_list) && [ -n "$flaky" ]; then
    for s in "${failing[@]}"; do td_in_list "$s" "$flaky" || { flaky=""; break; }; done
    if [ -n "$flaky" ]; then
      rj="$DIR/rerun.json"; rjp="$rj"; command -v cygpath >/dev/null 2>&1 && rjp="$(cygpath -w "$rj")"
      npx jest --json --outputFile="$rjp" --runInBand "${cfg[@]}" --runTestsByPath "${failing[@]}" > "$DIR/rerun.log" 2>&1
      # Only a suite the rerun lists as passed counts as passed; a missing result file or a suite that did not run
      # counts as failed twice.
      [ -s "$rj" ] && mapfile -t passed < <(_jt_failed "$rjp" passed)
      still=()
      for s in "${failing[@]}"; do
        if printf '%s\n' "${passed[@]}" | grep -qxF -- "$s"; then
          _jt_tag "jt: flaky, passed on rerun: $s"
        else
          still+=("$s"); _jt_tag "jt: flaky, failed twice: $s"
        fi
      done
      [ ${#still[@]} -eq 0 ] && CODE=0
    fi
  fi
  # 3. Known red: tag the failures that also fail on staging. A gate run on a clean origin/staging adds its failures.
  if [ ${#still[@]} -gt 0 ]; then
    local red head
    if red=$(td_known_red) && [ -n "$red" ]; then
      for s in "${still[@]}"; do td_in_list "$s" "$red" && _jt_tag "jt: known red on staging: $s"; done
    fi
    if [ -n "${GATE_EXCL_HELD:-}" ] && head=$(git rev-parse HEAD 2>/dev/null) \
      && [ "$head" = "$(git rev-parse origin/staging 2>/dev/null)" ] && [ -z "$(git status --porcelain 2>/dev/null)" ]; then
      td_known_red_add "${still[@]}"
    fi
  fi
  [ -n "${JT_CACHE_KEY:-}" ] && td_cache_store "$JT_CACHE_KEY" "$CODE" "$DIR/summary.txt"
  return 0
}
[ -n "${TD_TICKET:-}" ] && declare -F td_flaky_list >/dev/null && _jt_after "$@"
exit "$CODE"

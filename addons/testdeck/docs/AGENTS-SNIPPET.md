## Tests and heavy builds (machine-wide queue: Testdeck)

This PC runs a machine-wide queue for heavy work. It applies to every agent (Claude Code, Codex, Gemini and others).

- Run jest only through `bash ~/.claude/gate/jt.sh [--rdb] <jest args>`, from the directory that owns the jest config.
- Run every other heavy command (`next build`, a full `tsc`, lint of a whole package, `npm run build`) as
  `bash ~/.claude/gate/heavy.sh <command>`.
- Never run these commands raw, and never run several of them in parallel to get around the queue.

What the output lines mean:
- `queue: waiting in Testdeck, position P, about S s` or `queue: waiting for a free test slot`: your job waits for a
  slot. This is normal. Wait; do not cancel and retry.
- `testdeck: request cancelled in the Testdeck queue`: the owner cancelled it. Do not retry unless asked.
- `testdeck: lost the broker while waiting; using the file queue`: Testdeck stopped; the old file queue takes over.
- `testdeck: low RAM, this run was stopped and requeued`: the run restarts by itself. Keep waiting.
- jt.sh prints one `FAIL <file> > <test>: <error>` line per failing test, then the jest `Tests:` and `Test Suites:`
  lines and the path of the full log. Read a single failure with `grep -n -A 30 "<test name>" <log path>`.
- Lines starting with `jt: ` follow the summary: `jt: (cached <age> ago)` (a stored result for the same code),
  `jt: (joined an identical run)`, `jt: flaky, passed on rerun: <suite>`, `jt: flaky, failed twice: <suite>`,
  `jt: known red on staging: <suite>` (also fails on a clean staging, not caused by you).

How to wait:
- Claude Code: a hook decides. When a slot is free your command runs in the foreground with a 10-minute timeout;
  when it would queue, the hook moves it to the background. Then wait for the completion notification; do not poll.
- Other agents: run these commands in the background or with a long timeout (at least 15 minutes), and wait for
  them to finish. Do not poll in a tight loop.

Environment:
- `TESTDECK_REQUESTER=<your batch or thread name>` names you in the queue (default: the worktree folder).
- `TESTDECK_NO_CACHE=1` forces a real run instead of a cached result.

Never kill, cancel or reorder another agent's job, and never delete files under `$TMPDIR/jt-slots`.

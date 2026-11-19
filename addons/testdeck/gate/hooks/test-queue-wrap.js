// PreToolUse(Bash): machine-wide test queue (owner rule 2026-11-17).
// A raw heavy command (jest, next build, tsc, eslint, npm test/typecheck/build/lint) is rewritten to run inside
// ~/.claude/gate/heavy.sh, so it waits for a queue slot; raw jest also gets the queue's worker count.
// Every other command is passed to the RTK hook unchanged (this hook replaces the direct "rtk hook claude" entry).
// Hook decides (Testdeck, PROMPT.md Phase 3 item 4): for a wrapped command or one that already calls jt.sh/heavy.sh,
// ask Testdeck's /status. Slot free and queue empty: run in the foreground with a 10-minute timeout. Queued: run in
// the background so the agent gets a completion notification instead of a Bash timeout. Testdeck down: as before.
const { spawnSync } = require('child_process');
const http = require('http');

const HEAVY = /^(?:[A-Za-z_][A-Za-z0-9_]*=\S*\s+)*(?:npx\s+(?:jest|next\s+build|tsc|eslint)\b|next\s+build\b|tsc\b|npm\s+(?:run\s+)?(?:test|typecheck|build|lint)\b|npm\s+run\s+typecheck:\S+)/;
const SKIP = /jt\.sh|heavy\.sh|gate\.sh|rdb-lock|--listTests|--version|--help|--showConfig/;
const WORKER_FLAG = /--maxWorkers|\s-w\s|--runInBand|\s-i(\s|$)/;

// Command segments split on && || ; | and newlines outside quotes, so a | inside grep "a|b" is not a pipe.
function segmentsOf(cmd) {
  const out = []; let cur = ''; let q = null;
  for (let i = 0; i < cmd.length; i++) {
    const c = cmd[i];
    if (q) { if (c === q) q = null; else if (c === '\\' && q === '"') { cur += c + (cmd[++i] ?? ''); continue; } cur += c; continue; }
    if (c === '"' || c === "'") { q = c; cur += c; continue; }
    if (c === ';' || c === '|' || c === '\n' || (c === '&' && cmd[i + 1] === '&')) { out.push(cur.trim()); cur = ''; if (cmd[i + 1] === c) i++; continue; }
    cur += c;
  }
  out.push(cur.trim());
  return out;
}

function wrap(cmd) {
  if (SKIP.test(cmd)) return null;
  const segments = segmentsOf(cmd);
  if (!segments.some((s) => HEAVY.test(s))) return null;
  let inner = cmd;
  if (!WORKER_FLAG.test(cmd)) inner = inner.replace(/\bnpx\s+jest\b/g, 'npx jest --maxWorkers=${JT_WORKERS:-2}');
  return `bash ~/.claude/gate/heavy.sh bash -c '${inner.replace(/'/g, "'\\''")}'`;
}

// A segment that runs jt.sh/heavy.sh (optionally behind env vars, timeout or bash), not one that only mentions it (echo, grep).
// Needs "bash <path>" or a path with a slash, so a regex alternative like grep "a|jt.sh" (split on its |) is no call.
const CALLS_GATE_SEG = /^\(?\s*(?:[A-Za-z_][A-Za-z0-9_]*=\S*\s+)*(?:timeout\s+\S+\s+)?(?:bash\s+\S*?|\S*\/)(?:jt|heavy)\.sh\b/;
const callsGate = (cmd) => segmentsOf(cmd).some((s) => CALLS_GATE_SEG.test(s));

// Testdeck's /status, or null when it does not answer within 300 ms.
function status(port) {
  return new Promise((resolve) => {
    const req = http.get({ host: '127.0.0.1', port, path: '/status' }, (res) => {
      let body = '';
      res.setEncoding('utf8');
      res.on('data', (d) => (body += d));
      res.on('end', () => { try { resolve(res.statusCode === 200 ? JSON.parse(body) : null); } catch { resolve(null); } });
    });
    req.on('error', () => resolve(null));
    setTimeout(() => { req.destroy(); resolve(null); }, 300).unref();
  });
}

// The tool-input fields to change and an optional note for the agent, or null when nothing changes.
function decide(st, toolInput) {
  if (!st || toolInput.run_in_background) return null;
  const queue = Array.isArray(st.queue) ? st.queue : [];
  const running = Array.isArray(st.running) ? st.running : [];
  // shortcut: /status reports no slot count; assume JT_SLOTS (default 2), 1 in user mode, until it sends free_slots.
  const slots = st.user_mode ? 1 : Number(process.env.JT_SLOTS) || 2;
  const free = typeof st.free_slots === 'number' ? st.free_slots > 0 : running.length < slots;
  const blocked = st.paused || st.gate_asking || running.some((r) => r.kind === 'gate');
  if (free && !blocked && queue.length === 0) return toolInput.timeout ? null : { input: { timeout: 600000 } };
  const eta = queue.length ? queue[queue.length - 1].eta_secs : null;
  const about = typeof eta === 'number' ? `, about ${eta} s` : '';
  return {
    input: { run_in_background: true },
    note: `Queued in Testdeck (position ${queue.length + 1}${about}); this command runs in the background; wait for its completion notification, do not poll.`,
  };
}

let raw = '';
process.stdin.on('data', (d) => (raw += d));
process.stdin.on('end', async () => {
  let input;
  try { input = JSON.parse(raw); } catch { process.exit(0); }
  const cmd = input?.tool_input?.command;
  const wrapped = typeof cmd === 'string' ? wrap(cmd) : null;
  const heavy = !!wrapped || (typeof cmd === 'string' && callsGate(cmd));
  const d = heavy ? decide(await status(Number(process.env.TESTDECK_PORT) || 47123), input.tool_input) : null;
  let out;
  if (wrapped) {
    out = {
      hookSpecificOutput: {
        hookEventName: 'PreToolUse',
        permissionDecisionReason: 'test queue: heavy command runs through ~/.claude/gate/heavy.sh',
        updatedInput: { ...input.tool_input, command: wrapped },
      },
    };
  } else {
    const r = spawnSync('rtk', ['hook', 'claude'], { input: raw, encoding: 'utf8', shell: true });
    if (r.stderr) process.stderr.write(r.stderr);
    try { out = d && (r.status ?? 0) === 0 ? (r.stdout ? JSON.parse(r.stdout) : {}) : null; } catch { out = null; }
    if (!out) {
      if (r.stdout) process.stdout.write(r.stdout);
      process.exit(r.status ?? 0);
    }
  }
  if (d) {
    const h = (out.hookSpecificOutput ??= { hookEventName: 'PreToolUse' });
    h.updatedInput = { ...(h.updatedInput || input.tool_input), ...d.input };
    if (d.note) h.additionalContext = d.note;
  }
  process.stdout.write(JSON.stringify(out));
  process.exit(0);
});

if (require.main === module && process.argv[2] === '--self-test') {
  const t = (c, want) => { const got = wrap(c); if (!!got !== want) { console.error('FAIL', c, got); process.exitCode = 1; } };
  t('npx jest foo', true); t('cd tests && npx jest foo', true); t('npm run typecheck', true); t('cd client && npx next build', true);
  t('git status', false); t('bash ~/.claude/gate/jt.sh foo', false); t('git commit -m "run npx jest"', false); t('npx jest --listTests', false);
  if (!wrap('npx jest a').includes('--maxWorkers=${JT_WORKERS:-2}')) { console.error('FAIL workers'); process.exitCode = 1; }
  const g = (c, want) => { if (callsGate(c) !== want) { console.error('FAIL callsGate', c); process.exitCode = 1; } };
  g('bash ~/.claude/gate/jt.sh foo', true); g('cd tests && timeout 120 bash ~/.claude/gate/jt.sh x', true); g('X=1 bash heavy.sh npx tsc', true);
  g('(cd tests && bash ~/.claude/gate/jt.sh a)', true); g('echo "jt.sh now refuses" >> mem.md', false); g('grep -n jt.sh README.md', false); g('cp jt.sh jt.sh.bak', false);
  g('git ls-files | grep -E "client|slots.sh|jt.sh"', false);
  t('grep -iE "tsc|jt\\.sh|npm run typecheck" a.md | head', false); t('cd server && npm run typecheck | tail -3', true); g('bash jt.sh a', true); g('~/.claude/gate/jt.sh a', true);
  const dt = (st, ti, want) => { const got = JSON.stringify(decide(st, ti)?.input ?? null); if (got !== want) { console.error('FAIL decide', got, want); process.exitCode = 1; } };
  dt(null, {}, 'null'); dt({ running: [], queue: [] }, {}, '{"timeout":600000}'); dt({ running: [], queue: [] }, { timeout: 5 }, 'null');
  dt({ running: [{}, {}], queue: [] }, {}, '{"run_in_background":true}'); dt({ running: [], queue: [{ eta_secs: 9 }] }, {}, '{"run_in_background":true}');
  dt({ running: [{}, {}], queue: [] }, { run_in_background: true }, 'null');
  console.log(process.exitCode ? 'self-test FAIL' : 'self-test ok');
  process.exit(process.exitCode || 0);
}

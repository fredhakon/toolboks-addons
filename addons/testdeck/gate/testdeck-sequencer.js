// testdeck-sequencer.js: jest testSequencer that jt.sh passes (Testdeck failed-first, docs/PLAN.md). It runs the
// suites listed in $TESTDECK_FIRST_FILE (one absolute path per line: the suites that failed last time for the same
// requester and cwd) first and keeps the default order otherwise. It only sorts: which suites run, and shard(), stay
// those of the project's own @jest/test-sequencer. It also prints how many suites the run has, for Testdeck's progress
// bar: jest's default reporter prints no total before the end.
'use strict';
const fs = require('fs');
const mod = require(require.resolve('@jest/test-sequencer', { paths: [process.cwd()] }));
const Base = mod.default || mod;

const norm = p => String(p).replace(/\\/g, '/').toLowerCase();
let first = new Set();
try {
  first = new Set(fs.readFileSync(process.env.TESTDECK_FIRST_FILE || '', 'utf8').split(/\r?\n/).filter(Boolean).map(norm));
} catch (e) {
  // No list: the default order.
}

const failedFirst = tests => {
  if (!first.size) return tests;
  const hit = t => first.has(norm(t.path));
  return [...tests.filter(hit), ...tests.filter(t => !hit(t))];
};

class TestdeckSequencer extends Base {
  sort(tests) {
    process.stderr.write(`testdeck: suites ${tests.length}
`);
    const sorted = super.sort(tests);
    return sorted && typeof sorted.then === 'function' ? sorted.then(failedFirst) : failedFirst(sorted);
  }
}

module.exports = TestdeckSequencer;

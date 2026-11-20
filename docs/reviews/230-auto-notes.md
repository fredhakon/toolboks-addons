# Issue 230: automatic triggers, notes for the PR

What was built (add-on only, `addons/memory-cleaner/src/auto_triggers.rs`):

- Memory-limit trigger: physical memory in use `>=` limit (1 to 99, presets 70/80/90/95), at most
  once per 30 s. Interval trigger: every N minutes (1 to 1440, presets 5/15/30/60/120), no memory
  check. The limit is checked first. Both are off by default and have a Disable entry.
- Both use the injected-clock pure decision `AutoTriggers::tick`, wait while commit is at or above
  the scheduler's `COMMIT_NEAR_LIMIT`, and share one "last automatic run" with the pressure
  scheduler, so no automatic run follows another faster than its own cooldown (30 s for the
  limit, the interval itself for the interval, which also restarts after any automatic run).
- Settings persist under the preference key `auto_triggers` (an object; the all-off default is
  not written). Out of range numbers load clamped, anything unreadable loads as off.
- Controls and notes are on the Advanced page, Rules tab, under the existing automatic status.
  They carry the "Unproven" note and the existing trim evidence sentence.

Why no clean region runs automatically yet:

- Every one of the eight regions (`regions.rs`) is a system-wide call that only runs through the
  administrator helper, which shows a Confirm window. An unattended run must never show it.
  Region 1 (working set, all apps) is also a helper call; the safe per-app trim the scheduler
  uses is a different action and is what the two new triggers run.
- The opt-in "Allow standby and modified lists in automatic cleaning" is stored (off by default,
  with the freeze warning) and `auto_regions` filters the ticked regions: lists 4 and 5 are out
  unless the switch is on, and anything that needs the helper is out always. Today that leaves
  nothing; the Rules tab says so. When a region can run without the helper, only
  `needs_helper` changes.

Host: nothing needed.

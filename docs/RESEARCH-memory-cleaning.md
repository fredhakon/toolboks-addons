# Memory cleaning: does it help? (research and benchmark protocol)

Status: MEASURED ON ONE PC (32 GB, 2026-11-16), unattended, `none` and `trim` arms only. The
`purge` arm and the 8 GB class are not measured, and no game frame times were captured (see
Results). The result: no benefit from trimming could be shown, and a cost on next use was.
Decisions are in the last section.

This file is cited by `addons/memory-cleaner/src/pressure.rs` (thresholds) and
`addons/memory-cleaner/src/scheduler.rs` (automatic-mode rules). Issue: #32. Evidence labels
follow issue #33: **Measured**, **Mixed**, **Unproven**.

## Why

- Trimming a working set does not free memory. Windows moves the pages to the standby (cache)
  list or the modified list. They stay in RAM and a later access takes them back (a soft fault,
  or a hard fault if the page had to be written out and read again).
- "Available" memory in Windows already counts the standby list. After a trim it can look better
  on paper while the PC holds the same data.
- Commit charge does not change when memory is trimmed. A PC that is near its commit limit is
  not helped by trimming (`scheduler.rs` advises closing apps there instead of running).
- Windows already trims and reuses cache when it needs memory, so a manual action may only do
  earlier what the system would do anyway, at the cost of faults when the app is used again.
- The thresholds in `pressure.rs` (available below max(10% of RAM, 1.5 GiB), commit 85% busy and
  90% pressure, Pages Input/sec at or above 300, compressed memory growth, the hold times and the
  one-run-per-hour cooldown) are a synthesis of general Windows practice. They are NOT
  Microsoft guidance and NOT measured facts.
- We searched for a controlled benchmark of working-set trimming or standby-list purging on
  modern Windows (launch time, faults, frame times) and found none that we could rely on. This
  document does not cite one. Claims below are therefore Unproven until our own tables are full.

## The four arms

Each arm is what the owner presses in Memory Cleaner between the "before" and "after" samples.

| Arm | Script `-Arm` | What it does | Admin | Code |
|---|---|---|---|---|
| Nothing | `none` | Control. No action, same waits and samples. | no | n/a |
| Memory priority only | not offered | Lowers the memory priority of idle background apps (`MEMORY_PRIORITY_LOW`) so Windows reclaims their pages first. Verifies pid, start time and image name before each app; undoable. | no | `actions.rs` |
| Targeted trim | `trim` | The **Trim idle apps** button on the Home page: lowers memory priority and then trims (`EmptyWorkingSet`, per app and per pid) the idle background apps that pass the idle policy. No system-wide empty. Pages go to the cache. | no | `engine.rs`, `actions.rs`, `trim.rs` |
| Low-priority cache purge | `purge` | System-wide purge of only the lowest-priority standby pages (`SystemMemoryListInformation` command 5). A full standby purge, modified-list flush and empty-all are deliberately not offered. | yes, host admin helper (Confirm / Cancel window) | `memlist.rs` |

**Priority-only cannot be measured separately.** The UI has no priority-only action: the one
manual button, "Trim idle apps" (`home.rs`), runs the plan from `engine.rs`, which lowers priority
and then trims. The only other related control is "Undo priority change". So the `trim` arm is
really "priority then trim", and the `priority` arm is not measurable until a build switch that
runs the priority step alone exists (follow-up, not part of this issue). The script rejects
`-Arm priority`.

**Which trim path.** Use only **Trim idle apps** for the trim arm. It applies the idle policy
(`Policy::manual` in `actions.rs`): the app in front, apps playing audio, full-screen apps, game
launchers and anti-cheat, protected and excluded apps are skipped; an app must have had no input
for 60 s and hold at least 100 MB; an app trimmed by the user in the last 10 minutes is skipped.
The Advanced page's **Trim selected** is different: it trims exactly the processes the user
selected and checks only `skip_reason` (the app itself, system, game, launcher and anti-cheat
processes, a launcher's child, a process whose parent cannot be checked, and excluded apps). It
does not check foreground, idle time or audio. It is a different action and is not part of this
benchmark.

## Metrics

Collected by `scripts/bench-memory.ps1` (one CSV row per run, before and after the action):

- Available MB, commit %, standby (cache) MB, Page Faults/sec and Pages Input/sec (the hard
  fault proxy: pages read from disk).
- App launch time after the action: milliseconds from `Start-Process` until the process the
  script started has a main window. Only that process is closed afterwards. If the app hands over
  to another instance the value is NA; pick another `-App`.
- The sample window: all counters come from one `Get-Counter` call (3 one-second samples,
  averaged). The CSV records when each window started (`before_sampled_at`, `after_sampled_at`)
  and `sample_seconds`. If the counters fail, `sample_mode` says `cim` and only Available MB and
  commit % are filled.

Collected by hand:

- Responsiveness: notes on stutter when switching back to the apps that were trimmed. Note, do
  not score.
- Game 1% low frame times around the action: capture with PresentMon, or the Xbox Game Bar
  performance widget if available. Play the same fixed scene for 5 minutes, press the action
  once in the middle, compare 1% low and average FPS before and after. Only for machines that
  have a game; write `not measured` and the reason otherwise.

## Method

1. Reboot. Log in, start nothing extra, wait for startup activity to settle.
2. Idle 5 minutes, then load the fixed workload (the same apps and documents on every run,
   written down below the tables: for example browser with N tabs, chat app, editor, a launcher).
3. Run `scripts/bench-memory.ps1 -Arm <none|trim|purge> -Runs 5 -IdleSeconds 660 -Wait 30`.
   When it prints `NOW`, press the matching button within 30 seconds (arm `none`: do nothing).
   For `trim` press **Trim idle apps**, not Trim selected.
4. **Spacing of runs.** Manual trims skip an app trimmed in the last 10 minutes
   (`MANUAL_COOLDOWN` in `actions.rs`). `-IdleSeconds 660` (11 minutes) before every run keeps
   each run beyond that cooldown, and gives the apps the idle minute the policy needs. If you
   shorten it, quit and restart Memory Cleaner between runs instead (its cooldowns live in
   memory only).
5. **Confirm the action happened.** After every `trim` run read the **Last run** card on the Home
   page (what moved to cache, or "nothing to do"). After every `purge` run read the purge result
   text. Fill the `action_confirmed` column of that CSV row with `yes` or `no`. A `no` run (the
   tool had nothing eligible, or the purge failed) is repeated, not counted. Note the Last run
   numbers in the notes next to the table.
6. 5 confirmed runs per arm. Randomise the arm order per machine (for example draw from a
   shuffled list); reboot between arms so one arm does not warm the next.
7. Same machine for all arms. Record RAM size, Windows build, CPU, disk type (the script writes
   RAM and build into every row).
8. Games: PresentMon or Game Bar capture per run as described above.

Analysis: median per arm and metric, plus the spread (min to max) of the `none` arm as the
noise band. Do not average away outliers; list them.

Run the script from a normal (non-admin) Windows PowerShell:

```
powershell -ExecutionPolicy Bypass -File scripts\bench-memory.ps1 -Arm none -Runs 5 -IdleSeconds 660 -Wait 30
```

Output: `%TEMP%\toolboks-bench\bench-memory.csv` (override with `-OutDir`). The script does not
perform the action, needs no administrator rights, uses no network and writes only under
`-OutDir`. `-DryRun` prints the plan and exits.

For a run without a person at the PC (no button press, no window, no UAC), use
`scripts\bench-memory-driven.ps1`: it runs this script with `-Headless` and drives the `none` and
`trim` arms over benchmark-owned processes only. See "Unattended runs" below.

## Unattended runs (what was actually done)

The owner's PC was in use (a fullscreen game was running and the owner was at the keyboard), and
no UAC prompt may be clicked. So the arms were driven without a window and without the owner,
with `scripts/bench-memory-driven.ps1`. Raw files: `docs/bench-results/2026-11-16-32gb-pc/`
(`bench-memory.csv` is the script's usual output, `bench-driven.csv` has the re-read times and the
trim numbers). Differences from the method above, all deliberate:

- **What the trim touched.** Not the owner's apps. "Trim idle apps" trims every idle app that
  passes the policy, which on this PC means the owner's browser and chat apps. The driver instead
  starts 3 benchmark-owned child processes (the add-on's test exe, `bench::` tests in
  `addons/memory-cleaner/src/bench.rs`, compiled only for `cargo test`, never in the add-on exe),
  each holding 300 MB (900 MB in total) and idle. The trim step runs the add-on's own manual code
  (`Policy::manual`, `actions::plan`, `actions::run` with `WindowsOps`: lower memory priority,
  then `EmptyWorkingSet`) over those 3 processes only. It is handed their list and never
  enumerates other processes. It differs from the button in one way: the candidate list comes from
  the children, and their idle time is the real time since the script last touched them (at least
  90 s), because they have no window or input. In every trim run it reported 3 lowered, 3
  trimmed, 0 failed, with their working set going from 903-925 MB to 0-1 MB. `action_confirmed`
  is `yes` for all 5 trim runs.
- **Spacing.** `-IdleSeconds 90` instead of 660, and a fresh process per trim (so the 10-minute
  manual cooldown, which lives in memory, never applied). Wait before the "after" sample: 15 s.
- **Order.** The 10 runs (5 `none`, 5 `trim`) were interleaved in a shuffled order (none, none,
  trim, trim, none, none, trim, trim, none, trim) on one boot, so drift hits both arms. No reboot
  between arms (the owner's PC).
- **Launch time.** Notepad would open a window over the game, so `-Headless` was used: a hidden
  `powershell.exe -NoProfile -Command exit`, timed until it exits. Not comparable with
  window-ready times.
- **Extra metric, the cost of next use.** After each run, every child re-reads all of its memory
  (one read per 4 KB page) and times it: `retouch_ms`, summed over the 3 children. This is what an
  app pays when it is used again after a trim.
- **Not done.** `purge` (see below), the 8 GB class, game frame times (no PresentMon capture).

## Results

Machine: 32 GB RAM (the **16 GB+** class), Windows 11 build 26300.9818, Intel i7-10700K, SSDs
(plus one HDD), page file on C:. Workload: 3 idle benchmark processes x 300 MB. Ambient load: a
game of about 11 GB was running and in use, so system-wide counters are dominated by the game
(Available MB swung from about 340 MB to 16 GB between samples; Pages Input/sec from 1,400 to
19,000). Each cell is `median (min-max)` over 5 confirmed runs.

### App launch time (ms; headless: hidden powershell start to exit), `none` arm = noise band

| Arm | 8 GB | 16 GB+ (32 GB PC) |
|---|---|---|
| none | not measured: no 8 GB PC | 437 (357-999) |
| trim | not measured: no 8 GB PC | 397 (321-6315) |
| purge | not measured: needs administrator confirmation; stays manual, admin only, Unproven | not measured: needs administrator confirmation; stays manual, admin only, Unproven |

The 40 ms difference of the medians is far inside the noise (the `none` spread is 642 ms and one
`trim` run took 6.3 s). No effect.

### Pages Input/sec after the action (hard fault proxy)

| Arm | 8 GB | 16 GB+ (32 GB PC) |
|---|---|---|
| none | not measured: no 8 GB PC | 5227 (1471-19059) |
| trim | not measured: no 8 GB PC | 1739 (409-4095) |
| purge | not measured: needs administrator confirmation; stays manual, admin only, Unproven | not measured: needs administrator confirmation; stays manual, admin only, Unproven |

The lower `trim` median is not a result: the "before" values (median 2176 for the `none` runs,
1725 for the `trim` runs) and the spreads show the game's own disk reads decide this counter, and
the ranges overlap.

### Available MB and standby MB, change after the action (after minus before)

| Arm | 8 GB | 16 GB+ available | 16 GB+ standby |
|---|---|---|---|
| none | not measured: no 8 GB PC | -213 (-1098 to +2123) | +164 (-1072 to +675) |
| trim | not measured: no 8 GB PC | -1180 (-1773 to +3096) | -54 (-1238 to +1195) |
| purge | not measured: needs administrator confirmation; stays manual, admin only, Unproven | not measured: needs administrator confirmation; stays manual, admin only, Unproven | not measured: needs administrator confirmation; stays manual, admin only, Unproven |

Trimming moved the children's 900 MB out of their working sets (903-925 MB to 0-1 MB, in every
trim run). The system-wide counters do not show it: changes of +/-1 GB happened in `none` runs
too. Trimming does not free memory, it moves it to the cache list, which "Available" already
counts.

### Cost on next use: re-reading the 900 MB afterwards (ms, sum of 3 children)

| Arm | 16 GB+ (32 GB PC) |
|---|---|
| none | 86 (24-4189) |
| trim | 479 (251-2281) |

The five `none` runs: 73, 24, 87, 86 ms and one outlier of 4189 ms. In that run the PC was short
of memory (Available about 340 MB in its "after" sample) and Windows itself had already moved the
idle children's pages out: the case a trim is meant for, done by the system without help. The five
`trim` runs: 279, 479, 251, 2154, 2281 ms. Every `trim` run was about 3 times or more slower than the
four ordinary `none` runs, and two took about 2.2 s (hard faults under the game's memory demand).
The next use of a trimmed app pays this price; no run showed a gain to set against it.

### Game 1% low frame time (ms) and average FPS around the action

| Arm | 8 GB | 16 GB+ |
|---|---|---|
| none | not measured: no 8 GB PC | not measured: no capture tool run, and no fixed scene was played |
| trim | not measured: no 8 GB PC | not measured: no capture tool run, and no fixed scene was played |
| purge | not measured: needs administrator confirmation; stays manual, admin only, Unproven | not measured: needs administrator confirmation; stays manual, admin only, Unproven |

### The `purge` arm

Not measured: needs administrator confirmation; stays manual, admin only, Unproven. The purge
runs through the host's administrator helper, which shows a UAC / Confirm window. The owner
decided that no test may click or trigger one, so there is no number, and nothing here supports
or argues against it.

### Machines and workload

| Class | RAM | Windows build | CPU / disk | Workload |
|---|---|---|---|---|
| 8 GB | not measured: no 8 GB PC available | n/a | n/a | n/a |
| 16 GB+ | 32 GB | 26300.9818 | Intel i7-10700K, SSDs plus one HDD, page file on C: | 3 idle benchmark processes x 300 MB; a game of about 11 GB in front, a browser and chat apps in the background |

### What these numbers can and cannot say

- **Noise is larger than any effect in the system-wide counters and in launch time.** The machine
  was not quiet, so Available, standby, Pages Input/sec and launch time cannot show a difference
  of the size involved (900 MB of working set, on a PC with 32 GB).
- **The one clean signal is the cost on next use**, measured inside the benchmark processes
  themselves: a trimmed process re-reads its memory roughly 3 to 6 times slower (median 479 ms against 86
  ms; at least 251 ms against at most 87 ms if the single `none` outlier is left out), and
  sometimes much slower.
- **One PC, one boot, 5 runs per arm, 32 GB.** It says nothing about an 8 GB PC under constant
  pressure, which is where a benefit is most likely. That is still open (follow-up issue).

## Decision rule (per action)

An action (trim, purge) is **kept** only if, against the `none` arm, on BOTH RAM
classes:

1. the action was confirmed to have happened in each counted run (Last run card or purge
   result), and the median improvement in app launch time or Pages Input/sec is beyond the noise band of
   the `none` arm, and
2. there is no regression in game 1% low frame times (and none in launch time of the apps that
   were trimmed), and
3. the cost shows up honestly: if the next-use faults rise after a trim, that counts against it.

Otherwise it is dropped, or kept as opt-in only and labelled **Unproven** in the UI and docs.
If it helps on one RAM class only, it stays opt-in and the label says which class
(**Mixed**).

Evidence labels (issue #33): **Measured** = the rule above passed on both classes with the
tables filled; **Mixed** = passed on one class or the metrics disagree; **Unproven** = no
measurement, or no improvement beyond noise.

## Decision (the rule above applied to the 2026-11-16 measurements)

Rule check for **trim**, against `none`, on the 16 GB+ class (the only one measured):

1. Confirmed to have happened in each counted run: yes (5 of 5, 3 apps trimmed each). Improvement
   in launch time or Pages Input/sec beyond the `none` noise band: **no** (the differences are
   inside the noise).
2. No regression in game 1% lows: not measured. No effect on launch time of the trimmed apps
   either way.
3. Honest cost: next-use faults rose (re-read 479 ms against 86 ms median). That counts against it.

So the rule is **not** met. Nothing can be called Measured or Mixed, and nothing here is a benefit.

Current behaviour, unchanged by this issue:

- None of the four is on by default. Automatic cleaning is off by default
  (`scheduler.rs`, `home.rs` text "Off by default").
- Memory priority and targeted trim run together, only when the user presses "Trim idle apps"
  (or uses Trim selected on the Advanced page), or, if the user turns it on, in automatic mode after sustained pressure
  (60 s hold, at most once an hour, never the foreground app, never near the commit limit).
- The low-priority cache purge is manual and needs administrator rights through the host helper.
- The report wording after a run never says memory was freed; it shows what moved to cache and
  what came back (`report.rs`), and the Home note says "It does not free memory".

| Action | Label | Decision | Numbers next to it |
|---|---|---|---|
| Nothing | n/a (control) | n/a | `none`: launch 437 ms, re-read 86 ms (medians) |
| Memory priority only | Unproven | Not separately measurable (no priority-only action). Stays inside Trim idle apps. No benefit claimed. | n/a |
| Targeted trim (with priority) | Unproven | **Keep only as an opt-in manual action** the user presses on purpose, never presented as a speed-up. No benefit shown; a cost on next use is. Automatic mode has no evidence behind it and stays off by default; whether to keep it at all is the owner's product decision (follow-up issue). | launch 397 vs 437 ms and Pages Input 1739 vs 5227/s: inside the noise. Re-read after trim 479 vs 86 ms (cost). |
| Low-priority cache purge | Unproven | Keep manual, administrator only. | Not measured: needs administrator confirmation; stays manual, admin only, Unproven. |

No UI label or copy changes with this decision: every action is already off by default or
manual, no text claims a benefit, and Home says trimming "does not free memory". The evidence
badges per action belong to issue #33; they would all read **Unproven**.

Nothing in this document or in the add-on may claim a benefit until a re-run (8 GB class, a quiet
machine, and the purge arm with an administrator confirmation) passes the decision rule.

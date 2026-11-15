# Memory cleaning: does it help? (research and benchmark protocol)

Status: PROTOCOL READY, NO MEASUREMENTS YET. Every result cell below is `TBD (owner PC)`.
The decisions in the last section are provisional until those tables are filled.

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
selected and checks only `skip_reason` (the app itself, protected and excluded apps). It does not
check foreground, idle time, audio or launchers. It is a different action and is not part of
this benchmark.

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
  have a game; leave `TBD (owner PC)` otherwise.

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

## Results

Median over 5 confirmed runs per arm. Fill with `median (min-max)`.

### App launch time (ms), `none` arm = noise band

| Arm | 8 GB | 16 GB+ |
|---|---|---|
| none | TBD (owner PC) | TBD (owner PC) |
| trim | TBD (owner PC) | TBD (owner PC) |
| purge | TBD (owner PC) | TBD (owner PC) |

### Pages Input/sec after the action (hard fault proxy)

| Arm | 8 GB | 16 GB+ |
|---|---|---|
| none | TBD (owner PC) | TBD (owner PC) |
| trim | TBD (owner PC) | TBD (owner PC) |
| purge | TBD (owner PC) | TBD (owner PC) |

### Available MB and standby MB, change after the action

| Arm | 8 GB available | 8 GB standby | 16 GB+ available | 16 GB+ standby |
|---|---|---|---|---|
| none | TBD (owner PC) | TBD (owner PC) | TBD (owner PC) | TBD (owner PC) |
| trim | TBD (owner PC) | TBD (owner PC) | TBD (owner PC) | TBD (owner PC) |
| purge | TBD (owner PC) | TBD (owner PC) | TBD (owner PC) | TBD (owner PC) |

### Game 1% low frame time (ms) and average FPS around the action

| Arm | 8 GB | 16 GB+ |
|---|---|---|
| none | TBD (owner PC) | TBD (owner PC) |
| trim | TBD (owner PC) | TBD (owner PC) |
| purge | TBD (owner PC) | TBD (owner PC) |

### Machines and workload

| Class | RAM | Windows build | CPU / disk | Workload |
|---|---|---|---|---|
| 8 GB | TBD (owner PC) | TBD (owner PC) | TBD (owner PC) | TBD (owner PC) |
| 16 GB+ | TBD (owner PC) | TBD (owner PC) | TBD (owner PC) | TBD (owner PC) |

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

## Provisional decision (until the tables are filled)

Current behaviour, unchanged by this issue:

- None of the four is on by default. Automatic cleaning is off by default
  (`scheduler.rs`, `home.rs` text "Off by default").
- Memory priority and targeted trim run together, only when the user presses "Trim idle apps"
  (or uses Trim selected on the Advanced page), or, if the user turns it on, in automatic mode after sustained pressure
  (60 s hold, at most once an hour, never the foreground app, never near the commit limit).
- The low-priority cache purge is manual and needs administrator rights through the host helper.
- The report wording after a run never says memory was freed; it shows what moved to cache and
  what came back (`report.rs`).

| Action | Provisional label | Provisional decision |
|---|---|---|
| Nothing | n/a (control) | n/a |
| Memory priority only | Unproven | Not separately measurable until a build switch exists. Stays inside Trim idle apps. No benefit claimed. |
| Targeted trim (with priority) | Unproven | Keep opt-in or manual. No benefit claimed. |
| Low-priority cache purge | Unproven | Keep manual, administrator only. No benefit claimed. |

Nothing in this document or in the add-on may claim a benefit until the result tables are
filled and the decision rule above has been applied. Update the labels, this section and the
UI copy together, then remove the "PROTOCOL READY" status line at the top.

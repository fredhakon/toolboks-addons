# Toolboks add-ons

First-party native add-ons live here and release independently. The host keeps the SDK,
wire protocol, shared UI and catalog entry/download checks. Disk Manager is the first migrated tool (host #88); Hello
is only a small pipeline fixture.

## Layout

- `addons/<name>/`: one crate, one `<name>.tbaddon` SDK manifest, and `tests/protocol.rs`.
- `catalog/index.json`: the host's format 1, initially empty and unsigned.
- `tools/repo/`: manifest/index validation, release-to-catalog automation using host types,
  and the `measure` command.
- `budgets.json`: the size, start-up and memory limits `measure` checks.
- `.github/workflows/`: the isolated per-add-on release workflow. There is no CI: checks run locally before merging.

Folder, crate and executable names must agree. The manifest version matches Cargo's
version; its ID must match `run_addon`. Every add-on supplies a `protocol` integration
test accepting `ADDON_TEST_EXE` to test a release executable. Hello adapts the host's
`sdk/examples/hello-addon/tests/stdio.rs` test host: handshake, headless drawing,
heartbeat and quit, with a timeout and child cleanup. Named-pipe authentication and the
graphical test-host window are not exercised by this headless test.

Host dependencies pin commit `a96b0ab766648033af8937ac12ed269b58d4bbd7` (host staging,
protocol 3, minimum host 0.12.5). They are not on host main yet. Upgrade all four git
dependencies together, regenerate `Cargo.lock`, review compatibility and rerun checks.

## Build and test

Install stable Rust and Visual C++ build tools on Windows:

```powershell
cargo build --workspace --locked
cargo test --workspace --locked
cargo run --locked -p addon-repo -- validate
cargo fmt --all --check
./scripts/test-release.ps1
cargo build --locked --release -p hello
$env:ADDON_TEST_EXE = (Resolve-Path target/release/hello.exe).Path
cargo test --locked -p hello --test protocol
```

Run these checks locally for all add-ons before merging; nothing runs them for you. For interactive inspection, use the pinned
host's `cargo run --example addon_host -- <absolute-hello.exe-path> toolboks.hello`.

## Budgets

Every add-on is a separate process, so each one has to stay small (host issue #97).
`budgets.json` holds the limits: the first frame within 1000 ms of starting the exe, and
under 25 MB of memory when idle.

```powershell
cargo build --locked --release -p hello
cargo run --locked -p addon-repo -- measure            # every add-on; fails when one is over
cargo run --locked -p addon-repo -- measure hello --report-only --json target/measure.json
```

It starts each release exe 5 times (`--runs <n>` changes that) the way the protocol test
does, and prints the median: exe size, time to the handshake, time to the first drawn
frame, and memory after 3 idle seconds. "Idle memory" is the working set, which is at least
what Task Manager shows; "Private bytes" is the memory only that process holds.
Nothing runs it for you: run it locally before merging.

It measures the add-on alone. The named pipes, the launch token and the host's own drawing
are not part of it, and neither are the host budgets (host exe size, no process for a hidden
add-on); those are checked in the host repository.

## Release flow

1. Update the intended add-on's Cargo and SDK manifest versions and merge to main.
   Use plain `major.minor.patch`; host catalog format 1 does not support SemVer
   prerelease/build suffixes. Preview channel support awaits a preview add-on.
2. Tag the main commit, for example `hello-v0.1.0`, and push that tag. **Tag pushes
   publish real public releases.** No workflow creates tags automatically.
3. The Windows build job validates tag syntax, main ancestry and matching versions,
   then builds/tests only that crate, including the exact release executable. It
   reports executable bytes and uploads the exe and `.exe.sha256`.
4. A separate publish job creates the release. It alone has a write-capable
   `GITHUB_TOKEN` and executes no repository code. All actions use full SHAs.
5. The catalog job uses the released manifest/protocol pin and executable to update
   main's catalog, then opens a PR. URLs derive from `github.repository`, allowing
   a future owner move without workflow changes.
6. Review the plain unsigned catalog before merging; see [catalog/README.md](catalog/README.md).

Configure **`CATALOG_PR_TOKEN`** before releasing: a fine-grained bot token restricted
to this repository with Contents and Pull requests write access. Only the final PR
step receives it. Build/catalog jobs' `GITHUB_TOKEN` stays read-only. No signing keys are used. Protect main and
configure a tag ruleset for `*-v*` to restrict who can release.

If catalog PR creation fails, executable publication is already complete. Fix configuration
and re-run only the failed catalog job. Existing releases and branches are never overwritten.
If a branch was pushed before a PR API failure, open its PR manually. Concurrent catalog
PRs must be rebased/reconciled before owner review; preserve every other released entry.

## Test a release without publishing

Run the commands above. Validate a tag locally from a commit reachable from `origin/main`:
`./scripts/check-release.ps1 -Tag hello-v0.1.0`. In a temporary checkout, run
`cargo run --locked -p addon-repo -- catalog-update hello 0.1.0 OWNER/REPO <absolute-hello.exe-path>`
to update an unsigned catalog without calling GitHub. Run `actionlint` for workflow syntax.

For actual publication end to end, use a disposable repository with Actions and the bot
token configured, merge this code to its main, then push its Hello tag. Check release
assets, checksum, size summary and catalog PR. Do not push a production tag just to test CI.
Disk Manager is included; no private keys are included here.

## Disk Manager pilot (#88)

`addons/disk-manager` builds `disk-manager.exe`, preserving ID `toolboks.disk-manager`,
the five pages (Overview, Largest files, File types, Scan details, Optimization), and
the existing `settings`, `optimize_selected_drive` and `fragmentation_reports` preferences.
Use `--open "Disk Manager/Largest files"` after catalog installation as before. The host
keeps `addon-data/toolboks.disk-manager/` and namespaced preferences; the old local activation
package alone cannot install this executable. No data migration is required.

`host-services.json` is optional per-add-on metadata copied by `catalog-update` into the
catalog entry. Disk Manager declares its page names and `background: true`, so exports and
fragmentation analysis can finish while hidden. Suspending cancels scans and releases their
results. It keeps an idle background process under the existing host lifecycle; it does not
poll without work. Closing the host ends it. No protocol/supervisor changes are made here.

Drive optimization uses the host's administrator helper (protocol 3). The drawing process
never runs elevated. A click on Analyze, Defrag, TRIM or Optimize sends one request; a separate
elevated helper process prepares the exact change, shows its own native Confirm / Cancel window
and applies it only after Confirm. Input from the host window cannot confirm. See the host's
`docs/ADDON-AUTHOR-GUIDE.md`. The real-window dialog and a real UAC and drive test are still to
be run before the first release.

Where the add-on touches it:

- `src/main.rs`: `run_addon_with_admin`, so the same exe also starts as the helper.
- `src/optimization/mod.rs`: sends `HostRequest::AdminOperation`, receives `admin_result`.
- `src/optimization/backend.rs`: `prepare_admin` validates and previews; the helper applies after Confirm.

Run `cargo run --locked -p addon-repo -- measure disk-manager` for a release build, executable
bytes, three spawn-to-first-frame stdio timings, and suspended working set/private commit
after five seconds. The result is process-only and excludes the installer, named pipes and
host drawing. No catalog writes or signatures occur. Full pilot evidence and remaining
checks are in the paired host PR under `docs/reviews/88/`.

## Memory Cleaner (host #90)

`addons/memory-cleaner` builds `memory-cleaner.exe`, keeping ID `toolboks.memory-cleaner`, the
pages Home and Advanced and the built-in tool's behavior. The host's `src/tools/memory/` code
moved over with imports changed to the shared crates, and 160 of its own unit tests moved with
it. Nothing is stored between runs, as before.

- **Administrator action.** The one action that needs administrator rights, "Clear low-priority
  cache", no longer restarts the whole app as administrator. It uses the host's protocol 3
  helper: a separate elevated process shows its own Confirm / Cancel window and only then runs
  the purge (capability `system_tuning`). The drawing process is never elevated. Trimming idle
  apps, process trims and automatic cleaning need no administrator rights.
- **Background.** `host-services.json` sets `background: true` so a trim or optimize that is
  running finishes while the window is hidden. Sampling and automatic checks run while the
  window is shown, as in the built-in tool.
- **Status bar.** The `RAM nn%` item is sent through the host's status services.
- The guided tour has the same twelve steps.

The built-in copy stays in the host until the final removal and "Restore your tools" step (host
#94). The tray's memory percentage is measured by the host itself and does not depend on this
add-on. Measure with `cargo run --locked -p addon-repo -- measure memory-cleaner`.


## Event Watch (host #91)

`addons/event-watch` builds `event-watch.exe`, keeping ID `toolboks.event-watch`, the three
pages Live, Crashes and Summary, and the built-in tool's behavior. The code is the host's
`src/tools/events/` moved over; the only changes are the imports (`toolboks_shared::ui` instead
of the host's own module) and the add-on entry point in `src/lib.rs`. It only reads: Windows
event logs and crash dump headers, no administrator rights, nothing is cleared or changed. A
log the user cannot read is reported in the page header as before. Its guided tour has the same
six steps. Suspending releases the loaded events; they are read again on return. The tool has no
saved settings today, so nothing needs migrating.

The built-in copy stays in the host until the final removal and "Restore your tools" step (host
#94), so users never lose the tab or see it twice. Measure it with
`cargo run --locked -p addon-repo -- measure event-watch`.

## Resource Monitor (host #89)

`addons/resource-monitor` builds `resource-monitor.exe`, keeping ID `toolboks.resource-monitor`,
the pages Overview, Details, Processes and Connections and the built-in tool's behavior. The
host's `src/tools/resources/` code moved over with imports changed to the shared crates (and the
add-on entry point in `src/lib.rs`); 91 of its own unit tests moved with it. It needs no
administrator rights and stores nothing: the graphs live in memory while the add-on is shown.

- **Status bar.** The `CPU nn%` and `GPU nn%` items go through the host's status services.
- **Suspending** stops the sampler and drops the graphs, as the built-in tool does when it is
  not shown.
- **Not in the background yet.** `host-services.json` does not set `background`. Keeping the
  sampler running while hidden belongs with the history recorder (host #64), which does not
  exist yet. Today the built-in tool does not record while hidden either.
- The guided tour has the same 22 steps.

The built-in copy stays in the host until the final removal and "Restore your tools" step (host
#94). Measure with `cargo run --locked -p addon-repo -- measure resource-monitor`.

## Audio Gainer / Aural (host #93)

`addons/aural` builds `aural.exe`, keeping ID `toolboks.aural`, the title Audio Gainer, the
byline "by aural" and the built-in provider's behavior. The host's `src/native/aural.rs` and its
`routing` and `settings` modules moved over unchanged apart from imports (the `egui` crate
instead of `eframe::egui`) and making the type public; 27 of its own unit tests moved with it.
It needs no administrator rights.

- **Capabilities:** `live_audio` (reads the microphone and plays to chosen outputs) and
  `local_preferences` (its settings under `audio_settings_v1`, kept by the host).
- **Background.** `host-services.json` sets `background: true`: an input that is running keeps
  running, and device discovery keeps going, while the window is hidden. Aural reports that in
  `has_background_work`, as it did in the host.
- Sending audio to another app still needs a separately installed virtual cable driver.
- The repo tool now treats both `host-services.json` keys (`pages`, `background`) as optional,
  so `catalog-update` no longer panics for add-ons that set only one of them.

The built-in provider and its legacy 0.1 package stay in the host until the final removal and
"Restore your tools" step (host #94). Measure with
`cargo run --locked -p addon-repo -- measure aural`.

## Macro Manager (host #93)

`addons/macro-manager` builds `macro-manager.exe`, keeping ID `toolboks.macro-manager`, the
pages Editor and Mouse editor, the settings key `macro_manager_v1` and the built-in provider's
behavior. The host's `src/native/macros.rs` and its engine moved over; 14 of its own unit tests
moved with it (3 more need an interactive desktop and stay ignored). It needs no administrator
rights.

- **Capabilities:** `input_automation` (global key and mouse recording, hotkeys and playback with
  real input) and `local_preferences`.
- **Background.** `host-services.json` sets `background: true`: shortcuts, recording and playback
  keep working while the window is hidden.
- **What had to change because it is now a separate process.** The built-in provider knew where
  its own Record button was on screen and which window was Toolboks. A separate process does
  not. Two things changed, both small:
  - Keys typed while a Toolboks window is in front are not recorded. The add-on now treats its
    own process and the process that started it (the Toolboks window) as "the app".
  - The click on "Stop recording" is no longer excluded by a screen rectangle. After recording
    stops, trailing steps that sit over a Toolboks window (found with `WindowFromPoint`, outside
    the hook) are dropped, so that click and the cursor travel to it are not in the macro.
- Not yet checked in a real window: record and stop with a mouse and a keyboard, hotkeys while the
  window is hidden, and that the stop click is really left out.

The built-in provider stays in the host until the final removal and "Restore your tools" step
(host #94). Measure with `cargo run --locked -p addon-repo -- measure macro-manager`.

## Debloater (host #92)

`addons/debloater` builds `debloater.exe`, keeping ID `toolboks.debloater` and the five pages
Privacy and ads, Apps, Startup, Tasks and Services. The host's `src/tools/debloat/` pages moved
over with imports changed; the engine stays in the host's `toolboks-debloat-engine` crate, which
this repo uses as a git dependency pinned with the others (System Profiles will use it too).

**What works.** Everything that does not need administrator rights, exactly as in the built-in
tool: reading the real state of every item, the plain-words review, applying and undoing
current-user changes (privacy and advertising switches, startup entries, installed apps for your
own account), and the guided tour.

**Administrator changes (host issue #145), family by family.** Protocol 3 add-ons never run
elevated, so a change that needs administrator rights goes to the separate helper, which shows its
own Confirm / Cancel window and only then runs the engine. The request names catalog items by id;
the preview text is built from the catalog, never from the request; undo steps are accepted only
for registry locations the catalog itself lists.

- **Done: Privacy and ads.** Applying items that write the machine-wide registry, the "Create a
  restore point first" switch, and undoing a run that touched a machine-wide value all go through
  the helper (`debloat` operation, kinds `registry_apply` and `registry_undo`). Items that only
  change your own account still run in the add-on without any prompt.
- **Done: Services and Tasks.** Switching one service off or one task on or off, the restore point
  switch on those pages, and undo (one step, the newest step of one service, or all) go through the
  helper (kinds `service_set`, `service_undo`, `task_set`, `task_undo`). The helper looks the
  service or task up in the catalog by id (the service name and task path never come from the
  request), refuses names the engine never offers, and runs the unchanged engine. Task Scheduler
  work runs on its own thread inside the helper because it uses COM. An undo batch is undone newest
  first and never puts an older value over a newer one that failed.
- **Done: Startup entries that need administrator rights** (all-users Run keys and the all-users
  Startup folder). Switching one on or off, and undoing a batch that includes one, go through the
  helper (kinds `startup_set` and `startup_undo`). The helper re-reads the real startup list and
  only accepts an entry that is in it (source and name), refuses the protected ones, and builds the
  preview from the real entry (name, publisher, command, scope). Undo values are accepted only as
  absent or exactly 12 bytes, for entries that still exist. Entries for your own account still
  change in the add-on without a prompt.

All families are now done, so nothing in the Debloater is listed as unavailable any more.

Real UAC validation of the new operations has not been done; the owner chose to test on use and
report anything found.

The built-in tool stays in the host until the final removal and "Restore your tools" step (host
#94). Measure with `cargo run --locked -p addon-repo -- measure debloater`.

## System Profiles (host #92)

`addons/system-profiles` builds `system-profiles.exe`, keeping ID `toolboks.system-profiles`, its
single Gaming page and the built-in provider's behavior. The host's `src/native/profiles.rs` and
its engine moved over with imports changed; the shared debloat engine is a pinned git dependency.

**What works.** Reading the current value of every choice, the "Review current values" and
"Review registry edits" previews, the guided Windows and NVIDIA settings list, opening the
backup folder and the guided tour.

**Administrator changes.** Applying a profile, turning on System Protection and restoring need
administrator rights. They go through the protocol 3 helper: the Apply and Restore buttons send
one request (`gaming_profile`, kind `apply` or `restore`, choices by id) and the drawing process
never runs elevated. The helper checks the ids against the add-on's own list, builds the preview
from that list and the current registry values (never from request text), shows its own Confirm /
Cancel window, and only then runs the same engine as before (restore point, registry backup,
changes, verification, activity log). Cancelling at the UAC prompt or in the window changes
nothing. Real UAC validation is still required (host issue #142).

The built-in provider stays in the host until the final removal and "Restore your tools" step
(host #94). Measure with `cargo run --locked -p addon-repo -- measure system-profiles`.

## Memory Cleaner benchmark

Issue #32 asks whether the memory priority, targeted trim and low-priority cache purge actions
help at all. `docs/RESEARCH-memory-cleaning.md` holds the protocol, the result tables (empty until
measured on 8 GB and 16 GB+ PCs) and the decision rules; no benefit is claimed yet. Priority-only
has no UI action, so only trim and purge are scripted.
`scripts/bench-memory.ps1` collects the numbers (it does not run the action, needs no
administrator rights and writes only under `-OutDir`); try it with `-Arm none -DryRun`.


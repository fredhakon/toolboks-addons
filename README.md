# Toolboks add-ons

First-party native add-ons live here and release independently. The host keeps the SDK,
wire protocol, shared UI and catalog entry/download checks. Disk Manager is the first migrated tool (host #88); Hello
is only a small pipeline fixture.

## Layout

- `addons/<name>/`: one crate, one `<name>.tbaddon` SDK manifest, and `tests/protocol.rs`.
- `catalog/index.json`: the host's format 1, initially empty and unsigned.
- `tools/repo/`: manifest/index validation and release-to-catalog automation using host types.
- `.github/workflows/`: Windows PR CI and isolated per-add-on releases.

Folder, crate and executable names must agree. The manifest version matches Cargo's
version; its ID must match `run_addon`. Every add-on supplies a `protocol` integration
test accepting `ADDON_TEST_EXE` to test a release executable. Hello adapts the host's
`sdk/examples/hello-addon/tests/stdio.rs` test host: handshake, headless drawing,
heartbeat and quit, with a timeout and child cleanup. Named-pipe authentication and the
graphical test-host window are not exercised by this headless test.

Host dependencies pin commit `5a1fee118f98b7e6e677836ec64003e5c3589497` (host staging,
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

PR CI runs these checks for all add-ons. For interactive inspection, use the pinned
host's `cargo run --example addon_host -- <absolute-hello.exe-path> toolboks.hello`.

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
step receives it. Build/catalog jobs' `GITHUB_TOKEN` stays read-only. The separate token
allows bot PRs to trigger CI. No signing keys are used. Protect main and
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

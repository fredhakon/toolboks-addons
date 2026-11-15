# Disk Manager #24 verification

Required local checks passed: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo run --locked -p addon-repo -- validate`. Release build: `cargo build --release -p disk-manager`. Output and large fixtures are on D:.

## Measurements

Read-only scan of D:, with deliberate changes limited to `D:/CodexBuilds/disk-manager-issues-22-24/bench-mutations`. Debug profile, Windows folder-time method, same machine and drive:

| Run | Full | Quick | Verification full |
|---|---:|---:|---:|
| Initial serial-probe build, cold (other builds active) | 129.568 s | 2.801 s | 3.960 s |
| Warm final implementation | 3.976 s | 0.859 s | 4.947 s |

Final run: 406,162 files, 33,802 folders, 755,771,770,384 logical bytes. Quick reported **2 new, 2 removed, 1 changed** after an addition, deletion, rename and size edit in a changed folder. It read 4 folders and reused 33,798 directory lists. The verification full scan matched file count and bytes exactly. Warm speedup is about 4.6x; the cold result alone overstates the gain. The initial run's extra changes included build output and are not used as the controlled count check. Benchmark log: `D:/CodexBuilds/disk-manager-issues-22-24/issue-24-drive-benchmark-final.log`.

Synthetic large-tree test (700,000 files / 700,101 nodes; no 700,000-file fixture is created on disk): 41,894,774-byte snapshot, 3.076 s load, 806 headless frames while loading, longest frame 0.548 s in debug. The final frame includes building the map/list. The original tree is dropped before loading, and decoding streams directly into one node vector; no whole-file buffer or second loaded tree is retained. Log: `issue-24-large-load.log` in the same D: folder. This is a headless debug measurement, not a claim of native 700k-file window testing.

## Coverage

Tests cover binary round trip/path reconstruction, version rejection, corruption, truncation, impossible counts, an interrupted pending write preserving the prior snapshot, volume/root/exclusion matching, atomic replacement, per-drive/all deletion, turning remembering off deleting files on disk, save/delete queue ordering, unchanged nested folders, added/removed/renamed entries, refreshed size inside changed folders, the in-place-edit limit, cancellation, navigation/selection/pagination restoration, pointer activation of Scan, and repaint scheduling for background cache work. Protocol tests use isolated data and verify migration of old preferences to remembering scans by default.

## Storage and host boundary

Scans live in the actual installed host root's `addon-data/toolboks.disk-manager/scans`; standalone builds use `toolboks_shared::storage::data_root`. This handles the Preview host root without changing the host or adding a new capability. The host's `installer::data_dir` and `remove_data` delete that exact subtree on remove-with-data. Native uninstall click-through has not been exercised; source/path coverage is verified.

The pinned SDK/host has no capability or custom install-review line for large local scan storage. Existing `local_preferences`/`filesystem_read` access is retained. Manifest short/long descriptions and the add-on page explicitly disclose local file-name storage, but the grouped native install review only renders generic capability labels. A host review-copy change remains a follow-up; no host files were edited.

One snapshot per volume/root replaces the previous file. The cap is 384 MiB per root and the page warns above 256 MiB. Older/damaged formats are ignored and require a fresh scan. A drive-letter reassignment cannot load data for another volume; old roots that no longer map to the recorded volume are skipped rather than migrated automatically.

## Native window

The isolated copied preview host installed the local catalog/release executable. A seven-file mixed folder was scanned and saved, the host exited, and a fresh process reopened the scan without clicking Scan. The map, lists, scan age, quick/full actions and privacy controls were visible.

After creating two files, removing one, renaming one and changing a size in a changed folder, the native quick action displayed **3 new, 2 removed, 1 changed**. Native Full rescan then saved exactly **32,300,038 bytes / 8 files**, matching quick totals, with the quick summary cleared and full-scan time refreshed. Captures:

- `D:/CodexBuilds/disk-manager-issues-22-24/shots24-final-scan/click-01.png`
- `D:/CodexBuilds/disk-manager-issues-22-24/shots24-reopen/04-open-page-1.png`
- `D:/CodexBuilds/disk-manager-issues-22-24/shots24-quick/click-01.png`
- `D:/CodexBuilds/disk-manager-issues-22-24/shots24-full/click-01.png`

The native check revealed a missing repaint after starting cache work during UI rendering (the SDK ticks before rendering). UI rendering now schedules another frame for pending work; a regression test covers this. File deletion and remembering-off effects are verified on disk in headless tests, not by native confirmation clicks. Host remove-with-data, native privacy confirmation click-through, C: specifically and 125-200% scaling remain owner checks. No optimization, administrator action, signing, tag or release was performed.

The final release also loaded the actual D: snapshot (406,162 files / 703.87 GiB) without a scan and rendered all five pages in the copied preview host. The first drawn frame was 140.389 ms after opening; the complete map was visible at the five-second capture. Key capture: `D:/CodexBuilds/disk-manager-issues-22-24/shots24-final-drive/04-open-page-1.png`; the same folder holds all page captures, timing.json and result.txt. All captures were inspected.

The pre-existing Resource Monitor process-identity test was intermittent during workspace verification (also seen in #22). Final `cargo test --workspace` passed with `RUST_TEST_THREADS=1`; no Resource Monitor source was changed. Full log: `D:/CodexBuilds/disk-manager-issues-22-24/issue-24-workspace-tests-serial.log`. Strict clippy, formatting and catalog validation passed normally.

# Disk Manager #22 verification

Local checks: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, and `cargo run --locked -p addon-repo -- validate` passed. Release executable built with `cargo build --release -p disk-manager`. All build output was on D:.

Tests cover every category shade and selection text at WCAG AA, dominant nested folder categories, distinct shades within one category, layout geometry, menu positioning, Escape/outside dismissal, restoring focus, Shift+F10/menu key, missing-item fallback, and Windows Shell parsing of a >260-character path containing spaces and Unicode. No Explorer window is opened by tests.

The isolated preview host was copied to `D:/CodexBuilds/disk-manager-issues-22-24/Toolboks-Preview.exe`. A local catalog used the release executable with recomputed size/SHA-256. Initial installation/review and all five pages rendered. The mixed fixture and real Rust target folder were scanned read-only in the host. Final captures live under `D:/CodexBuilds/disk-manager-issues-22-24/shots22-confirm-mixed/` and `shots22-confirm-target/` (`click-01.png`).

The automated host check cannot right-click or send keyboard shortcuts. Menu behavior is covered by headless egui tests; actual right-click/keyboard through the host, Explorer selection, and 125-200% native scaling remain owner click-through checks. No normal host, administrator action, optimization, signing, tag or release was started.

The first workspace run found a transient pre-existing Resource Monitor process-identity test failure. The full rerun and final full run passed without changes to Resource Monitor. Logs: `D:/CodexBuilds/disk-manager-issues-22-24/issue-22-tests-final.log`.

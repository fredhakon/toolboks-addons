# Verification

Local Windows checks, 2026-11-14, stable Rust 1.100.0:

- `cargo build --workspace --locked`: passed.
- `cargo test --workspace --locked`: passed (manifest agreement, protocol host, catalog update).
- `cargo run --locked -p addon-repo -- validate`: passed using host validators.
- `cargo fmt --all --check`: passed.
- `cargo build --locked --release -p hello`: passed; executable 2,862,080 bytes.
- `ADDON_TEST_EXE=target/release/hello.exe cargo test --locked -p hello --test protocol`:
  passed against the release executable (set the environment variable using PowerShell syntax).
- `scripts/test-release.ps1`: valid tag accepted; injection text, malformed version,
  unknown add-on, version mismatch and off-main commit rejected in a disposable local repository.
- `actionlint`: passed for both workflows. Action commit pins resolve on GitHub.

CI follow-up: the tag-validation test passed its assertions but left the expected
off-main `git merge-base` exit code at 1. Reproduced using GitHub's PowerShell wrapper
pattern (`if (Test-Path variable:\LASTEXITCODE) { exit $LASTEXITCODE }`). After clearing
the native exit code only on successful completion, the same wrapper exits 0 and all
rejection assertions still pass. `actionlint` also passes after the correction.

No production tags or GitHub releases were created. Actual publication/catalog-PR flow
requires merging to main, configuring the documented bot token and pushing a release tag.
Catalog signing remains the owner's offline step. The headless stdio protocol test is
adapted from the pinned host sample; named-pipe authentication and graphical integration
remain host-side checks.

## Budgets (host #97), 2026-11-15

`cargo run --locked -p addon-repo -- measure` on a Windows 11 PC, release build, median of
5 starts: `hello` is 2.7 MB on disk, completes the handshake in 5 ms, draws its first frame
in 21 ms and holds 7.0 MB of working set (2.1 MB private) after 3 idle seconds. Both budgets
are met. With the budgets set below those numbers the command exits 1; with `--report-only`
it exits 0.

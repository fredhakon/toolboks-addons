# Toolboks add-ons

First-party native add-ons live here and release independently. The host keeps the SDK,
wire protocol, shared UI and catalog verifier. Real tool migration is host #88; Hello
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

Host dependencies pin commit `5f7c39b04dc367379be0eb9396cfb7362af2ffa3` (host staging,
protocol 2, minimum host 0.12.5). They are not on host main yet. Upgrade all four git
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
6. Review and sign the catalog offline before merging; see [catalog/README.md](catalog/README.md).

Configure **`CATALOG_PR_TOKEN`** before releasing: a fine-grained bot token restricted
to this repository with Contents and Pull requests write access. Only the final PR
step receives it. Build/catalog jobs' `GITHUB_TOKEN` stays read-only. The separate token
allows bot PRs to trigger CI. No signing key belongs in this secret. Protect main and
configure a tag ruleset for `*-v*` to restrict who can release.

If catalog PR creation fails, executable publication is already complete. Fix configuration
and re-run only the failed catalog job. Existing releases and branches are never overwritten.
If a branch was pushed before a PR API failure, open its PR manually. Concurrent catalog
PRs must be rebased/reconciled before signing; sequence/expiry become final at owner review.

## Test a release without publishing

Run the commands above. Validate a tag locally from a commit reachable from `origin/main`:
`./scripts/check-release.ps1 -Tag hello-v0.1.0`. In a temporary checkout, run
`cargo run --locked -p addon-repo -- catalog-update hello 0.1.0 OWNER/REPO <absolute-hello.exe-path>`
to update an unsigned catalog without calling GitHub. Run `actionlint` for workflow syntax.

For actual publication end to end, use a disposable repository with Actions and the bot
token configured, merge this code to its main, then push its Hello tag. Check release
assets, checksum, size summary and catalog PR. Do not push a production tag just to test CI.
No real tools or private keys are included here.

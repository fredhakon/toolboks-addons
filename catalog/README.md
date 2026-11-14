# Catalog

`index.json` is the host's format 1 (`toolboks-catalog::Index`). It starts empty:
there is no downloadable Hello entry until an executable has actually been released.
The initial unsigned index is a development source, not a trusted published catalog.

Release automation adds one entry per version, retaining older entries. It copies the
released manifest's host compatibility and the pinned protocol version, and hashes the
actual executable. New entries have `verified: false`; only owner review can change that.
The bot increments `sequence`, sets a 14-day validity window and removes stale signatures.

Before merging any catalog PR, rebase onto current main, preserve all other releases,
and make `sequence` higher than every published index. Refresh `generated_at` and
`expires_at` (Unix seconds, at most 90 days apart). Review compatibility, release URL,
size and SHA-256. The placeholder does not warrant a verified badge by default.

Signing follows the **offline-only owner decision in host #84**. No CI signing secret
is used. On the owner's machine, build `tools/catalog-sign` from the pinned host commit:

```powershell
cargo build --release --locked -p catalog-sign
# Use that binary on the add-ons checkout; the private-key path is outside this repo.
catalog-sign sign catalog/index.json E:\catalog-main\catalog-private.key
catalog-sign verify catalog/index.json <main-public-hex> <recovery-public-hex>
```

Commit `catalog/index.json` and `catalog/index.json.sig` together after signing. Keep
`rotations.json` beside the index when rotating keys. The recovery key signs rotation
statements only. Never generate keys in CI or put private keys in this repository or
Actions secrets. `.gitattributes` preserves signed bytes exactly across platforms.

The host still needs owner-configured public keys and a catalog delivery URL. Publishing
or hosting a signed index is an owner step; this workflow publishes executable releases
and opens catalog PRs, but does not deploy an unsigned index. Renew the catalog before
expiry even when there are no add-on releases. See the pinned host's
`docs/catalog-signing.md` for the full signing and recovery runbook.

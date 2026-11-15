# Catalog

`index.json` is a plain unsigned list in host format 1 (`toolboks_catalog::Index`):

```json
{"format_version":1,"entries":[]}
```

This follows the owner's 2026-11-15 decision, implemented in host PR #134. There are
no signatures, keys, rotation statements, sequence numbers, or validity dates.
Executable downloads are checked against the size and SHA-256 declared by each entry.

The list stays empty until an executable has actually been released. Release automation
copies the released manifest's compatibility, byline and capabilities plus optional
`host-services.json` pages/background metadata, records the pinned protocol version, and
hashes the actual executable. New entries have `verified: false`; owner review controls it.

Before merging a catalog PR, rebase onto current main, preserve other add-ons and versions,
and review the executable URL, version, compatibility, capabilities, page names, background
behavior, size and SHA-256. The workflow opens the PR; owner review completes publication.
No signing tool or signing secret is used. Tags/releases remain separate owner/orchestrator
steps; do not publish a production add-on merely to test the pipeline.

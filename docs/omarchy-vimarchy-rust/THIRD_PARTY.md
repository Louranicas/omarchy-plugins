# Third-party provenance and notices

This development snapshot is an independent adaptation. It is not an upstream release and does not imply endorsement by the upstream authors. Attribution below concerns the pinned source used for implementation or source-derived behavior/tests; it is not a claim that every adapted Rust line was copied.

## Original plugin

- Upstream: [clickety-clacks/vimarchy](https://github.com/clickety-clacks/vimarchy).
- Pinned commit: [`30601976903821a82097ab568f10aae56564c32f`](https://github.com/clickety-clacks/vimarchy/tree/30601976903821a82097ab568f10aae56564c32f).
- Original notice: Copyright (c) 2026 Mike Manzano. The complete original [MIT license notice](LICENSES/vimarchy-MIT.txt) is preserved byte-for-byte. Existing crate-level copies are retained.

## Hyprland source fixtures

`modal-runtime/fixtures/hyprland-0.56.2/` contains upstream source fixtures used by tests. Source provenance records commit `efb50993780079460b0cbed1363e2166a2de1d9f` and per-file hashes in [provenance.json](../../modal-runtime/fixtures/hyprland-0.56.2/provenance.json). Preserve the fixture [original LICENSE](../../modal-runtime/fixtures/hyprland-0.56.2/LICENSE), also copied as [BSD-3-Clause notice](LICENSES/Hyprland-BSD-3-Clause.txt), Copyright (c)2022–2026 vaxerski. These files are not relicensed by the project’s MIT metadata. Several Rust tests use their contents directly.

## Adaptation and dependency scope

Cargo manifests record the licensing declarations present in this snapshot. Shared Rust modules were developed for this adaptation; the workspace declares MIT, but a workspace default does not itself add a missing per-package manifest field. These notices preserve upstream terms and do not invent an owner for newly authored contributions or override third-party licenses.

Registry dependencies are identified by the Cargo lockfiles and are fetched by Cargo; this snapshot does not vendor their source or include built binaries. Before distributing a binary or vendored dependency bundle, collect the actual dependency/system-library license notices and satisfy their applicable distribution requirements. This document is not a completed dependency-license audit or blanket reuse clearance.

No credentials, machine-specific validation receipts, private review transcripts or desktop session screenshots are intentionally included. Upstream public test fixtures remain attributed to the pinned source; fixture data is not evidence collected from the publishing user's machine.

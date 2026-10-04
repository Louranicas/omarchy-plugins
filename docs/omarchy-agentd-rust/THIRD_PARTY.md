# Third-party provenance and notices

This development snapshot is an independent adaptation. It is not an upstream release and does not imply endorsement by the upstream authors. Attribution below concerns the pinned source used for implementation or source-derived behavior/tests; it is not a claim that every adapted Rust line was copied.

## Original plugin

- Upstream: [clickety-clacks/agentd](https://github.com/clickety-clacks/agentd).
- Pinned commit: [`a539cf2f327e304a71fd333fd53d4296b9e339c1`](https://github.com/clickety-clacks/agentd/tree/a539cf2f327e304a71fd333fd53d4296b9e339c1).
- The pinned Cargo.toml declares `license = "MIT"`, but the inspected upstream commit contains no root LICENSE or NOTICE file. [Exact declaration evidence](LICENSES/agentd-upstream-license-declaration.txt) is preserved. This note does not invent an upstream copyright notice or claim one was present.

## Adaptation and dependency scope

Cargo manifests record the licensing declarations present in this snapshot. Shared Rust modules were developed for this adaptation; the workspace declares MIT, but a workspace default does not itself add a missing per-package manifest field. These notices preserve upstream terms and do not invent an owner for newly authored contributions or override third-party licenses.

Registry dependencies are identified by the Cargo lockfiles and are fetched by Cargo; this snapshot does not vendor their source or include built binaries. Before distributing a binary or vendored dependency bundle, collect the actual dependency/system-library license notices and satisfy their applicable distribution requirements. This document is not a completed dependency-license audit or blanket reuse clearance.

No credentials, machine-specific validation receipts, private review transcripts or desktop session screenshots are intentionally included. Upstream public test fixtures remain attributed to the pinned source; fixture data is not evidence collected from the publishing user's machine.

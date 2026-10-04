# Snapshot validation

Validated locally on Linux on 2026-10-04 after relocating the source into this self-contained checkout.

102 test executions passed; 0 failed; 1 ignored. Shared-crate tests can execute in both the root and UI workspace; this is not a distinct-test count.

All commands below exited successfully:

```sh
cargo generate-lockfile --offline
cargo fmt --all --check
cargo test --workspace --locked --offline
cargo clippy --workspace --all-targets --locked --offline -- -D warnings
```

These are library, integration-fixture and compiled UI tests. They do not establish live provider compatibility, full native compositor behavior, hardware performance, installer safety or deployment readiness. Native desktop harnesses tied to the private development environment and historical receipts are excluded. No installation or live service activation was performed.

The ignored Agentd procfs replay requires a separately authorized real-harness capture; no result is claimed for it.

# Snapshot validation

Validated locally on Linux on 2026-10-04 after relocating the source into this self-contained checkout.

244 test executions passed; 0 failed; 0 ignored. Shared-crate tests can execute in both the root and UI workspace; this is not a distinct-test count.

All commands below exited successfully:

```sh
cargo generate-lockfile --offline
cargo fmt --all --check
cargo test --workspace --locked --offline
cargo clippy --workspace --all-targets --locked --offline -- -D warnings
cargo fmt --manifest-path yoohoo-ui/Cargo.toml --check
cargo test --manifest-path yoohoo-ui/Cargo.toml --locked --offline
cargo clippy --manifest-path yoohoo-ui/Cargo.toml --all-targets --locked --offline -- -D warnings
```

These are library, integration-fixture and compiled UI tests. They do not establish live provider compatibility, full native compositor behavior, hardware performance, installer safety or deployment readiness. Native desktop harnesses tied to the private development environment and historical receipts are excluded. No installation or live service activation was performed.

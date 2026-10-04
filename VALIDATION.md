# Combined repository validation

Validated locally on Linux on 2026-10-04 after consolidation.

491 passing test executions, 0 failures, 1 ignored. Shared crates run in more than one workspace, so this is not a unique-test count. The ignored Agentd capture test requires separately authorized real-harness input.

All 13 commands succeeded:

```sh
cargo generate-lockfile --offline
cargo fmt --all --check
cargo test --workspace --locked --offline
cargo clippy --workspace --all-targets --locked --offline -- -D warnings
cargo fmt --manifest-path ask-native/Cargo.toml --check
cargo test --manifest-path ask-native/Cargo.toml --locked --offline
cargo clippy --manifest-path ask-native/Cargo.toml --all-targets --locked --offline -- -D warnings
cargo fmt --manifest-path vimarchy-ui/Cargo.toml --check
cargo test --manifest-path vimarchy-ui/Cargo.toml --locked --offline
cargo clippy --manifest-path vimarchy-ui/Cargo.toml --all-targets --locked --offline -- -D warnings
cargo fmt --manifest-path yoohoo-ui/Cargo.toml --check
cargo test --manifest-path yoohoo-ui/Cargo.toml --locked --offline
cargo clippy --manifest-path yoohoo-ui/Cargo.toml --all-targets --locked --offline -- -D warnings
```

These checks cover the relocated source, fixture integrations and compiled UI tests. They do not qualify production installation, live providers, complete compositor workflows or power-loss recovery. No live service or desktop configuration was changed.

## Additional native workflow investigation

A later, broader candidate verification found an unexpected maximize in the interactive Vimarchy changed-target second-tap scenario. The cause is under investigation; input delivery versus implementation has not been established. The commands above remain passing evidence for their stated scopes, but do not resolve this native workflow finding. Passive-renderer evidence does not qualify interactive input behavior.

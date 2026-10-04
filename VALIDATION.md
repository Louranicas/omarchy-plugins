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

A later, broader candidate verification found an unexpected maximize in the interactive Vimarchy changed-target second-tap scenario. Diagnosis confirmed that the passive-renderer update disabled descendant focus on the interactive controls container, so the entry retained its original target despite edit-key delivery. This snapshot corrects that property only in interactive mode; passive mode remains unfocusable. Independent private-Wayland checks observed a/s changed-target inputs within 152 ms with zero maximize, and a/a ordinary double-tap within 145 ms with one maximize. Compositor observations/effects were simulated. The original aggregate failure remains historical; it is not reclassified as a full passing run.

After copying the independently reviewed correction into this combined checkout, UI formatting, all 7 Vimarchy UI tests and all-target strict Clippy passed with the locked dependencies. Tests and Clippy used `CARGO_BUILD_JOBS=2 CARGO_PROFILE_DEV_DEBUG=0`; the commands were the Vimarchy UI commands listed above. All 74 Vimarchy source-manifest entries matched, including the documented manifest whitespace adaptation. The whole workspace, other UIs and native/passive suites were not rerun for this one-line correction. No release qualification follows.

## Executable continuity and stale-action regressions

The shared desktop transport now retains the original executable inode and rejects a same-PID switch to a different executable. Independent socket/process tests confirmed rejection even when exec occurs after request delivery, while a still-running original executable remains usable after its pathname is unlinked. This is executable-inode continuity, not code attestation: same-inode exec, interpreter code changes and changes restored between observations remain undetected. Checks are not atomic with remote effects.

Eight additional Yoohoo regression cases cover stale, closed, focused, partial, flooded, replayed and incorrectly fenced activation inputs. The source candidate passed the full 541-test root workspace check, followed by the expanded 15-test service suite and strict Clippy for its test-only additions. The publication verification below separately covers this relocated snapshot.

The desktop peer integration test requires Python 3 at `/usr/bin/python`, Perl at `/usr/bin/perl` with its core `IO::Socket::UNIX`, Linux procfs/pidfd support and same-user access to executable links. These are test prerequisites, not runtime dependencies.

Publication verification on 2026-10-04: all nine commands passed—root workspace format, all-target tests and strict Clippy, plus tests and all-target strict Clippy for Ask native, Vimarchy UI and Yoohoo UI. **500 passing test executions, 0 failures, 1 ignored** across these scopes; counts may overlap. The single ignored Agentd captured-procfs check remains pending. Commands used offline locked dependencies, two build jobs and development debug information disabled. No deployment qualification is implied.

# Snapshot validation

Initial snapshot `27932dd` was validated locally on Linux on 2026-10-04 after relocating the source into this self-contained checkout.

224 test executions passed; 0 failed; 0 ignored. Shared-crate tests can execute in both the root and UI workspace; this is not a distinct-test count.

All commands below exited successfully:

```sh
cargo generate-lockfile --offline
cargo fmt --all --check
cargo test --workspace --locked --offline
cargo clippy --workspace --all-targets --locked --offline -- -D warnings
cargo fmt --manifest-path vimarchy-ui/Cargo.toml --check
cargo test --manifest-path vimarchy-ui/Cargo.toml --locked --offline
cargo clippy --manifest-path vimarchy-ui/Cargo.toml --all-targets --locked --offline -- -D warnings
```

These are library, integration-fixture and compiled UI tests. They do not establish live provider compatibility, full native compositor behavior, hardware performance, installer safety or deployment readiness. Native desktop harnesses tied to the private development environment and historical receipts are excluded. No installation or live service activation was performed.

## Passive-renderer follow-up

On 2026-10-04, the reviewed passive-renderer source was copied into this checkout and the relocated UI was revalidated. All 7 UI tests passed, with 0 failures and 0 ignored tests. Both formatting checks and strict Clippy passed:

```sh
cargo fmt --all --check
cargo fmt --manifest-path vimarchy-ui/Cargo.toml --check
CARGO_BUILD_JOBS=2 CARGO_PROFILE_DEV_DEBUG=0 cargo test --manifest-path vimarchy-ui/Cargo.toml --locked --offline
CARGO_BUILD_JOBS=2 CARGO_PROFILE_DEV_DEBUG=0 cargo clippy --manifest-path vimarchy-ui/Cargo.toml --all-targets --locked --offline -- -D warnings
```

All 74 source-manifest entries matched their recorded hashes, honoring the existing published Cargo manifest whitespace adaptation. The four-file diff passed whitespace and added-text private-path/token checks and manual review. No private receipts or native harness files were copied.

The earlier whole-workspace result above belongs to the initial snapshot; this follow-up reran the changed UI package and formatting, not the entire workspace suite. The README distinguishes separately reviewed development fixtures from checks reproducible in this checkout. This update adds no physical-input adapter and does not advance native or deployment release gates.

## Interactive entry-focus correction

A broader development fixture exposed that the passive-renderer change also disabled descendant focus in interactive mode. The hint entry therefore retained its original target even when edit keys arrived. The container now permits descendant focus only in interactive mode; passive behavior remains unchanged. Independent private-Wayland checks observed changed targets a/s within 152 ms with zero maximize, and ordinary a/a double-tap within 145 ms with one maximize. Compositor observations/effects in these cases were simulated; the original failed aggregate run remains historical and is not reclassified as passing.

After relocating this one-line fix, the same UI formatting, 7-test and strict-Clippy commands above passed again. The entire workspace and native/passive suites were not rerun for this correction. No private evidence or fixture paths were copied, and no release gate is advanced.

# omarchy-vimarchy-rust

Development snapshot of the Omarchy Rust plugin. This is an independent adaptation, not an upstream release.

## Status

The controlled managed presenter supports explicit passive rendering: keyboard interactivity remains None, the pointer input region stays empty, and plugin GTK key/pointer handlers are not attached. Hints are read-only. This does not implement the static-key input adapter.

Input provenance, ordered key delivery, preheld/device identity, several workspace/master operations and full native integration remain incomplete. No production-readiness or deployment qualification is claimed.

In the trusted managed-fixture environment, `OMARCHY_VIMARCHY_PRESENTATION=passive` selects this mode. Absence or `interactive-fixture` retains the existing interactive test mode; unknown or non-UTF8 values refuse configuration. The isolated-display launch guard remains required. This is not a production launcher or an instruction to enable live bindings.

Independent development review covered passive GTK controller absence, keyboard mode and actual Wayland input-region requests, including compiled negative controls. An isolated native compositor test confirmed guarded focus and keyboard pass-through using an explicitly trusted fixture controller. That controller's startup corrections and machine-specific harness are not included in this checkout. These results do not establish an authenticated physical-key path, pointer-click pass-through, full compositor gates or release readiness.

## Build and verify

Linux and a current Rust toolchain supporting edition 2024 are required.

```sh
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all --check
```

Native UI requires GTK 4.14 or newer, gtk4-layer-shell, pkg-config and a supported Wayland compositor. It is a separate Cargo workspace:

```sh
cargo test --manifest-path vimarchy-ui/Cargo.toml --locked
cargo clippy --manifest-path vimarchy-ui/Cargo.toml --all-targets --locked -- -D warnings
```

## Source layout and provenance

Required local Rust crates are included as sibling directories so path dependencies resolve without another checkout. `SOURCE-MANIFEST.json` records copied source hashes. Shared crates are snapshot copies; changes must be reconciled with the development workspace before the next publication.

See `THIRD_PARTY.md` for upstream attribution. Local watch state, review sessions, transcripts, machine-specific evidence, build outputs and deployment secrets are excluded. Publication does not install services or enable desktop integrations.

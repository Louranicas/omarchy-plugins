# omarchy-ask-rust

Development snapshot of the Omarchy Rust plugin. This is an independent adaptation, not an upstream release.

## Status

Real provider integration, durable policies, full interaction parity and native handoff remain incomplete. No production-readiness or deployment qualification is claimed.

## Build and verify

Linux and a current Rust toolchain supporting edition 2024 are required.

```sh
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all --check
```

Native UI requires GTK 4.14 or newer, gtk4-layer-shell, pkg-config and a supported Wayland compositor. It is a separate Cargo workspace:

```sh
cargo test --manifest-path ask-native/Cargo.toml --locked
cargo clippy --manifest-path ask-native/Cargo.toml --all-targets --locked -- -D warnings
```

## Source layout and provenance

Required local Rust crates are included as sibling directories so path dependencies resolve without another checkout. `SOURCE-MANIFEST.json` records copied source hashes. Shared crates are snapshot copies; changes must be reconciled with the development workspace before the next publication.

See `THIRD_PARTY.md` for upstream attribution. Local watch state, review sessions, transcripts, machine-specific evidence, build outputs and deployment secrets are excluded. Publication does not install services or enable desktop integrations.

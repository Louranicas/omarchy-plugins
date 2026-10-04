# Omarchy Rust plugins

One self-contained development workspace for Vimarchy, Ask, Yoohoo and Agentd, with shared Rust dependencies included. These are independent adaptations, not official upstream releases. No production deployment qualification is claimed.

## Build and check

Use Linux and a current Rust toolchain supporting edition 2024. Native UI crates require GTK 4.14 or newer, gtk4-layer-shell and pkg-config, and use separate Cargo workspaces.

```sh
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all --check
for ui in ask-native vimarchy-ui yoohoo-ui; do
  cargo test --manifest-path "$ui/Cargo.toml" --locked
  cargo clippy --manifest-path "$ui/Cargo.toml" --all-targets --locked -- -D warnings
  cargo fmt --manifest-path "$ui/Cargo.toml" --check
done
```

## Current scope

- Vimarchy: gesture/workspace core and explicitly selected passive managed renderer. Trusted compositor input routing and full native workflows remain unfinished.
- Ask: session/permission, file, subprocess and transcript foundations. Real provider integration and full interaction coverage remain unfinished.
- Yoohoo: attention/selection and live list refresh. Production notification attribution, persistence and reconnect handling remain unfinished.
- Agentd: local agent registry, process identity and bounded subprocess handling. Real harness/service qualification remains unfinished.

The passive Vimarchy mode is an isolated renderer checkpoint, not a production input adapter. Publishing this source neither installs nor activates services. Installer work and private desktop test infrastructure are outside this plugin snapshot.

See [VALIDATION.md](VALIDATION.md) for checks on this combined checkout. Earlier plugin-specific validation is retained under docs; it is historical evidence, not a replacement for combined-checkout validation. PROVENANCE.json records source commits. Required fixtures retain their upstream provenance; private session state, raw operational evidence and credentials are excluded.

## Attribution

Upstream notices and licensing scope:

- [Vimarchy](docs/omarchy-vimarchy-rust/THIRD_PARTY.md)
- [Ask](docs/omarchy-ask-rust/THIRD_PARTY.md)
- [Yoohoo](docs/omarchy-yoohoo-rust/THIRD_PARTY.md)
- [Agentd](docs/omarchy-agentd-rust/THIRD_PARTY.md)

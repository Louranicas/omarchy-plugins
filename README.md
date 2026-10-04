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

- Vimarchy: gesture/workspace core, passive managed renderer and [experimental matched-input receiver](vimarchy-runtime/INPUT-BRIDGE.md). An [isolated compositor-to-receiver join](tools/native-input-join/README.md) exercises counted intents; production Host wiring and hardware input qualification remain unfinished.
- Ask: session/permission, file, subprocess and transcript foundations, plus [explicit provider profiles with guarded launch](ask-native/PROVIDER-PROFILES.md). [Typed cleanup observations](ask-native/CLEANUP-STATUS.md) and synthetic recovery tests are available; real vendor acceptance, managed restart and full interaction coverage remain unfinished.
- Yoohoo: attention/selection, live list refresh, bounded nonmodal source recovery and an [explicit daemon/control entrypoint](yoohoo-runtime/CONTROLLER.md). New opens acquire fresh authority; uncertain cleanup prevents reuse. Real desktop integration, notification attribution and persistence remain unfinished.
- Agentd: local agent registry, process identity and bounded subprocess handling. Real harness/service qualification remains unfinished.

The passive Vimarchy mode is an isolated renderer checkpoint, not a production input adapter. Publishing this source neither installs nor activates services. Installer work and private desktop test infrastructure are outside this plugin snapshot.

See [the latest integration checkpoint](docs/INTEGRATION-2026-10-04.md) for reviewed changes and open boundaries, and [VALIDATION.md](VALIDATION.md) for checks on this combined checkout. Earlier plugin-specific validation is retained under docs; it is historical evidence, not a replacement for combined-checkout validation. PROVENANCE.json records source commits. Required fixtures retain their upstream provenance; private session state, raw operational evidence and credentials are excluded.

## Attribution

Upstream notices and licensing scope:

- [Vimarchy](docs/omarchy-vimarchy-rust/THIRD_PARTY.md)
- [Ask](docs/omarchy-ask-rust/THIRD_PARTY.md)
- [Yoohoo](docs/omarchy-yoohoo-rust/THIRD_PARTY.md)
- [Agentd](docs/omarchy-agentd-rust/THIRD_PARTY.md)

## Repeatable quality gates

Run `tools/check.sh --offline` for root workspace formatting, all-target tests and strict Clippy. Add `--native` to include all three separate GTK workspaces; native checks require their documented system libraries. Each command has a ten-minute timeout with process-group termination. Omit `--offline` when locked dependencies first need downloading.

After dependencies are cached, `python3 tools/mutation-check.py` executes four selected semantic controls for Ask close-queue revocation and runtime identity, Vimarchy reply fencing and Yoohoo revision rollback. Each baseline must run and pass its named test; a mutant must compile and fail that exact test at its expected behavioral assertion. Compilation errors, missing tests, surviving mutants and timeouts fail the check. Sources are copied from tracked files into a private temporary directory with a separate build target; mutations never touch the working source or normal fixture binaries. Owned command process groups are terminated even on normal return; command duration and cleanup waits are bounded. Deliberately detached descendants and hard log-size enforcement are outside this helper’s scope. Receipts live under `target/mutation-evidence/`. These controls are not a whole-project mutation score.

The GitHub workflow uses `--portable` to check root Rust workspace tests **except the Arch-dependent ask-preview test suite**, plus workspace-wide strict Clippy and selected mutations on Ubuntu with the reviewed Rust toolchain and a read-only token. Default local checks still require ask-preview tests with Bubblewrap, ImageMagick 7, Poppler and the target Arch dynamic-loader layout. Portable CI is a partial gate. It does **not** establish Arch decoder sandbox, GTK/Wayland end-to-end acceptance, provider authentication, ignored real-capture qualification, or production deployment. Those require separate recorded checks. CI follows [GitHub's Rust workflow guidance](https://docs.github.com/en/actions/tutorials/build-and-test-code/rust); checkout is pinned to a reviewed commit and credentials are not persisted.

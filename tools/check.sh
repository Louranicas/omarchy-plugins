#!/usr/bin/env bash
# Run from any directory. No installation or service activation.
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
native=false
portable=false
locked=(--locked)
for arg in "$@"; do
  case "$arg" in
    --portable) portable=true ;;
    --native) native=true ;;
    --offline) locked+=(--offline) ;;
    *) printf 'Usage: %s [--native] [--offline] [--portable]\n' "$0" >&2; exit 2 ;;
  esac
done
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
export CARGO_PROFILE_DEV_DEBUG="${CARGO_PROFILE_DEV_DEBUG:-0}"
check() { printf '\nChecking:'; printf ' %q' "$@"; printf '\n'; timeout --kill-after=10s 10m "$@"; }
check cargo fmt --all -- --check
test_scope=()
if "$portable"; then
  # Preview sandbox tests depend on Arch's dynamic-loader layout and tools.
  # They remain mandatory in the default local gate; this exclusion is explicit.
  test_scope+=(--exclude ask-preview)
fi
check cargo test --workspace --all-targets "${test_scope[@]}" "${locked[@]}"
check cargo clippy --workspace --all-targets "${locked[@]}" -- -D warnings
if "$native"; then
  for ui in ask-native vimarchy-ui yoohoo-ui; do
    check cargo fmt --manifest-path "$ui/Cargo.toml" -- --check
    check cargo test --manifest-path "$ui/Cargo.toml" --all-targets "${locked[@]}"
    check cargo clippy --manifest-path "$ui/Cargo.toml" --all-targets "${locked[@]}" -- -D warnings
  done
fi

#!/usr/bin/env bash
# One-time setup: fetches magicblock-validator (dev branch) and builds everything.
#
# Usage:
#   ./setup.sh                  clone magicblock-validator@dev into ./magicblock-validator
#   ./setup.sh --link <path>    symlink an existing magicblock-validator checkout instead
set -euo pipefail
cd "$(dirname "$0")"

VALIDATOR_REPO_URL="https://github.com/magicblock-labs/magicblock-validator.git"
VALIDATOR_BRANCH="dev"
# The dev-branch commit this PoC was validated against. Bump deliberately —
# the bench shares Cargo pins and config schema with the validator tree.
VALIDATOR_COMMIT="c0de404ff"
VALIDATOR_DIR="magicblock-validator"

say()  { printf '\033[1;34m[setup]\033[0m %s\n' "$*"; }
fail() { printf '\033[1;31m[setup]\033[0m %s\n' "$*" >&2; exit 1; }

# ---------- prerequisites ----------
command -v git >/dev/null   || fail "git is required"
command -v cargo >/dev/null || fail "Rust is required — install via https://rustup.rs"
command -v rustup >/dev/null || fail "rustup is required — install via https://rustup.rs"
if ! command -v solana-test-validator >/dev/null; then
  fail "solana-test-validator not found on PATH.
        Install the Agave tools:  sh -c \"\$(curl -sSfL https://release.anza.xyz/stable/install)\"
        then add ~/.local/share/solana/install/active_release/bin to PATH and re-run."
fi
say "prerequisites OK ($(cargo --version | cut -d' ' -f1-2), $(solana-test-validator --version | cut -d' ' -f1-2))"

# ---------- validator checkout ----------
if [ "${1:-}" = "--link" ]; then
  [ -n "${2:-}" ] || fail "--link requires a path to an existing magicblock-validator checkout"
  SRC="$(cd "$2" && pwd)"
  [ -d "$SRC/test-integration/configs" ] || fail "$SRC does not look like a magicblock-validator checkout"
  rm -rf "$VALIDATOR_DIR"
  ln -s "$SRC" "$VALIDATOR_DIR"
  say "linked $VALIDATOR_DIR -> $SRC"
elif [ -e "$VALIDATOR_DIR" ]; then
  say "$VALIDATOR_DIR already present, checking out pinned commit $VALIDATOR_COMMIT"
  git -C "$VALIDATOR_DIR" fetch origin "$VALIDATOR_BRANCH" || true
  git -C "$VALIDATOR_DIR" checkout "$VALIDATOR_COMMIT"
else
  say "cloning $VALIDATOR_REPO_URL ($VALIDATOR_BRANCH branch, pinned at $VALIDATOR_COMMIT)"
  git clone -b "$VALIDATOR_BRANCH" "$VALIDATOR_REPO_URL" "$VALIDATOR_DIR"
  git -C "$VALIDATOR_DIR" checkout "$VALIDATOR_COMMIT"
fi

# ---------- builds ----------
# Pin the toolchain from the validator's own rust-toolchain.toml so a stray
# rustup directory override cannot select an incompatible compiler.
TOOLCHAIN="$(sed -n 's/^channel *= *"\(.*\)"/\1/p' "$VALIDATOR_DIR/rust-toolchain.toml" | head -1)"
say "building magicblock-validator (release, toolchain ${TOOLCHAIN:-default}) — first build takes a few minutes"
( cd "$VALIDATOR_DIR" && RUSTUP_TOOLCHAIN="${TOOLCHAIN}" cargo build --release -p magicblock-validator )

say "building er-bench (release)"
( cd bench && cargo build --release )

say "done. Start the benchmark with:  ./run.sh"

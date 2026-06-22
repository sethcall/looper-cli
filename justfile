# looper-cli — task runner.
# Conventions, crate boundaries, and dev-quality gates: see AGENTS.md.
set shell := ["bash", "-cu"]

# List available recipes.
default:
    @just --list

# ----------------------------------------------------------------------------
# Dev-quality gates
# ----------------------------------------------------------------------------

# Format all Rust code.
fmt:
    cargo fmt --all

# Check formatting without modifying files.
fmt-check:
    cargo fmt --all --check

# Clippy with warnings denied.
lint:
    cargo clippy --all-targets --all-features -- -D warnings

# Run the test suite.
test:
    cargo test --workspace

# Full gate: format-check + clippy(-D warnings) + tests.
check: fmt-check lint test

# ----------------------------------------------------------------------------
# Build
# ----------------------------------------------------------------------------

# Debug build of the whole workspace.
build:
    cargo build --workspace

# Release build.
build-release:
    cargo build --workspace --release

# ----------------------------------------------------------------------------
# Setup / doctor
# ----------------------------------------------------------------------------

# Verify the toolchain this repo needs. Pure Rust — no system libraries, no Node, no Python.
doctor:
    #!/usr/bin/env bash
    set -uo pipefail
    miss=0
    echo "==> looper-cli toolchain"
    if command -v cargo >/dev/null 2>&1; then echo "  ok   $(cargo --version)"; else
      echo "  MISS cargo — install Rust via https://rustup.rs"; miss=1; fi
    if command -v rustc >/dev/null 2>&1; then echo "  ok   $(rustc --version)"; else
      echo "  MISS rustc — install Rust via https://rustup.rs"; miss=1; fi
    if v=$(cargo fmt --version 2>/dev/null); then echo "  ok   rustfmt ($v)"; else
      echo "  MISS rustfmt — rustup component add rustfmt"; miss=1; fi
    if v=$(cargo clippy --version 2>/dev/null); then echo "  ok   clippy ($v)"; else
      echo "  MISS clippy — rustup component add clippy"; miss=1; fi
    if [ "$miss" -eq 0 ]; then echo "All good. Try: just check"; else
      echo "Some tools are missing (see MISS above)."; exit 1; fi

# Verify the toolchain and install the git pre-commit hook (runs the gates on staged Rust changes).
setup: doctor
    git config core.hooksPath .githooks
    @echo "Installed pre-commit hook (.githooks/pre-commit)."

# ----------------------------------------------------------------------------
# Demo (see README "Demo") — run from the repo root, two terminals.
# ----------------------------------------------------------------------------

# Terminal 1: the live Activity TUI over the bundled demo repos.
demo-tui:
    cargo run -p looper-cli -- tui demo/frontend demo/backend demo/sre --kb demo/kb

# Terminal 2: generate a live stream of changes over the demo repos (~3/s; Ctrl-C restores them).
# `just demo reset` instead strips any leftover demo markers from the docs (after a hard kill).
demo *ARGS:
    cargo run -p looper-cli -- demo {{ ARGS }}

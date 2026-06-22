# looper-cli — AGENTS.md

`looper-cli` is a **Rust-only engine** for AI-generated docs: it harvests markdown, indexes it into an
**OKF** knowledge base ("KB"), watches files for changes, and builds/updates the index — with no
Tauri, no GUI, and no LLM dependency.

It is consumed by a separate desktop application (not part of this repository).

## The boundary (one rule)

**Tauri code and LLM/enrichment features live in the desktop repo; everything else lives here.**
The dependency direction is always **desktop → looper-cli**, never the reverse.

- **Here (looper-cli):** the engine crates below + the `looper-cli` binary (Activity-view TUI
  planned). Indexing is **pure Rust** (`looper-okf`) — no Python or OKF submodule required.
- **Desktop repo:** the Tauri shell + React UI, installers, first-run help, the concrete Gemini
  enricher (`looper-okf-enricher`) + OS-keychain API-key storage, **and the OKF Python submodule
  (`vendor/knowledge-catalog`)** — which only the LLM enrichment path uses, so it lives with
  enrichment in the desktop repo, not here.

## Crate map (14 crates)

**Foundational** (leaves — external deps only):

- `looper-config` — layered config (defaults → JSON file → env → CLI) + platform-dir resolution.
- `looper-observability` — `tracing` subscriber, rolling file logs, panic hooks. _(may depend on config)_
- `looper-state` — atomic JSON persistence of app state; schema versioning/migration.
- `looper-ipc` — shared serde DTOs (source for generated TS types). **Must not depend on `tauri`.**

**Domain:**

- `looper-watcher` — `notify` + debounce; OS watch-limit detection + per-OS hints; watch-health.
- `looper-git` — `gix` repo discovery + tracking.
- `looper-workspace` — workspace model + `.looper` symlink/junction + scan/watch exclusions.
- `looper-scan` — gitignore-aware markdown walk + blake3/mtime fingerprints + startup catch-up.
- `looper-kb` — the **swappable KB trait** + DTOs + in-memory `MockKb`.
- `looper-okf` — concrete OKF producer (emit bundle md, `okf_id`, sidecar index) + Rust viz renderer.
- `looper-sync` — the **swappable sync seam** (`Syncer`/`SyncBackend` trait) + git-CLI backend + `MockSyncer`.
- `looper-enrichment` — the **enrichment seam** (`Enricher` trait + error/result types + `MockEnricher`).
  Pure Rust; the concrete Gemini enricher lives in the desktop repo (`looper-okf-enricher`).

**Orchestration / binary:**

- `looper-core` — engine wiring watcher+scan+workspace+git+**KB trait**+**sync trait**+**enrichment
  trait**; event bus; clean API. Never names a concrete backend.
- `looper-cli` — the terminal binary (composition root that injects `looper-okf`). `scan` (one-shot),
  `watch` (live; Ctrl-C drains + joins), and `tui` (live Activity view, `ratatui`) each take their
  source as **ad-hoc folders + `--kb`** or a saved **`--workspace`**; `scan`/`watch` support `--json`
  (JSONL). `workspace create` (interactive, `inquire`) + `workspace list` read/write the same JSON
  config the desktop app uses. Modules: `session` (workspace resolution + engine open), `tui`,
  `workspace`.

## Dependency-direction rules (keep the graph acyclic)

- Leaves depend only on external crates: `config`, `ipc`, `state`, `watcher`, `git`, `kb`.
- `observability` → config · `workspace` → {state, git, config} · `scan` → {state, config, ipc} ·
  `okf` → {kb, state, ipc} · `sync` → {ipc} · `enrichment` → {ipc}.
- `core` → {config, observability, state, ipc, watcher, git, workspace, scan, **kb (trait)**,
  **sync (trait)**, **enrichment (trait)**} — **never `looper-okf`** and never a concrete enricher.
- The composition root (the `looper-cli` binary; the desktop `looper-app`) is the only place that
  names concrete backends.

## Tech stack

tokio · tracing (+subscriber/appender) · thiserror (libs) + anyhow (binary) · figment + clap (config) ·
directories · notify + notify-debouncer-full · serde / serde_json · **serde_yaml_ng** with **IndexMap**
frontmatter · blake3 · **gix** · `ignore` · `ts-rs` (Rust→TS types) · `junction`/`dunce` (Windows).

## Dev-quality gates

- `just setup` — verify the toolchain (pure Rust; no system libs/Node/Python) + install the
  pre-commit hook (`.githooks/pre-commit`, runs the gates on staged Rust changes). `just doctor`
  runs just the checks.
- `just fmt-check` · `just lint` (`clippy -D warnings`) · `just test` · `just check` (all three).
- CI (`.github/workflows/ci.yml`) runs `just check` on Linux/macOS/Windows.
- `[workspace.lints]` denies warnings centrally; each crate sets `[lints] workspace = true`.

## Docs conventions

- **Keep the README "Usage" `--help` blocks in sync with the code.** Whenever you change the clap
  arguments of `scan`, `watch`, or `tui`, regenerate and update that command's `--help` code block in
  [`README.md`](./README.md)'s **Usage** section (each is marked with an HTML comment, e.g.
  `<!-- usage:scan -->`). Just run `cargo run -p looper-cli -- <scan|watch|tui> --help` and paste the
  output. Add a new block if a new run command is introduced.

- If you modify the tape file, re-run it with: `vhs demo/scripts/looper-cli-demo.tape`

## Standalone vs. consumed

These crates build and test standalone (`just check`) with no other repo present. A separate
application consumes them as path dependencies for side-by-side development; that app pins a specific
`looper-cli` git revision for its releases.

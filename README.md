# looper-cli

[![CI](https://github.com/sethcall/looper-cli/actions/workflows/ci.yml/badge.svg)](https://github.com/sethcall/looper-cli/actions/workflows/ci.yml)

The Rust engine behind [**Looper**](https://runlooper.dev) — auto-harvest markdown docs as you work,
index them into an [**Open Knowledge Format (OKF)** knowledge
base](https://cloud.google.com/blog/products/data-analytics/how-the-open-knowledge-format-can-improve-data-sharing),
watch files for changes, and build/update the index, all from the terminal. No GUI, no LLM
dependency: pure Rust.

> **Status:** early development. This repo is being carved out of the Looper desktop app.

<img alt="Looper CLI live OKF indexing demo" src="https://raw.githubusercontent.com/sethcall/looper-cli/main/demo/scripts/looper-cli-demo.gif" width="1200" />

## Why OKF?

OKF is extremely simple: markdown files, lightweight frontmatter, and normal links. That makes it a
good bolt-on to however you already work. The data flow can start in your git repos, where docs are
easy to review and version, then end up elsewhere as a read-only indexed knowledge base for browsing,
search, sync, or agent consumption.

That simple wiki adds value on its own because it gives people and tools a stable, readable map of a
workspace without forcing a new authoring workflow. With additional tooling, Looper being one example,
the same corpus also has a clear path to LLM enrichments: generate summaries, citations, cross-links,
metadata, or other derived knowledge while keeping the human-authored sources intact.

Looper's [`looper-okf`](./crates/looper-okf) crate is the native Rust implementation of the producer
side of the [OKF repository's reference Python](https://github.com/GoogleCloudPlatform/knowledge-catalog/tree/main/okf):
it emits OKF bundle markdown and maintains the sidecar index. It is producer-first, with a small
Rust visualization renderer that reads a bundle to generate `viz.html`, not a general-purpose OKF
consumer API.

OKF was inspired by Andrej Karpathy's
[LLM wiki](https://gist.github.com/karpathy/442a6bf555914893e9891c11519de94f) ideas: keep knowledge
in plain files that humans can read, agents can navigate, and tools can improve over time.

## What's here

This is the project that houses the Rust code of all deterministic (non-AI) features of the
`Looper` desktop app. To illustrate how this set of libraries work, we turned it into `looper-cli`,
a lightweight alternative to the Looper product.

The workspace is split into focused crates:

- Config: [`looper-config`](./crates/looper-config)
- Observability: [`looper-observability`](./crates/looper-observability)
- State persistence: [`looper-state`](./crates/looper-state)
- IPC DTOs: [`looper-ipc`](./crates/looper-ipc)
- File watching: [`looper-watcher`](./crates/looper-watcher)
- Git discovery and tracking: [`looper-git`](./crates/looper-git)
- Workspace model, symlinks, and exclusions: [`looper-workspace`](./crates/looper-workspace)
- Markdown scan and fingerprinting: [`looper-scan`](./crates/looper-scan)
- Swappable KB trait and test double: [`looper-kb`](./crates/looper-kb)
- Concrete OKF producer and visualization renderer: [`looper-okf`](./crates/looper-okf)
- Sync seam and git-backed sync implementation: [`looper-sync`](./crates/looper-sync)
- Enrichment seam and mock enricher: [`looper-enrichment`](./crates/looper-enrichment)
- Orchestration engine: [`looper-core`](./crates/looper-core)
- Terminal binary and composition root: [`looper-cli`](./crates/looper-cli)

See [`AGENTS.md`](./AGENTS.md) for the full crate map and dependency rules.

## Quickstart

```sh
just setup                                          # verify toolchain + install the pre-commit hook
cargo run -p looper-cli -- tui ./docs ./notes --kb ./looper-index  # quick start: live Activity view
cargo run -p looper-cli -- workspace create         # interactively set up a saved workspace
cargo run -p looper-cli -- workspace create --discover ~/workspace # pick sibling repos from a checklist
cargo run -p looper-cli -- workspace list           # list saved workspaces + their config
cargo run -p looper-cli -- scan  --workspace mydocs # build/update a saved workspace's index, once
cargo run -p looper-cli -- watch --workspace mydocs # re-index it live until Ctrl-C
```

Every run command (`scan`/`watch`/`tui`) takes its source the same way — either **ad-hoc folders**
(one or more, with `--kb <dir>` for the index output — **you're prompted for it if omitted**, and
nudged to `workspace create` so you needn't answer again) **or a saved `--workspace <name>`**:

- **`workspace create` / `list`** — write + inspect the same JSON workspace config the desktop app
  uses (`create` is interactive, can scan sibling folders with `--discover`, or can be scripted with
  `--name`/`--folder`/`--specs-folder`/`--kb-dir`/`--yes`; both take `--store`).
- **`scan`** — index once and exit. `--json` emits JSONL events + a summary.
- **`watch`** — scan, then re-index live as files change; **Ctrl-C drains + exits cleanly**. `--json`
  streams JSONL events (good piped into a service/log).
- **`tui`** — live **Activity** view (recent activity + index status). **Tab** focuses the list,
  **↑/↓** select a document, **Enter** opens it in a markdown viewer (**Esc** back); **q** quits.
  Pass ad-hoc folders + `--kb` to see something immediately without creating a workspace.

## Usage

Ad-hoc, choosing the index output dir with `--kb` (no workspace needed):

```sh
# Index two folders into a chosen output dir, then watch them live (JSONL events):
looper-cli watch ./docs ./design --kb /tmp/looper-index --json

# One-shot index of one folder into ./out:
looper-cli scan ./notes --kb ./out
```

The three run commands (`scan` / `watch` / `tui`) share the same source flags. Their `--help`:

<!-- usage:scan (keep in sync with the `Scan` clap args — see AGENTS.md) -->

```text
Scan once and build/update the OKF index, reporting what was indexed

Usage: looper-cli scan [OPTIONS] [FOLDER]...

Arguments:
  [FOLDER]...  Source folder(s) to index — one or more. Omit when using `--workspace`

Options:
      --workspace <NAME>  Use a saved workspace (by name or id) instead of ad-hoc folders
      --kb-dir <DIR>      OKF index output dir for ad-hoc folders. Prompted if omitted; not needed with --workspace [aliases: --kb]
      --store <FILE>      Workspace store file (default: the CLI config dir's `workspaces.json`)
      --json              Emit engine events + a summary as JSON lines (machine-readable)
  -h, --help              Print help
```

<!-- usage:watch (keep in sync with the `Watch` clap args — see AGENTS.md) -->

```text
Scan, then keep watching and re-indexing live until interrupted (Ctrl-C drains + exits)

Usage: looper-cli watch [OPTIONS] [FOLDER]...

Arguments:
  [FOLDER]...  Source folder(s) to index — one or more. Omit when using `--workspace`

Options:
      --workspace <NAME>  Use a saved workspace (by name or id) instead of ad-hoc folders
      --kb-dir <DIR>      OKF index output dir for ad-hoc folders. Prompted if omitted; not needed with --workspace [aliases: --kb]
      --store <FILE>      Workspace store file (default: the CLI config dir's `workspaces.json`)
      --json              Emit engine events as JSON lines (machine-readable; good for services)
  -h, --help              Print help
```

<!-- usage:tui (keep in sync with the `Tui` clap args — see AGENTS.md) -->

```text
Live Activity view in the terminal (a TUI): folder activity + index status

Usage: looper-cli tui [OPTIONS] [FOLDER]...

Arguments:
  [FOLDER]...  Source folder(s) to index — one or more. Omit when using `--workspace`

Options:
      --workspace <NAME>  Use a saved workspace (by name or id) instead of ad-hoc folders
      --kb-dir <DIR>      OKF index output dir for ad-hoc folders. Prompted if omitted; not needed with --workspace [aliases: --kb]
      --store <FILE>      Workspace store file (default: the CLI config dir's `workspaces.json`)
  -h, --help              Print help
```

Running `watch` as a background service (systemd / launchd): see
[`docs/running-as-a-service.md`](./docs/running-as-a-service.md).

## Demo

A self-contained demo lives in [`demo/`](./demo) — three mock repos (`frontend`, `backend`, `sre`),
each with `docs/` + `specs/` and a little content. Run it **from the repo root in two terminals**:

```sh
# terminal 1 — the live Activity view over the three repos:
just demo-tui     # cargo run -p looper-cli -- tui demo/frontend demo/backend demo/sre --kb demo/kb

# terminal 2 — stream changes across them (~3/s):
just demo         # cargo run -p looper-cli -- demo
```

Watch the activity feed light up; **Tab** into the list, **↑/↓** to pick a doc, **Enter** to open it
in the markdown viewer. The `demo` command edits each doc with a tiny rotating trailing comment (a
real change — the engine de-dups no-op writes) and **restores every file on Ctrl-C**. The generated
index goes to `demo/kb` (its contents are gitignored). If a hard kill ever leaves markers behind,
`just demo reset` (`looper-cli demo reset`) strips them.

To record the whole flow as a short screencast, run the [VHS](https://github.com/charmbracelet/vhs)
tape from the repo root (after `cargo build --release`):

```sh
vhs demo/scripts/looper-cli-demo.tape   # → demo/scripts/looper-cli-demo.gif (+ .mp4)
```

The rendered output is gitignored — regenerate it from the tape.

## Scope

This repo is the engine only. The GUI, installers, and LLM/Gemini enrichment live in a separate
desktop application that consumes these crates; that app is not part of this repository.

## License

MIT — see [`LICENSE`](./LICENSE).

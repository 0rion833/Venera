# Venera

Venera is a terminal coding agent with a local context engine, tool calling, a user
backup, and token-only context measurement.

It runs offline until you send a chat request. The index, VCE map, backup, and token
measurements never call a model.

## Token report

The full, self-contained report is in [`../token-tests.html`](../token-tests.html). It
contains tokens only: no prices, because provider rates differ.

Measured on this repository with `o200k_base`:

| What is read | Tokens |
|---|---:|
| Whole repository, all text files | 345,250 |
| `src/` only | 232,857 |
| VCE map, 6,000-token budget | 5,997 |
| VCE map plus one full file | 13,525 |
| Saved vs. reading everything | 339,253 (98.3%) |
| Saved vs. reading `src/` | 226,860 (97.4%) |

The 150,000 → 6,000 scenario saves **144,000 tokens (96%)**. On the current repository
the full read is **57.6× larger** than the 5,997-token map.

Reproduce it without an API key:

```bash
venera measure . --max-tokens 6000 --rate 0
```

`measure` is offline; `--rate 0` keeps the output focused on tokens.

## Quick start

From the repository root:

```bash
cargo build --release
./target/release/venera
```

On first launch Venera creates a config and asks for a master password for the encrypted
key vault. Add a provider key with:

```text
/api proxyapi
/api aitunnel
```

Choose a model with `/models`, then start a task with a normal prompt.

## Context and backup

- `/vce` refreshes the live index (`index1`), renders the project map, and snapshots
  user-owned files into `.venera/index2.db`.
- `/back` restores the user snapshot and removes files created by the model that have no
  user snapshot.
- A one-second poller notices files created or edited outside Venera and updates both
  indexes. Model writes are reindexed first and never overwrite the user backup.
- `index1` is dynamic. `index2` is static with respect to the model.

## Tools

Chat exposes a compact tool set:

- `search_files`, `get_file` — read and locate files;
- `append_file` — add text without deleting existing text;
- `write_file` — create a new file; existing files need `overwrite=true`;
- `edit_file` — replace one exact fragment; large shrink needs `allow_shrink=true`;
- `bash` — run tests, linters, or builds in the project root.

Every write and every shell command asks for confirmation. Writes show old/new text;
shell commands show the command line. `bash` runs `cmd /C` on Windows and `/bin/sh -c`
on macOS/Linux, with a 120-second default timeout and a 400-line output cap.

## Languages and themes

Everything the binary needs is inside this directory:

```text
src/i18n/ru.yml
src/i18n/en.yml
src/themes/orange.xml
src/themes/venera-dark.xml
```

`mono` is a compiled-in palette. Project-local `themes/` directories are still read
first, so custom themes continue to work.

## Project layout

```text
vce/index/     SQLite index, scanner, ranking, map
vce/context/   routing, segmentation, compression
vce/tools/     model-facing tools
vce/backup.rs  index2 snapshot and restore
tui/           terminal UI
i18n/          Russian and English catalogs
themes/        shipped XML themes
```

Integration and end-to-end tests live in the repository `tests/` directory.

## Checks

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

The suite includes an offline end-to-end test that creates a temporary project, snapshots
it, edits it as a user, edits it through a model tool, restores the backup, and runs a
shell command.

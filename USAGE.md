# Venera — usage and cost

A short operator's guide. The design rationale lives in `TOOL.md`; measured numbers in
`REPORT.md`. Everything below is offline and needs no API key.

## 1. What costs what

Prices for `gpt-6-luna` in RUB per 1M tokens (`config/default.json`): input 30, cache read
3, cache write 37.5, output 150. So:

| one output token is worth | input tokens | cache-read tokens |
|---|---:|---:|
| 5 | 5 | 50 |

Two consequences that drive every other rule:

1. **The reply is the expensive half.** In a 100-turn session, output is about 75% of the
   bill (`REPORT.md` §0.5C). Saving a thousand input tokens is worth less than saving two
   hundred output tokens.
2. **Replies are re-sent.** Whatever the model writes becomes input on every following
   turn — cached at 3 ₽/1M if the prefix is stable. A verbose reply has a tail.

## 2. Command cheat sheet

| Command | Use it for |
|---|---|
| `venera index [path]` | build or update `.venera/index.db`; unchanged files are skipped |
| `venera map [path] --task TEXT` | signatures only, no source; the cheapest way to see structure |
| `venera search <query>` | files and symbols in one pass, with line ranges |
| `venera outline <file>` | symbols of one file, no source read |
| `venera cat <file> --from N --to M` | a verified line range; prefer it over the whole file |
| `venera refs <name>` / `venera deps <file>` | who mentions a name, what imports what |
| `venera context "<request>"` | the context package the agent would build, without a model |
| `venera cache stats` / `cache clear` | local M0+M1 cache |
| `venera budget` | how a model window is split; shows the per-turn reply cap |
| `venera measure <path>` | tokens and RUB with and without the index |
| `venera tui [path]` | interactive shell: transcript, model picker, themes, sessions |

## 3. When the map is cheaper, and when it is not

The map has a fixed price `M`, a file costs `F`, a range costs `R`:

```text
the map pays off only when  files_read > (M + R) / F
```

- One named file, whole: reading it directly is cheaper. `venera map test --max-tokens 10`
  is a hint, not a habit.
- Several unknown files: the map wins. On this repository it is ~996 tokens against
  95 918 for reading everything (`REPORT.md` §2.1).
- A range instead of a whole file moves the break-even almost to zero, because `R` is much
  smaller than `F`.

## 4. Writing a cheap task

1. Name the file, the symbol, or the error. A path match outranks everything else in
   ranking (`vce/index/rank.rs`).
2. Ask about one thing per turn. Each turn re-sends the history, and each reply is billed
   at the output rate.
3. Say "change" only when you want a change: `explain`, `where`, `find` are routed to
   smaller reply ceilings than `modify` (`vce/context/route.rs`).
4. If the answer is a fact, expect `path:line`, not a paragraph — the system prompt asks
   for exactly that.

## 5. Knobs that move the bill

| Setting | Default | Effect |
|---|---|---|
| `limits.max_output_tokens_per_turn` | 2000 | hard ceiling on one reply, thinking included |
| `limits.session_budget_rub` | 500 | the agent stops and asks when the session hits it |
| `vendor.models.*.reasoning` | `medium` for luna | thinking tokens are billed as output; `off` is the cheapest |
| `index.map.default_token_budget` | 1000 | size of the map when `--max-tokens` is not given |

`venera budget` prints the resolved cap in `reserve` and `turn cap`. A value in the config
wins over the per-operation default, and the model's own `max_output` is still a hard bound.

Note: `config/default.json` ships `reasoning: "medium"` for `gpt-6-luna`. On simple tasks
`--reasoning off` (or changing that line) is the single largest saving available, larger
than any context compression (`REPORT.md` §0.5C).

## 6. Measuring instead of guessing

```powershell
venera measure . --rate 30 --baseline-top 5
venera context "read src/vce/index/map.rs --max-tokens 400" --format json
venera cache stats
venera budget --window 1050000
```

`measure` counts input only. `REPORT.md` §1.4 lists what it does not model: prompt cache,
tool schemas, output, thinking. Treat its percentages as input-side numbers, not as the
total bill.

## 7. Terminal UI

`venera tui` opens the current directory, `venera tui <path>` another project. While the
alternate screen is active nothing is printed to the terminal; logs go to
`<data-local>/venera/logs/tui.log`.

From top to bottom: wordmark, directory and the active model with its context window; the
transcript; a status bar whose meter and dot breathe white; key hints; the input line.
The status bar shows the prepared context against `index.map.default_token_budget` on the
left and the session's spend on the right — input, cached input when the provider reports
it, output, and rubles. The ruble figure is pinned to the right edge, so a narrow terminal
truncates the meter instead of hiding the price.

The transcript grows upwards from the input and is capped at 4000 entries: the oldest lines
are dropped first, which keeps a long session cheap to redraw. A blank line opens every new
turn and closes a block of engine output; inside that output, diff lines starting with `+`
and `-` are drawn green and red, so a patch reads as a patch.

The input is a small editor rather than a single field: it grows to six rows, wraps at word
boundaries, and the caret can sit anywhere in the text.

| Key | Does |
|---|---|
| `Enter` | run the line: a slash command, or a context request; a multi-line prompt is sent as one line |
| `Shift+Enter` | start a new line without sending |
| `Tab` | complete the slash command being typed; commands with arguments get a trailing space |
| `←` / `→`, `Home` / `End` | move the caret; `Ctrl+A` / `Ctrl+E` do the same as `Home` / `End` |
| `Ctrl+←` / `Ctrl+→` | jump by words |
| `Backspace` / `Delete` | remove the character before / under the caret |
| `Ctrl+W`, `Ctrl+Backspace` | delete the word before the caret |
| `Ctrl+U` | clear the input |
| `↑` / `↓` | walk the input history; past the newest line the draft comes back |
| `PgUp` / `PgDn`, `Ctrl+↑` / `Ctrl+↓` | scroll the transcript |
| `Esc` | close the overlay, or clear the input |
| `Ctrl+L` | clear the transcript and show the welcome lines again |
| `Ctrl+C` | leave |

The input history is stored with the session, so the lines typed yesterday come back on the
next start and survive `/new`.

Mouse and paste work through `crossterm`:

| Event | Does |
|---|---|
| wheel | scrolls the transcript three lines, or moves the model picker one row |
| left click on a picker row | applies that model |
| left click outside the picker | dismisses it |
| left click on a completion row | completes that command |
| left click while help is open | closes help |
| paste | inserted into the input, newlines become spaces |

Long input wraps inside the box: the caret stays visible while the prompt is taller than the
six rows the box may take.

Slash commands mirror the CLI: `/index`, `/map`, `/context`, `/s`, `/outline`, `/cat`,
`/deps`, `/cache`, `/skills`, plus `/model`, `/provider`, `/api`, `/theme`, `/new`,
`/sessions`, `/session`, `/cwd`, `/cost`, `/help`. `/model` opens a picker over curated
config models and the active provider's catalog; typing filters, `Enter` applies. The
completion popup sits on top of the status bar, never over it.

`/provider` lists the supported aggregators (ProxyAPI, AITUNNEL). Choosing one with no key
opens a masked prompt: paste the key, press `Enter`, and Venera stores it in the encrypted
vault (master password at startup) and downloads that provider's public model catalog.
`/api` lists every aggregator with its masked key (`sk-490…`) and cached model count;
`/api <id>` replaces one key. The CLI equivalent is `venera providers [--refresh]`.

### Key vault

The first launch asks for a master password (at least 8 characters). Argon2id stretches
it into a key, and the key map is sealed with XChaCha20-Poly1305 in
`<data-local>/venera/vault.json` (`0600` on Unix). The master password is never stored:
a wrong password fails the authentication tag. API keys are encrypted, not hashed —
a hash cannot be decrypted for use or for the masked list.

`/api` shows masked keys only. `/vault reset` asks for confirmation and then deletes the
vault together with every stored API key (and legacy keyring/file copies). The next
launch asks to create a new master password. CLI commands can unlock non-interactively
with `VENERA_VAULT_PASSWORD` (or use `VENERA_VAULT_PATH` for a portable vault).

### AITUNNEL

1. Take a key in the AITUNNEL panel (`https://aitunnel.ru/panel`) or export
   `AITUNNEL_API_KEY`.
2. In Venera run `/provider`, pick `AITUNNEL`, paste the key, press `Enter`; the public
   catalog loads 201 chat models with RUB prices. `/models` switches the model.
3. A plain prompt is sent to AITUNNEL as one OpenAI-compatible request:

```bash
curl https://api.aitunnel.ru/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $AITUNNEL_API_KEY" \
  -d '{
  "model": "gpt-6-luna",
  "reasoning": { "effort": "medium" },
  "messages": [
    {"role": "user", "content": "Сколько букв «р» в слове «пересмотр»?"}
  ]
}'
```

The body is the same as ProxyAPI (`model`, `messages`, `stream: false`); the only
difference is the thinking hint: ProxyAPI sends `"reasoning_effort": "medium"`, while
AITUNNEL sends `"reasoning": { "effort": "medium" }`. Venera picks the right field per
provider automatically.

Themes are `venera-dark` (black background, near-white text) and `mono`. A project can ship
`themes/<name>.xml`; a file overrides only the slots it declares and inherits the rest from
the builtin with the same name (`mono.xml` from `mono`), falling back to `venera-dark` for an
unknown name. `/theme` lists themes, `/theme mono` switches, and
`venera theme validate themes/venera-dark.xml` checks a file.

Sessions are JSON documents under `<data-local>/venera/sessions/<project-key>/`, scoped by
the canonical project path. The newest session reopens on start; `/new` starts another and
`/sessions` lists them with spend. Session ids are unique even when two are created in the
same millisecond. `/new` starts a fresh session with zero spend; `/clear` clears the
transcript and the model conversation but keeps money already spent in that session.

The shell sends a plain prompt to the active provider as a streaming chat request
(OpenAI-compatible SSE, with AITUNNEL's `reasoning` object when thinking is on). Text
deltas appear as they arrive; tool calls are accumulated and run only after the turn ends.
Token usage and the provider-reported charge are appended when the provider sends them;
`/context` still previews the prepared context without calling the model. Chat offers a
compact tool set — `search_files`, `get_file`, `append_file`, `write_file`, `edit_file`,
`bash` — instead of
the full registry, so a reset context stays near 0.02–0.03 RUB, not 0.10. `append_file`
adds text without deleting it; by default it inserts a newline unless the file already
ends with whitespace or `content` starts with whitespace, and `separator: "none"` appends
exactly. `write_file` creates only and refuses an existing file
unless `overwrite=true`; `edit_file` replaces one exact fragment and refuses to remove
most of it unless `allow_shrink=true`. Every write waits for
the confirmation box (`Enter` allow, `Esc` deny), which shows old/new text and warns about
a full rewrite or a much shorter replacement; every path stays inside `PathGuard`. The
map/outline/deps tools remain available to `/context` and the CLI, not to every chat turn.
Catalog models without the `Функции` capability (for example `phi-4`) are sent without
tools; `/models` marks them `без инструментов`, so they chat but cannot edit files.
ProxyAPI rejects `reasoning_effort` together with function tools on `/chat/completions`,
so Venera sends `reasoning_effort: "none"` whenever tools are offered; AITUNNEL keeps its
`reasoning` object.

`bash` runs `cmd /C <command>` on Windows and `/bin/sh -c <command>` on macOS/Linux in the
project root. Every command needs confirmation, has a 120 s default timeout (600 s max),
and its output is capped at 400 lines. Venera removes its own API-key variables from the
child environment. `bash` can run the project's tests, linter, or build; it is not a
sandbox, so read the command before pressing Enter.

`/vce` also writes `.venera/index2.db`, the user backup. `index1` stays the live index;
files last changed by the model are marked `ai` and are not copied. `/back` restores every
snapshot file and removes AI-created files that have no user snapshot. A one-second poller
watches the project: when the user creates or edits a file outside Venera, it updates
`index1` and `index2`; AI writes are reindexed first, so they never overwrite the backup.

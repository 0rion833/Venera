# Venera

A terminal coding agent with a local context engine, tools, streaming chat, and a user
backup.

## Documentation

Everything lives in [`src/README.md`](src/README.md):

- token measurements and the 150,000 → 6,000 scenario;
- build and first-run instructions;
- tools, backup, languages, and themes;
- project layout and checks.

The standalone token report is [`token-tests.html`](token-tests.html).

## Build

From the repository root:

```bash
cargo build --release
./target/release/venera
```

A prebuilt Windows release is in `dist/venera.exe`; the folder is ignored by Git and is
meant for GitHub Releases, not a commit.

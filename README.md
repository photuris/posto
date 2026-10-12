# Posto

A bespoke personal email client. Proof of concept: one Fastmail account
over JMAP, with a Dioxus desktop GUI and a scriptable CLI over the same
Rust core.

## Layout

| Crate | What it is |
|---|---|
| `crates/posto-core` | All logic: config, secrets, SQLite store (WAL + FTS5), JMAP client, sync, actions, body fetch, compose/send, HTML sanitising, Markdown, Omarchy theme hints, keyboard shortcut map, UI fixtures. |
| `crates/posto-cli` | `posto`, a noun-verb CLI. JSON on stdout, JSON errors on stderr, typed exit codes, no prompts. |
| `crates/posto-gui` | `posto-gui`, a three-pane Dioxus 0.7 desktop app. |

## Build and test

```sh
cargo build --workspace
cargo test --workspace
cargo run -p posto-gui            # the app
cargo run -p posto-cli -- --help  # the CLI
```

No `dx` CLI is needed. On Linux the GUI needs WebKitGTK 4.1 and a
Wayland or X11 session.

## First run

1. Create a Fastmail API token (Settings, Privacy & Security, API tokens)
   with mail read/write and submission scopes.
2. Store it and set your sender address:

   ```sh
   printf '%s' "$TOKEN" | posto account set-token --token-stdin
   posto account set --email you@fastmail.com
   posto sync run
   ```

   The GUI Settings window does the same thing.
3. Start `posto-gui`. It syncs every 5 minutes (configurable, 0 = off)
   and on `g`.

Data lives in `$POSTO_DATA_DIR`, default `~/.local/share/posto`:
`config.toml`, `secrets.toml` (mode 0600; `POSTO_TOKEN` in the
environment overrides it), `posto.db`, `sync.lock`. The CLI and GUI share
the database; sync is single-writer through the lock file.

## Keyboard shortcuts (defaults)

`j`/`k` next/previous, `Enter` open, `Esc` close, `Backspace` back,
`e` archive, `#` delete, `!` junk, `r` reply, `a` reply all, `c` compose,
`u` toggle read, `s` toggle flag, `m` move, `/` search, `g` sync,
`Ctrl+Enter` send. Override any of them in Settings or in `config.toml`:

```toml
[shortcuts]
archive = "y"
delete = "ctrl+d"
```

## CLI

`posto <noun> <verb>`; every command's `--help` ends with the JSON shape
it prints. Nouns: `account`, `sync`, `mailbox`, `email`, `attachment`,
`theme`, `shortcuts`. Exit codes: 0 ok, 2 usage, 3 not configured, 4 auth
rejected, 5 server/transport, 6 not found, 7 sync lock held, 1 other.

```sh
posto email list --mailbox Inbox --unread --jsonl | jq -r .subject
posto email show <id> --text
posto email archive <id>...
posto email reply <id> --body-file reply.md --markdown
```

## UI test mode

`POSTO_UI_TEST=1 posto-gui` runs on an in-memory store seeded with
fixture mail and never touches the network or your config. Add
`POSTO_EXIT_AFTER_MS=4000` to exit after first render (used by the smoke
test).

## Window decorations

`[ui] decorations` in `config.toml` is `"auto"` (default), `"on"`, or
`"off"`. Auto keeps the native title bar on macOS and Windows; on Linux
it turns decorations off when the session looks like a tiling window
manager (Hyprland, sway, i3, river, niri and friends, detected from
`XDG_CURRENT_DESKTOP` or their sockets) and keeps them on for desktops
like GNOME, KDE, and COSMIC.

## Theming

The GUI uses `color-scheme: light dark`, `light-dark()`, system colours
and `system-ui`. On Omarchy it reads and watches
`~/.local/state/omarchy/current/theme/colors.toml` and pushes the palette
as CSS variables; elsewhere it falls back to the system scheme.

## Not in this proof of concept

IMAP, multiple accounts, contacts, calendar, conversation view, push,
offline action queue, outgoing attachments, HTML composer, keyring
storage, packaging.

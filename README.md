# Posto

A bespoke personal email client. Proof of concept: one or more Fastmail
accounts over JMAP, with a Dioxus desktop GUI and a scriptable CLI over
the same Rust core.

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

   The GUI Settings window does the same thing. A fresh install has one
   account, `default`.
3. Start `posto-gui`. It syncs every 5 minutes (configurable, 0 = off)
   and on `g`.

Data lives in `$POSTO_DATA_DIR`, default `~/.local/share/posto`:
`config.toml`, `secrets.toml` (mode 0600; `POSTO_TOKEN` in the
environment overrides the token of the default account, the first
enabled one), `posto.db`, `sync.lock`. The CLI and GUI share the
database; sync is single-writer through the lock file.

## Accounts

Each account is an `[[accounts]]` entry in `config.toml`; its token lives
under `[tokens]` in `secrets.toml`, keyed by the account id:

```toml
# config.toml
[[accounts]]
id = "fastmail"            # a-z, 0-9, '-'; unique; used by --account
name = "Fastmail"          # display name (optional)
email = "me@fastmail.com"  # default sender address
session_url = "https://api.fastmail.com/jmap/session"
enabled = true

[[accounts]]
id = "work"
email = "me@work.example"
```

```toml
# secrets.toml
[tokens]
fastmail = "..."
work = "..."
```

Files from the single-account version (`[account]`, top-level `token`)
load as the account `default` and are rewritten in the new layout on the
next write. The cache database is rebuilt when its schema version is
older than the program's (the next sync refetches mail); config and
secrets are never discarded.

With several enabled accounts the GUI sidebar gets one group per account
and an "All inboxes" entry that merges them; Settings, General manages
the accounts. In the CLI, `--account <id>` picks an account. Reads
(`email list`, `email search`, `mailbox list`) cover every account
without it, and rows carry `account_id`. An email id given without
`--account` is looked up in every account and is an error when more than
one has it. `posto account list|add|remove|set|set-token|show|identities`
manage the accounts; `posto sync run` syncs each enabled account and
reports each outcome (a failing account does not stop the others).

## Classification

Posto can sort new inbox mail with Jev (TypeSafe
System One). For each new message Jev picks a tray: needs reply, updates,
promotional, social or spam. Posto then adds the message to the tray's
mailbox (created at the top level when missing), takes it out of the inbox
unless the tray is kept there, and flags urgent replies. Spam goes to the
junk mailbox. When Jev's two best trays are too close to call, the message
stays in the inbox and also goes into the `Unsure` mailbox. Mail is queued
at sync time and classified afterwards; the first sync of an account
queues nothing.

```toml
# config.toml
[classify]
enabled = false            # global switch; an account's `classify` overrides it
model = "jev-latest"
unsure_margin = 0.15       # top-two gap below which an answer is unsure
flag_urgency_at = 4        # needs_reply with urgency >= this gets $flagged
max_per_run = 50
body_chars = 2000          # body text sent to Jev
keep_in_inbox = ["needs_reply", "updates"]
unsure_mailbox = "Unsure"

[classify.trays]           # tray -> mailbox name; spam always goes to junk
needs_reply = "Needs reply"
updates = "Updates"
promotional = "Promos"
social = "Social"

[[accounts]]
id = "fastmail"
classify = true            # per account: true, false, or absent = follow [classify].enabled
```

The TypeSafe API key lives in `secrets.toml` (`typesafe_key`; mode 0600).
`TYPESAFE_API_KEY` in the environment overrides it. Set it in
Settings > Classification or with
`printf '%s' "$KEY" | posto classify key --key-stdin`.

**GUI.** After each sync pass the app classifies the queue when at least
one account has classification on and a key exists; a toast reports
`Classified N (M unsure)`. Settings > Classification holds the switch, the
key, the model, the thresholds, the tray mailbox names, keep-in-inbox and
the per-account opt-in (click cycles follow global, on, off). The message
list shows a muted chip with the tray's mailbox name and an accent
`Unsure` chip. The sidebar's `Unsure` entry lists an account's unsure,
uncorrected messages. `t` (or "Classify as…" in the reader and the bulk
panel) files the selected messages under a tray by your decision; it moves
them by the same rules and marks them corrected.

**CLI.**

```sh
posto classify run                     # sync, then classify the queue
posto classify run --backfill 100      # queue the newest 100 unclassified inbox messages, classify
posto classify run --limit 20 --retry-failed
posto classify run --dry-run --backfill 5   # show the state blocks; changes and sends nothing
posto classify status                  # per account: classified, unsure, queued, failed, per tray
posto classify show <id>
posto classify correct <id> updates    # needs_reply updates promotional social spam
```

`classify run` needs the key (exit 3 without it) and a switched-on
account. A message that fails three times stays in the queue as `failed`
until `--retry-failed`. The classifier holds `classify.lock`, so the app
and the CLI never classify the same message twice. A correction made
without a token changes the local copy and reaches the server with the
next `classify run` or classifying sync.

## Keyboard shortcuts (defaults)

`j`/`k` next/previous, `Enter` open, `Esc` close, `Backspace` back,
`e` archive, `#` delete, `!` junk, `r` reply, `a` reply all, `c` compose,
`u` toggle read, `s` toggle flag, `m` move, `l` labels, `t` classify as, `/` search, `g` sync,
`Ctrl+Enter` send. Delete moves a message to Trash; deleting a message that is
already in Trash deletes it permanently, after a confirmation. Labels are mailboxes;
a message can be in several. Override any of
them in Settings or in `config.toml`:

```toml
[shortcuts]
archive = "y"
delete = "ctrl+d"
```

## CLI

`posto <noun> <verb>`; every command's `--help` ends with the JSON shape
it prints. Nouns: `account`, `sync`, `mailbox`, `email`, `attachment`,
`classify`, `theme`, `shortcuts`. `--account <id>` is global. Exit codes: 0 ok, 2 usage, 3 not configured, 4 auth
rejected, 5 server/transport, 6 not found, 7 sync lock held, 1 other.

```sh
posto account add --id work --email me@work.example
printf '%s' "$TOKEN" | posto account set-token --account work --token-stdin
posto email list --mailbox Inbox --unread --jsonl | jq -r .subject
posto email show <id> --text
posto email archive <id>...
posto email label Promos <id>...
posto email unlabel Promos <id>...
posto email destroy <id>... --yes   # permanent; `email delete` only moves to Trash
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

IMAP, moving mail between accounts, contacts, calendar, conversation view, push,
offline action queue, outgoing attachments, HTML composer, keyring
storage, packaging.

# Posto: notes for agents

Read `README.md` first. This file holds what the code does not say.

## Shape

- Rust workspace, edition 2024. `posto-core` is the only place for logic;
  `posto-cli` and `posto-gui` are thin views over it. A feature that needs new
  behaviour goes into the core with a test, then gets wired into both front
  ends.
- The public core API is documented in
  `.overseer/runs/2026-10-12-posto-poc/PLAN.md` (gitignored ledger of the build
  run, present on the original machine). The doc comments in
  `crates/posto-core/src/*.rs` are the live contract.
- Two back ends behind one seam: `provider::Connection` is the only type the
  rest of the core talks to; `jmap.rs` (RFC 8620/8621 + submission) and
  `proton.rs` (`proton-mail-api`, pinned by git tag) implement its
  operations and never leak past it. A new provider is a new arm plus a
  module; sync/actions/body/compose/classify are written once. No IMAP.
- Features are the lowest common denominator across providers. The binding
  model mapping (roles, hidden Proton system ids, keywords, folder vs label
  semantics, what is `Error::Unsupported`) is in
  `.overseer/runs/2026-10-15-proton/PLAN.md` (gitignored ledger) and
  summarised in README "Providers".
- Multi-account: every store row, sync cursor and action is scoped by the local
  account slug (`account_id`, from `config.toml`; not the server's account id).
  The slug is the first parameter of every `Store` method that touches account
  data (`list_emails` and `search` take `Option<&str>`; `None` is the unified
  view), of `actions::apply`, `body::fetch_body` and `compose::send`. A message
  is identified outside the store by `MsgRef { account_id, id }`
  (`<account>/<id>` as a string): email ids repeat across accounts, so never
  key a list, selection or cache on a bare email id. Sync cursors live under
  `<account>/<key>` in `sync_state` (`Store::account_state`); only
  `schema_version` is global.
- Merged views: `store::mailbox_groups` groups mailboxes across accounts
  (key `role:<role>` for the six roles, else `name:<lower-cased name>`;
  nesting ignored) and `Store::list_emails_in` lists a set of
  `(account, mailbox)` members as one page. The GUI's `Folder::Merged { key }`
  resolves its members from `MAILBOXES` at query time. A group action targets
  each account's *first* member, "has the label" means any member, removal
  removes every member of that account; accounts without a member are
  reported, never auto-created (`action_toast`). A reply records
  `Draft.reply_to_account_id`; `compose::send` marks the original answered
  only in that account, and the GUI's `finish_send` does it through the
  original account's connection when the sender differs. The Unsure row is one
  view over every account. A group row has no disclosure control on purpose:
  a double click toggles its members (`EXPANDED`), a click selects the merged
  view. Right-clicking a Trash or Spam row (group or single) offers
  "Empty …", which runs `actions::empty_mailbox` per account (destroys what
  the store lists there; `Confirm.on_accept` is an `Accept`, not only a key
  action).
- GUI shell follows the `dioxus-desktop-apps` skill: search and the panel
  toggle in the sidebar's 44px head, Settings in its footer and shown in the
  main pane (not a modal), `Ctrl+B` (`Action::ToggleSidebar`), the sidebar
  folds below sidebar + list + 360px. GUI state is still `GlobalSignal`
  statics, which the skill lists as the rejected option.

## Rules that bit us

- The database is a cache with a schema version (`schema_version` in
  `sync_state`, now 2). `Store::open` drops and recreates every cache table,
  cursor and identity when the version is missing or older, and refuses a newer
  one. A table or column change means bumping `SCHEMA_VERSION`; never write a
  data migration for the cache. Config and secrets are the only files that
  migrate (`Config::load` converts the legacy `[account]`; the first `Secrets`
  write converts the legacy `token`).
- `Secrets` setters read-modify-write the whole file (tokens and the TypeSafe
  key share it). Read tokens only through `Secrets::token_for(cfg, id)`: it
  applies `POSTO_TOKEN` to the default (first enabled) account only.
- `Store::apply_delta_page` is the only way a delta page lands: rows, deletes,
  queue rows and the `email_state` cursor in one transaction.

- The sync writes `<account>/provider` into `sync_state`; `actions::apply`
  and `classify::ensure_mailbox` read it to apply the Proton folder rules
  with or without a connection (move keeps labels, move-to-label refused,
  a folder added through a patch evicts the other folders,
  tray mailboxes must be labels). Without the state the JMAP rules apply.
- Proton tokens rotate on 401. Every `proton::Session` operation (success
  and error paths, including `create_mailbox`/`destroy` which make no
  request) ends in `persist_if_changed`; `last_blob` advances only when the
  callback returned `Ok`. `Secrets::set_proton_session` writes only while the
  stored `generation` matches the one the connection was opened with
  (`set_proton` issues a fresh one, never reused, high-water kept across
  `clear_account`), and never writes a non-object blob.
- Proton refresh tokens are single-use, so one account gets one live
  `Session`: the GUI's `state::connect_with` connects one account at a time
  and hands waiters the cached connection; keep it that way. A CLI command
  run while the GUI is open still holds its own copy, so keep such runs short.
- Error 9101 ("Access token does not have sufficient scope") on a resumed
  session came from `GET /core/v4/keys/salts`, which Proton allows only
  briefly after login. `proton-mail-api` v0.4.1 unlocks from the blob's
  stored `key_password` and skips the salts; never pin a version below it.
  `map_err` turns a remaining 9101 into `Error::Auth` (stored key password
  stale, e.g. after a password change: sign in again).
- `proton::map_err` uses the crate error's `Display` only. Never append
  `server_message()` (unrestricted server text) and never `{:?}`-format the
  crate error into anything a user, a log or a test failure can see.
- Never put the bearer token in an error, log, or debug output. The client
  redacts it from server-controlled text; keep it that way and never format
  deserialisation errors that can embed response bodies.
- Email HTML reaches the GUI only through `html::sanitize` and a sandboxed
  `iframe` `srcdoc`. Never `dangerous_inner_html` mail content. With
  `allow_remote == false` every resource that is not `cid:` or `data:image/` is
  dropped and `had_remote` is set.
- Tests never reach the network. A Proton test builds a `Session` against
  wiremock (`Session::for_tests`, resumed blob) and generates its own PGP
  keys; CLI tests validate `account login` inputs with an account that is
  not Proton, an empty password or an empty app version. One mutation run
  that inverted the provider guard sent a real login request (run
  2026-10-15-proton); do not weaken a login guard while running CLI tests.
- `std::env::set_var` only in a synchronous `main` before any runtime exists.
  Tests use the `_at` / `_with_lock` / `open_memory` entry points and
  `Command::env`, never in-process env mutation (except `config::tests`, which
  serialises behind a mutex).
- Sync state: `email_state` is captured before initial paging (so arrivals
  during paging are picked up by the next delta); every `Email/changes` page
  persists its own `newState`; a `cannotCalculateChanges` error clears the
  local email cache.
- `compose::send` matches JMAP responses by method name and call id and
  inspects the implicit `Email/set` response of the submission; a
  sent-but-not-moved-to-Sent result is `Error::Other` so callers never resend.
- The theme watcher's notify callback only sends on a channel; a worker thread
  owns the watcher and does the re-watching (calling `watch`/`unwatch` inside
  the callback deadlocks).
- Dioxus 0.7: `iframe` has no typed `sandbox` attribute (use `"sandbox": ""`);
  `document::Style` inserts once, so dynamic CSS goes in a plain `style {}`
  element; create signals in the scope that uses them or the runtime logs
  `copy_value_hoisted` warnings.

## Checks before claiming done

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
POSTO_UI_TEST=1 POSTO_EXIT_AFTER_MS=4000 cargo run -p posto-gui   # exit 0, no panic
```

The GUI needs a display; on this machine use
`WAYLAND_DISPLAY=wayland-1 XDG_RUNTIME_DIR=/run/user/$(id -u)`.

## graphify

This project has a knowledge graph at graphify-out/ with god nodes, community
structure, and cross-file relationships.

When the user types `/graphify`, use the installed graphify skill or
instructions before doing anything else.

Rules:
- For codebase questions, first run `graphify query "<question>"` when
  graphify-out/graph.json exists. Use `graphify path "<A>" "<B>"` for
  relationships and `graphify explain "<concept>"` for focused concepts. These
  return a scoped subgraph, usually much smaller than GRAPH_REPORT.md or raw
  grep output.
- Dirty graphify-out/ files are expected after hooks or incremental updates;
  dirty graph files are not a reason to skip graphify. Only skip graphify if
  the task is about stale or incorrect graph output, or the user explicitly
  says not to use it.
- If graphify-out/wiki/index.md exists, use it for broad navigation instead of
  raw source browsing.
- Read graphify-out/GRAPH_REPORT.md only for broad architecture review or when
  query/path/explain do not surface enough context.
- After modifying code, run `graphify update .` to keep the graph current
  (AST-only, no API cost).

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
- Three back ends behind one seam: `provider::Connection` is the only type the
  rest of the core talks to; `jmap.rs` (RFC 8620/8621 + submission),
  `proton.rs` (`proton-mail-api`, pinned by git tag) and `gmail.rs` (Gmail
  REST API v1, OAuth desktop loopback flow) implement its operations and
  never leak past it. A new provider is a new arm plus a
  module; sync/actions/body/compose/classify are written once. No IMAP.
- Features are the lowest common denominator across providers. The binding
  model mapping (roles, hidden Proton system ids, keywords, folder vs label
  semantics, what is `Error::Unsupported`) is in
  `.overseer/runs/2026-10-15-proton/PLAN.md` and, for Gmail,
  `.overseer/runs/2026-10-20-gmail/PLAN.md` (gitignored ledgers), and is
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

- Classifier trays are config, not code: `classify::Tray` is a key of
  `[classify.trays]` or `spam`, validated with `Tray::parse_in`; Jev's
  `questions` are built per client from `jev::tray_criteria` (config
  `[classify.criteria]`, else `jev::BUILTIN_CRITERIA`). Beyond jevmail's
  questions Posto asks `action_required`: at `ACTION_AT` an uncorrected
  answer stays in the inbox and is flagged when urgent, whatever its tray.
  `set_correction` feeds sender memory (`Store::remembered_tray`, two
  corrections in a row); a remembered answer has model `sender` and costs no
  request. The integration tests pin the pre-2026-10 trays
  (`legacy_trays`) on purpose: old configs must keep working.

- Triage is a store query, not state: `Store::triage_email_ids` /
  `list_triage` (one SQL rule: inbox, not trash/junk/draft, and flagged or
  classifier-owed and not answered; answered = `$answered` or a later
  same-thread email in that account's Sent, thread id non-empty). The GUI's
  `Folder::Triage` and the `TRIAGE` count are recomputed wherever the
  Unsure counts are, plus after a reply without archive (`send_draft` ends
  in `settle_selection`). The list title shows the total, not the loaded
  page. The CLI's `--triage` covers enabled accounts unless `--account`
  names one.
- Masked emails (Fastmail) are senders, not identities: `MaskedEmail/get`
  goes out only when the session advertises
  `https://www.fastmail.com/dev/maskedemail`, and only that call adds it to
  `using` (an unknown capability fails the whole request). They are cached
  like identities (`<account>/masked_emails` in `sync_state`, no table).
  `compose::send` sends a masked From as the account's own identity (else
  the first) with an empty display name. An address that is also an identity
  of the account is the identity, so the GUI leaves it out of the masked
  group. From resolution searches the draft's account first.
- Reply-To lives on `EmailBody.reply_to`, not on the summary. Proton list
  and event metadata never carry `ReplyTos` (only the full message does), so
  `compose::reply` reads it from the fetched body. Rule: own mail goes to the
  original To; otherwise a non-empty Reply-To wins (mine removed, but never
  replaced by From, even when every address in it is mine), else From. When
  Reply-To applied, reply-all also drops every From address from Cc, so a
  Proton Pass alias reply cannot copy the real sender. Proton Pass itself
  (alias lists, sending new mail from an alias) is out of reach: the session
  has no `pass` scope, and the Pass API answers 403 with code 9108.

- Gmail: `ARCHIVE` is Posto's own mailbox id (no Gmail label has it) and is
  never sent to Gmail. `gmail::visible` is the one membership rule: Spam or
  Trash hide Inbox/Drafts/Sent, and `ARCHIVE` is present exactly when none
  of the five folder-like built-ins is. Gmail follows the Proton folder
  rules in `actions::apply` (state `provider = gmail`), plus: changes to
  `SENT`/`DRAFT` are refused, the mirror keeps an email's `SENT`/`DRAFT`
  and then goes through `visible`, and the empty-patch guard looks at that
  result (removing a sole Inbox archives). A Gmail tray must be a role-less
  label (`ensure_mailbox`). Chats (`CHAT`) are never stored. Attachment
  `blob_id` is the index in mail-parser's `attachments()` (Gmail's own
  attachment ids change between fetches). The OAuth client is
  `<data_dir>/google-client.json`; each account keeps only a refresh token
  under `[gmail]`; `POSTO_TOKEN` never applies to Gmail.

## Rules that bit us

- The database is a cache with a schema version (`schema_version` in
  `sync_state`, now 7). `Store::open` drops and recreates every cache table,
  cursor and identity when the version is missing or older, and refuses a newer
  one. A table or column change means bumping `SCHEMA_VERSION`; never write a
  data migration for the cache. The one exception is `sender_trays` (sender
  memory, learned from corrections): it is not in `CACHE_TABLES`, survives a
  rebuild, and so its layout must never change. Config and secrets are the only files that
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
- Gmail errors carry the HTTP status number only, never Google's error text
  (`error`, `error.status`, `message` can echo request data). The request
  helper decides on the status before reading a body: a 401/404/429/5xx
  body is never read. A 401 forgets the cached token only if it is the one
  the request used, then replays once. `messages.send` is never retried on
  a transient error, and a malformed 2xx or a non-connect transport error
  there is the "may have been sent; check Sent" `Error::Other`.
- Every `Connection` future must be `Send`: the GUI spawns `sync_all` on
  tokio. Borrowing items across `buffered()` in `gmail::Session::summaries`
  broke that once; `tests/gmail_core.rs::connection_futures_are_send`
  guards it.
- The Gmail Message-ID is set by Posto (`<random hex@sender domain>`) and
  `mail-builder` is built without default features: its default puts the
  machine's hostname into every Message-ID.
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
  not Proton, an empty password or an empty app version. A Gmail test never
  holds both a refresh token and a `google-client.json` in one sandbox (that
  pair reaches Google); the loopback sign-in is tested only through
  `gmail::login_with` against wiremock. One mutation run
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
- Mail links target the top frame (`html::sanitize` sets `target="_top"`,
  the reader iframes allow `allow-top-navigation-by-user-activation`). The
  webview's navigation handler (`main.rs`) then opens http(s)/mailto in the
  browser and refuses every other top navigation except `about:`. A
  `_blank` link inside the sandbox is silently dropped: Dioxus has no
  new-window hook.
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

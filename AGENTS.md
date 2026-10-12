# Posto: notes for agents

Read `README.md` first. This file holds what the code does not say.

## Shape

- Rust workspace, edition 2024. `posto-core` is the only place for
  logic; `posto-cli` and `posto-gui` are thin views over it. A feature
  that needs new behaviour goes into the core with a test, then gets
  wired into both front ends.
- The public core API is documented in
  `.overseer/runs/2026-10-12-posto-poc/PLAN.md` (gitignored ledger of
  the build run, present on the original machine). The doc comments in
  `crates/posto-core/src/*.rs` are the live contract.
- JMAP only (RFC 8620/8621 + submission). No IMAP.
- Multi-account: every store row, sync cursor and action is scoped by the
  local account slug (`account_id`, from `config.toml`; not the server's
  account id). The slug is the first parameter of every `Store` method
  that touches account data (`list_emails` and `search` take
  `Option<&str>`; `None` is the unified view), of `actions::apply`,
  `body::fetch_body` and `compose::send`. A message is identified outside
  the store by `MsgRef { account_id, id }` (`<account>/<id>` as a string):
  email ids repeat across accounts, so never key a list, selection or
  cache on a bare email id. Sync cursors live under `<account>/<key>` in
  `sync_state` (`Store::account_state`); only `schema_version` is global.

## Rules that bit us

- The database is a cache with a schema version (`schema_version` in
  `sync_state`, now 2). `Store::open` drops and recreates every cache
  table, cursor and identity when the version is missing or older, and
  refuses a newer one. A table or column change means bumping
  `SCHEMA_VERSION`; never write a data migration for the cache. Config and
  secrets are the only files that migrate (`Config::load` converts the
  legacy `[account]`; the first `Secrets` write converts the legacy
  `token`).
- `Secrets` setters read-modify-write the whole file (tokens and the
  TypeSafe key share it). Read tokens only through
  `Secrets::token_for(cfg, id)`: it applies `POSTO_TOKEN` to the default
  (first enabled) account only.
- `Store::apply_delta_page` is the only way a delta page lands: rows,
  deletes, queue rows and the `email_state` cursor in one transaction.

- Never put the bearer token in an error, log, or debug output. The
  client redacts it from server-controlled text; keep it that way and
  never format deserialisation errors that can embed response bodies.
- Email HTML reaches the GUI only through `html::sanitize` and a
  sandboxed `iframe` `srcdoc`. Never `dangerous_inner_html` mail content.
  With `allow_remote == false` every resource that is not `cid:` or
  `data:image/` is dropped and `had_remote` is set.
- `std::env::set_var` only in a synchronous `main` before any runtime
  exists. Tests use the `_at` / `_with_lock` / `open_memory` entry
  points and `Command::env`, never in-process env mutation (except
  `config::tests`, which serialises behind a mutex).
- Sync state: `email_state` is captured before initial paging (so
  arrivals during paging are picked up by the next delta); every
  `Email/changes` page persists its own `newState`; a
  `cannotCalculateChanges` error clears the local email cache.
- `compose::send` matches JMAP responses by method name and call id and
  inspects the implicit `Email/set` response of the submission; a
  sent-but-not-moved-to-Sent result is `Error::Other` so callers never
  resend.
- The theme watcher's notify callback only sends on a channel; a worker
  thread owns the watcher and does the re-watching (calling
  `watch`/`unwatch` inside the callback deadlocks).
- Dioxus 0.7: `iframe` has no typed `sandbox` attribute (use
  `"sandbox": ""`); `document::Style` inserts once, so dynamic CSS goes
  in a plain `style {}` element; create signals in the scope that uses
  them or the runtime logs `copy_value_hoisted` warnings.

## Checks before claiming done

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
POSTO_UI_TEST=1 POSTO_EXIT_AFTER_MS=4000 cargo run -p posto-gui   # exit 0, no panic
```

The GUI needs a display; on this machine use
`WAYLAND_DISPLAY=wayland-1 XDG_RUNTIME_DIR=/run/user/$(id -u)`.

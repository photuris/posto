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

## Rules that bit us

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

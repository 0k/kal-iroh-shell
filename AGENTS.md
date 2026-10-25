# kal-iroh-shell

Thin app shells whose interface is published by the server and
hot-reloaded, state saved, on every device.  See `README.org`.

## Layout

- `crates/kal-iroh-shell-protocol/` — wire formats only (paths, header
  names, JSON shapes).  MUST NOT depend on axum, tokio, reqwest or any
  HTTP/async crate: every party depends on it.
- `crates/kal-iroh-shell-server/` — release server library + the
  `kal-iroh-shell-releases` binary.
- `crates/kal-iroh-shell/` — client shell, embedded in an app's local
  server through the `Host` trait.  Must compile for Android (no
  `notify`, no desktop-only crates in its non-dev dependencies).
- `js/` — npm `@0k/kal-iroh-shell`: page kit (`src/*.ts`, source-only,
  compiled by the consumer) and Vite plugin (`src/vite.mjs`, plain JS
  because Node loads it from `node_modules`).

## Rules

- **A wire name exists once per language.**  Rust: the protocol crate.
  TypeScript: `js/src/protocol.ts`.  `js/test/protocol.test.ts` fails
  when they disagree; never silence it by editing only one side.
- **Every SSE endpoint goes through `kal-sse`** (REST snapshot with
  `x-sub-id` + `x-snapshot-index`, stream with `?sub=`).  Never a raw
  `Sse::new` over a broadcast channel: it loses events on reconnect.
- **The control stream is only "which release is current"**, separate
  from any data stream of the app.  The server only relays a publish
  (pointer switch); it never decides to update a device.
- **A release directory is immutable once published**; publishing
  completes the copy under a temporary name, renames it, then switches
  the pointer by a rename.
- **The page always keeps the shell's local origin.**  The shell must
  never hand the page an upstream (tunnel) URL: that port changes on
  every reconnect.
- **The reload never waits on a dead page**: no answer within
  `ANSWER_TIMEOUT`, or a busy lease not renewed within `BUSY_LEASE`,
  forces the reload.  Do not lengthen these to hide a page bug.
- The app-specific part (what to save, what means busy) stays in the
  app; this repository holds no knowledge of any particular app.

## Build / test

- `cargo fmt && cargo clippy --all-targets && cargo test`
- `cargo check -p kal-iroh-shell --features tauri`
- `cd js && npm run check`

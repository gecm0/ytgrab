# ytgrab

Rust desktop app (`app/`, gpui + tiny_http, all in `app/src/main.rs`) plus a Chrome MV3 extension (`extension/`). See README.md for layout and behavior.

## Commands

- Rust tests: `cd app && cargo test` (first build is slow because of gpui)
- Extension test: `bun extension/cookies.test.js`
- Format: `cd app && cargo fmt` (config in `app/rustfmt.toml`; a hook runs rustfmt after every `.rs` edit)

## Gotchas

- The `"key"` in `extension/manifest.json` pins the extension ID. `EXTENSION_ORIGIN` in `app/src/main.rs` must match that ID, or the app rejects every request. Change both together.
- The app's only auth is the `Origin` check on `127.0.0.1:47321`. Anything that widens what the server can do needs real auth.
- Cookies from a logged-in account are written to a temp file per job and must always be deleted afterwards.
- macOS builds use gpui's `runtime_shaders` feature (no Metal toolchain needed). Windows builds need the MSVC toolchain.
- Use bun, never npm or yarn.

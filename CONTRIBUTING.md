# Contributing to Aperture

Thanks for taking a look. Aperture is a small Rust shell (`tao` + `wry`) over the system WebView2
runtime. Most of the app lives in `src/main.rs` (the event loop, tab lifecycle, layout, and IPC
router), with focused modules alongside it: `ai.rs` (Ollama client), `search.rs` (web-search fetch +
parse), `bitwarden.rs` (the `bw` CLI), `webauthn.rs` (in-app passkey provider), `blocker.rs` (adblock),
and `store.rs` (settings + persisted data). The UI is plain HTML/CSS/JS in `src/ui/` and is compiled
into the binary with `include_str!`.

## Building

See the README for the full toolchain setup. In short: Rust **GNU** toolchain (a `rust-toolchain.toml`
pins it; no MSVC needed), MinGW-w64's `dlltool.exe` + `windres.exe` on `PATH`, and the WebView2
Evergreen Runtime installed. Then `cargo run`.

Because the UI is `include_str!`'d, **any change to a file in `src/ui/` requires a rebuild** to take
effect; there is no hot reload.

## Hard invariants (please don't break these)

These are easy to violate by accident and a few of them will fail in subtle, hard-to-debug ways rather
than at compile time.

1. **WebView2 COM versions must unify with wry's.** wry hands us raw COM objects (`ICoreWebView2`); if
   our `webview2-com`/`windows`/`windows-core` resolve to a *different* version than the one wry uses
   internally, those objects are a different Rust type and every `.cast::<...>()` fails at runtime. The
   `Cargo.toml` requirements are chosen so Cargo unifies them with wry's transitive versions to a single
   version each (see the comment next to them). After any `wry` bump, re-check with
   `cargo tree -i webview2-com` (and `-i windows-core`): there must be exactly one version of each.

2. **The security boundary.** Privileged IPC (the `with_ipc_handler` that can drive the app) lives
   **only** in the chrome webview. Content webviews are treated as hostile: minimal injected scripts,
   no host powers. There are a few deliberately narrow, non-privileged exceptions (a captured-login
   "save candidate", the passkey `webauthn_get` hint, an AI-selection hint) that cannot drive
   navigation or app state. Do not add new powers to content-webview IPC, and validate any
   externally-influenced URL before navigating or persisting it (`url_is_safe`).

3. **Honesty in user-facing copy.** Privacy here means tracker/ad blocking + a local-only profile +
   DoH + third-party storage partitioning. It is **not** anti-fingerprinting, and the profile is
   persistent by design (it's a daily driver). Say "minimized telemetry", not "zero". Don't claim a
   capability the code doesn't have.

4. **No filler in UI copy.** Ship only functional text: button/menu labels, input placeholders, and
   concise instructions for non-obvious controls. No taglines, no reassurances, no sub-captions that
   just restate a label. Everywhere (user-facing copy, documentation, and code comments), avoid em
   dashes and "it's not X, it's Y" phrasing.

5. **TrySuspend ordering.** Always `set_visible(false)` (through wry, never the raw controller) on a
   tab *before* `TrySuspend`, or WebView2 throws `ERROR_INVALID_STATE`. Never mix
   `MemoryUsageTargetLevel` with `TrySuspend` on one webview.

6. **One shared profile per running instance.** The chrome and every normal tab share one persistent
   `WebContext` under the active profile's data dir (`%APPDATA%/RustBrowser` for Default,
   `%APPDATA%/RustBrowser/profiles/<name>` otherwise) so logins survive restarts. Private/temporary
   tabs use a separate ephemeral context that is wiped each launch. All persisted state must go
   through `store::data_dir()` (never `root_dir()`, which is only for the cross-profile registry),
   or it will leak between profiles. Don't revert to wry's default profile location (it puts the
   profile next to the exe and logs the user out whenever the exe moves).

## Style

Match the surrounding code. Prefer small, reversible changes over large rewrites. `main.rs` is large;
if you're adding a self-contained subsystem, a new module is welcome, but please don't bundle a
big-bang refactor into a feature PR.

## Pull requests

- Describe what changed and why, and how you tested it on Windows.
- Keep the diff focused on one thing.
- If you touched anything in the hard-invariants list above, call that out explicitly so it gets a
  careful review.

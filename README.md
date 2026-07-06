# Aperture

A lightweight, private daily-driver web browser for Windows, built as a small Rust
shell (`tao` + `wry`) over the system **WebView2** (Edge/Chromium) engine. Same engine
your sites already work on, far less of the RAM.

Aperture is **not** a new browser engine. It's a thin, fast frontend that leans on
WebView2 for rendering and adds the things that make a browser pleasant to live in:
aggressive idle-tab memory reclaim, ad/tracker blocking, a clean monospace UI, an
optional page-aware AI assistant that runs entirely on your own machine, Bitwarden
autofill, and real browser-extension support, with no telemetry and a local-only
profile.

> Status: early but usable as a daily driver. Single-window, tabbed.

## Why

Chrome is heavy. In one informal measurement on a single machine with the same five
real sites open, a default Chrome profile used roughly **2.5× the RAM** of Aperture,
and Aperture's idle-tab reclaim cut its own footprint **roughly in half** once tabs
went idle. Treat the cross-browser figure as a ballpark from one run, not a benchmark:

| | RAM (same 5 tabs) | processes |
|---|---|---|
| Aperture, all tabs awake | ~1.5 GB | 19 |
| Aperture, idle tabs discarded | **~0.75 GB** | 8 |
| Chrome, fresh profile, all awake | ~4.0 GB | 64 |

Two things drive the gap: Aperture **blocks ads/trackers** (fewer subframe processes),
and it **discards idle tabs** (kills their renderer processes to reclaim memory, then
reloads them when you return). The ~50% self-drop from discarding is the cleanest, most
reproducible number. Absolute figures vary by sites and run. If your Chrome runs an ad
blocker the all-awake gap shrinks, but the idle-discard win is independent.

## Features

- **Idle-tab memory reclaim.** Background tabs dim, then sleep, then are discarded
  (renderer killed) after they stay idle. Pin a tab to keep it awake forever.
- **Browsing trail.** The new-tab page draws a force-directed map of your recent
  browsing: pages are nodes, the links you actually followed are edges, including which
  tab opened which. Pick the time window, drag and zoom the graph, click a node to go
  back. Recording is local-only and fully controllable in Settings (retention window,
  per-site exclusions, one-click wipe). Private tabs are never recorded.
- **Tab auto-archive.** Tabs you have not touched for a couple of weeks (configurable)
  quietly close into a searchable archive under the History viewer, so the strip never
  accumulates hundred-tab guilt. Pinned tabs are exempt. One click restores any of them.
- **Workspaces.** Named, color-coded tab contexts (work, personal, a project). The strip
  shows one workspace at a time; the others' tabs stay open, hidden, and keep sleeping.
  The command palette searches across all of them.
- **Link routing.** Rules like "github.com opens in Work": links that spawn a new tab
  are routed into the right workspace automatically. Right-click a tab for a one-click
  "always open this site here" rule.
- **Profiles.** Chrome-style separate identities, each with its own logins, cookies,
  history, settings, and extensions. Profiles run side by side in separate windows,
  `--profile <name>` opens one directly, and an optional picker asks at launch.
- **Default-browser ready.** Register Aperture from Settings, confirm in Windows
  Settings, and links from other apps open in the running window.
- **Private, page-aware AI assistant (optional).** A docked sidebar (`Ctrl+J`) that
  answers questions about the current page, runs inline selection and address-bar
  actions, can browse the web for you (Aperture fetches and reads real pages itself),
  reads a screenshot when a page resists text extraction, and organizes your tabs. It
  talks to a local [Ollama](https://ollama.com) instance, so prompts stay on your
  machine unless you explicitly run a web search. Pick any installed model from a
  dropdown. The whole layer can be turned off in Settings. Frees the model from VRAM
  when the panel closes.
- **Ad / tracker blocking.** Brave's `adblock` engine against a filter list.
- **Browser extensions.** Loads unpacked extensions from your profile, including the
  real Bitwarden extension. Passkeys (`navigator.credentials.get`) are served in-app by
  Aperture's own WebAuthn provider (ES256, reads the passkey from your Bitwarden vault).
- **Bitwarden autofill + save.** Fill a login from your vault, or save a new one to it,
  via the Bitwarden CLI (`bw`). HTTPS-only, explicit action, vault re-locked after each
  use. The master password is never stored.
- **Tabs your way.** Vertical (left column) or horizontal layout, split view for two
  pages side by side (`Ctrl+\`), private/temporary tabs on an isolated ephemeral profile
  (`Ctrl+Shift+N`), color-coded groups, open-in-new-tab, middle-click to close,
  `Ctrl+Shift+T` to reopen, pinning.
- **Keyboard-first.** Command palette (`Ctrl+K`), `Ctrl+T/W/L/R/D/J/H`, `Ctrl+Tab`,
  `Ctrl+F`, zoom `Ctrl +/-/0`, `F5` reload and `Ctrl+Shift+R` / `Ctrl+F5` hard reload
  (bypasses cache). Every shortcut is rebindable in Settings.
- **More daily-driver tools.** Reading list, whole-page translate (local AI), full-page
  screenshot, find-in-page, per-site zoom memory, downloads, styled site-permission
  prompts.
- **Session restore, bookmarks (with folders), history + smart omnibox autocomplete.**
- **Custom new-tab page.** A console-style landing page with an editable shortcut grid
  (icon + label + URL) and a binary clock. Theme colors are fully editable in Settings.
- **Private by default.** No app telemetry, DNS-over-HTTPS, HTTPS upgrades, third-party
  storage partitioning, a local-only profile (no cloud sync). This is tracker-blocking
  plus a local profile, **not** anti-fingerprinting.

## Build (Windows)

### Prerequisites

- **Windows 10/11 x64.**
- **Microsoft Edge WebView2 Evergreen Runtime.** Preinstalled on Windows 11. If missing,
  install the Evergreen Bootstrapper from Microsoft's WebView2 page. It is used from the
  system, not bundled.
- **Rust (GNU toolchain)** via [rustup]. Aperture is built and tested on the GNU
  toolchain specifically so it does **not** need MSVC / Visual Studio. A
  `rust-toolchain.toml` pins it, so rustup will select/install it automatically.
- **MinGW-w64** on `PATH`, providing **`dlltool.exe`** (linker) and **`windres.exe`**
  (embeds the app icon). Install via `winget install BrechtSanders.WinLibs.POSIX.MSVCRT`,
  or download from [winlibs.com], or MSYS2 (`pacman -S mingw-w64-x86_64-binutils`).
  Add that install's `mingw64\bin` to `PATH`.

### Build & run

```sh
git clone https://github.com/arthurrees/aperture
cd aperture
cargo run
```

The first clean build takes a few minutes (it compiles `wry`, `tao`, and the
`windows` crate). The build script copies `WebView2Loader.dll` next to the binary so
it runs in place. If you move the `.exe` elsewhere, keep that DLL beside it and have
`mingw64\bin` on `PATH`.

### Troubleshooting

- `error calling dlltool 'dlltool.exe': program not found` means MinGW `mingw64\bin`
  isn't on `PATH`.
- windres / icon errors are the same cause: `windres.exe` missing from `PATH`.
- WebView2 COM error / `0x80070002` at launch means the WebView2 Evergreen Runtime
  isn't installed (or isn't registered). Install it.

### Where your data lives

Profile, history, bookmarks, sessions, the browsing trail, and settings are stored
locally under `%APPDATA%\RustBrowser`, with no cloud sync. Extra profiles live under
`%APPDATA%\RustBrowser\profiles\<name>`, each fully self-contained.

## Local AI setup (optional)

The assistant talks to [Ollama](https://ollama.com) over loopback HTTP. Install Ollama,
pull any chat model (`ollama pull llama3.1`, `qwen2.5`, `mistral`, whatever you like),
then open Settings → AI and pick it from the dropdown (the list is populated from your
running Ollama). Set the host there too if Ollama isn't on the default
`http://127.0.0.1:11434`. You are not locked to any particular model. If you don't want
a local AI at all, leave the assistant disabled and every AI control disappears. The
rest of the browser works exactly the same.

## Bitwarden setup (optional)

Autofill/save use the official Bitwarden CLI. Install it (`npm i -g @bitwarden/cli`)
and log in once in a terminal (`bw login`). Aperture prompts for your master password
per action, unlocks just long enough to read/write the one item, then re-locks.

## License

Apache-2.0, see [LICENSE](LICENSE). Third-party components and their licenses
(including the MPL-2.0 `adblock` engine and the ISC Lucide icons) are listed in
[NOTICE](NOTICE).

[rustup]: https://rustup.rs
[winlibs.com]: https://winlibs.com

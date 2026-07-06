# Aperture architecture

How the pieces fit, for anyone reading or changing the code. CONTRIBUTING.md lists the hard
invariants; this file explains the shape.

## The one-paragraph version

Aperture is a single `tao` window containing several `wry` WebViews on the system WebView2
(Edge/Chromium) runtime. One WebView is the **chrome** (toolbar, tab strip, modals, all trusted UI,
one HTML file). Each tab is its own **content** WebView, treated as hostile. A Rust event loop in
`main.rs` owns all state (tabs, workspaces, groups, settings) and mediates everything through a
`UserEvent` enum. Persistence is JSON files in a per-profile data directory.

## Process and webview model

- One `CoreWebView2Environment` is shared by the chrome and all normal tabs (they share browser,
  GPU, and network processes; each tab gets its own renderer process).
- Only the active tab's WebView is visible. Background tabs are hidden and progress through the
  memory tiers below. Bounds for every webview are recomputed from the window size, the tab-strip
  orientation, and the AI-panel width (`content_area` and friends).
- Private tabs use a second, ephemeral `WebContext` whose folder is wiped at every launch.
- Full-window surfaces (Settings, History, the palette, pickers) are drawn inside the chrome
  webview, which temporarily grows to cover the window, then shrinks back.

## The security boundary

The chrome webview has privileged IPC (`with_ipc_handler`): its messages can drive navigation, tabs,
settings, everything. Content webviews get no host powers: only the adblock cosmetic CSS and a few
content scripts that emit narrow, non-privileged hints (captured-login save candidate, the passkey
`webauthn_get` request, an AI selection hint). The host validates anything that came from a page
before acting on it (`url_is_safe`, origin checks in `webauthn.rs`). New capabilities go on the
chrome side, not the content side.

## Event flow

Everything funnels into one `match` on `UserEvent` in the run loop:

- Chrome UI action -> `window.ipc.postMessage({cmd: ...})` -> IPC handler -> `UserEvent` ->
  run-loop arm mutates state -> `push_*` helpers re-render the chrome via `evaluate_script`.
- Page events (title, favicon, URL, audio, downloads, new-window requests) arrive through per-tab
  WebView2 handlers and become `UserEvent`s the same way.
- Worker threads (AI streaming, web search, Bitwarden CLI, the idle-tick, the open-url pipe) never
  touch state directly; they post `UserEvent`s through an `EventLoopProxy`.

The chrome UI is dumb by design: the host pushes it complete state (`setTabs`, `setGroups`,
`setWorkspaces`, `setProfiles`, ...) and it re-renders.

## Tab lifecycle (the RAM feature)

Tiers, applied by a periodic sweep (`ReclaimIdleTabs`) using per-tab idle clocks:

1. **ACTIVE** - visible, normal memory target.
2. **DIM** - hidden on tab switch, memory target dropped to Low. Scripts keep running.
3. **SLEEP** - after `idle_secs`, `TrySuspend` pauses the renderer (`set_visible(false)` must come
   first, see CONTRIBUTING #5).
4. **DISCARD** - after ~6x `idle_secs`, the WebView is dropped entirely; the renderer process
   exits. Reactivation rebuilds it and reloads the URL.
5. **ARCHIVE** - after `archive_days` of no use (wall-clock, survives restarts via
   `SessionTab.last_used`), the tab closes into `archive.json`, restorable from the History viewer.

Exempt from all tiers: the active tab and visible split pane, pinned tabs, tabs playing audio,
tabs with in-progress downloads, private tabs. Exempt tabs get their idle clocks refreshed each
sweep so exemption never turns into an instant sleep when it ends.

## Organization layers

- **Tabs** live in one `Vec<Tab>`; each has a `workspace` id and optional `group` id.
- **Groups** are colored, collapsible chips within the strip; `normalize_groups` keeps members
  contiguous.
- **Workspaces** partition the strip: the chrome renders only the active workspace's tabs, and
  tab cycling / index shortcuts / split view stay inside it. Hidden workspaces' tabs remain in
  the Vec and keep aging through the tiers. The palette searches all workspaces and switching to
  a cross-workspace tab follows it.
- **Link routing (ATC)**: destination-host suffix rules route new-tab links into a workspace.
- **Profiles** are a level above all of this: a separate data dir (and therefore cookie jar,
  stores, and extensions) per profile. One process per profile; profiles run side by side. A
  profile switch relaunches the exe with `--profile <name>` after releasing that profile's
  single-instance mutex.

## Data

All persistence is small JSON files through `store.rs`, in the active profile's data dir:
`session.json` (tabs incl. pin/group/workspace/last-used), `groups.json`, `workspaces.json`,
`bookmarks.json`, `history.json`, `trail.json` (timestamped visits with from-edges, feeds the
new-tab graph), `archive.json`, `links.json`, `zoom.json`, `settings.json`, plus the `WebView2/`
profile folder and `extensions/`. The cross-profile registry (`profiles.json`) is the only file at
the shared root. Private tabs are written to none of these.

## Single instance and external links

Each profile holds a named mutex. A second launch of the same profile forwards its command-line
URL to the running instance over the `aperture_open_url` named pipe and exits; the first instance
opens it like any page-initiated link (so link-routing rules apply). Default-browser registration
is plain HKCU registry entries written on request from Settings.

## UI files

`src/ui/chrome.html` (the whole chrome), `src/ui/home.html` (new-tab page incl. the trail graph),
`src/ui/ai.html` (assistant sidebar). They are `include_str!`'d into the binary: UI edits need a
rebuild. Theming works by the host injecting CSS-variable overrides from `settings.theme`.

## Modules

- `main.rs` - window, event loop, tab lifecycle, layout, IPC router, shortcuts. Large by design;
  add new self-contained subsystems as modules instead of growing it further.
- `store.rs` - data dir resolution (profiles), every load/save, small pure helpers.
- `blocker.rs` - Brave `adblock` engine setup + request/cosmetic filtering.
- `ai.rs` - Ollama client (streaming chat over loopback HTTP).
- `search.rs` - DuckDuckGo fetch + parse for the assistant's web-search scope.
- `bitwarden.rs` - `bw` CLI integration (fill, save, passkey read).
- `webauthn.rs` - in-app passkey provider (`navigator.credentials.get` shim + ES256 signing).

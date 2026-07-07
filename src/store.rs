//! Persistence for session (open tabs) and bookmarks, as JSON under %APPDATA%/RustBrowser.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

static PROFILE_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Root data dir shared by ALL profiles (profiles.json lives here). The Default profile keeps its
/// data directly here, so pre-profile installs keep their logins/history untouched.
pub fn root_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join("RustBrowser");
    let _ = fs::create_dir_all(&dir);
    dir
}

/// Filesystem-safe form of a profile name ("Work stuff" -> "work-stuff").
pub fn slug(name: &str) -> String {
    let s: String = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    s.trim_matches('-').to_string()
}

/// Select the active profile. Must be called once at startup, before anything loads or saves;
/// every store (and the WebView2 user-data folder) then lives under this profile's directory.
pub fn set_profile(name: &str) {
    let dir = if name == "Default" {
        root_dir()
    } else {
        root_dir().join("profiles").join(slug(name))
    };
    let _ = fs::create_dir_all(&dir);
    let _ = PROFILE_DIR.set(dir);
}

/// The ACTIVE profile's data directory (root for Default, profiles/<slug> otherwise).
pub fn data_dir() -> PathBuf {
    PROFILE_DIR.get().cloned().unwrap_or_else(root_dir)
}

/// The profile registry, shared across profiles at the root. "Default" is implicit and always
/// exists; `list` holds the extra profiles by display name.
#[derive(Serialize, Deserialize, Default)]
pub struct ProfilesFile {
    #[serde(default)]
    pub list: Vec<String>,
    /// Profile opened when no --profile argument is given.
    #[serde(default)]
    pub last: String,
    /// Show the profile picker on every launch (when more than one profile exists).
    #[serde(default)]
    pub ask_at_startup: bool,
}

pub fn load_profiles() -> ProfilesFile {
    fs::read_to_string(root_dir().join("profiles.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_profiles(p: &ProfilesFile) {
    if let Ok(s) = serde_json::to_string_pretty(p) {
        let _ = fs::write(root_dir().join("profiles.json"), s);
    }
}

/// Directory holding unpacked browser extensions (one folder per extension, each with a
/// manifest.json). Loaded into the WebView2 profile at launch. Created empty on first use.
pub fn extensions_dir() -> PathBuf {
    let dir = data_dir().join("extensions");
    let _ = fs::create_dir_all(&dir);
    dir
}

/// True if the extensions dir contains at least one subfolder with a manifest.json (a loadable
/// unpacked extension). Lets us skip enabling extension support when there's nothing to load.
pub fn has_extensions() -> bool {
    fs::read_dir(extensions_dir())
        .map(|rd| {
            rd.flatten()
                .any(|e| e.path().join("manifest.json").is_file())
        })
        .unwrap_or(false)
}

/// One restored tab (URL + whether it was pinned/kept-awake + its tab-group id, if any).
#[derive(Serialize, Deserialize, Clone)]
pub struct SessionTab {
    pub url: String,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub group: Option<u32>,
    /// Workspace this tab lives in (0 = the default "Main" workspace).
    #[serde(default)]
    pub workspace: u32,
    /// Unix seconds the user last had this tab active; feeds tab auto-archiving across restarts.
    /// 0 in older session files; treated as "just used" on restore.
    #[serde(default)]
    pub last_used: u64,
}

/// A saved tab group (color-labeled, optionally collapsed). Stored separately in groups.json.
#[derive(Serialize, Deserialize, Clone)]
pub struct SessionGroup {
    pub id: u32,
    pub name: String,
    pub color: String,
    #[serde(default)]
    pub collapsed: bool,
}

pub fn load_groups() -> Vec<SessionGroup> {
    fs::read_to_string(data_dir().join("groups.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_groups(groups: &[SessionGroup]) {
    if let Ok(s) = serde_json::to_string_pretty(groups) {
        let _ = fs::write(data_dir().join("groups.json"), s);
    }
}

/// A named, color-labeled tab context. The strip shows one workspace at a time; the rest of the
/// tabs stay loaded (and keep sleeping/discarding) out of view. Persisted in workspaces.json.
#[derive(Serialize, Deserialize, Clone)]
pub struct SessionWorkspace {
    pub id: u32,
    pub name: String,
    pub color: String,
}

#[derive(Serialize, Deserialize, Default)]
pub struct WorkspaceFile {
    #[serde(default)]
    pub list: Vec<SessionWorkspace>,
    /// Workspace shown when the app was last closed.
    #[serde(default)]
    pub active: u32,
}

pub fn load_workspaces() -> WorkspaceFile {
    fs::read_to_string(data_dir().join("workspaces.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_workspaces(file: &WorkspaceFile) {
    if let Ok(s) = serde_json::to_string_pretty(file) {
        let _ = fs::write(data_dir().join("workspaces.json"), s);
    }
}

/// The set of open tabs to restore on launch.
#[derive(Serialize, Deserialize, Default)]
pub struct Session {
    pub tabs: Vec<SessionTab>,
    #[serde(default)]
    pub active: usize,
}

pub fn load_session() -> Session {
    fs::read_to_string(data_dir().join("session.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_session(session: &Session) {
    if let Ok(s) = serde_json::to_string_pretty(session) {
        let _ = fs::write(data_dir().join("session.json"), s);
    }
}

pub fn clear_session() {
    let _ = fs::write(
        data_dir().join("session.json"),
        serde_json::to_string_pretty(&Session::default()).unwrap_or_else(|_| "{}".into()),
    );
}

pub fn load_closed_session() -> Session {
    fs::read_to_string(data_dir().join("last_closed_session.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_closed_session(session: &Session) {
    if session.tabs.is_empty() {
        return;
    }
    if let Ok(s) = serde_json::to_string_pretty(session) {
        let _ = fs::write(data_dir().join("last_closed_session.json"), s);
    }
}

pub fn clear_closed_session() {
    let _ = fs::remove_file(data_dir().join("last_closed_session.json"));
}

#[derive(Serialize, Deserialize, Clone)]
#[derive(Debug)]
pub struct Bookmark {
    pub title: String,
    pub url: String,
    /// Optional folder name; bookmarks sharing a name group under a folder chip in the bar.
    #[serde(default)]
    pub folder: Option<String>,
}

pub fn load_bookmarks() -> Vec<Bookmark> {
    fs::read_to_string(data_dir().join("bookmarks.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_bookmarks(bookmarks: &[Bookmark]) {
    if let Ok(s) = serde_json::to_string_pretty(bookmarks) {
        let _ = fs::write(data_dir().join("bookmarks.json"), s);
    }
}

/// A saved-for-later page (reading list), separate from bookmarks.
#[derive(Serialize, Deserialize, Clone)]
pub struct ReadingItem {
    pub title: String,
    pub url: String,
}

pub fn load_reading() -> Vec<ReadingItem> {
    fs::read_to_string(data_dir().join("reading.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_reading(items: &[ReadingItem]) {
    if let Ok(s) = serde_json::to_string_pretty(items) {
        let _ = fs::write(data_dir().join("reading.json"), s);
    }
}

/// A user-editable home-page shortcut (icon name + label + URL).
#[derive(Serialize, Deserialize, Clone)]
pub struct QuickLink {
    pub icon: String,
    pub label: String,
    pub url: String,
}

pub fn load_links() -> Vec<QuickLink> {
    fs::read_to_string(data_dir().join("links.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(default_links)
}

pub fn save_links(links: &[QuickLink]) {
    if let Ok(s) = serde_json::to_string_pretty(links) {
        let _ = fs::write(data_dir().join("links.json"), s);
    }
}

fn default_links() -> Vec<QuickLink> {
    let l = |icon: &str, label: &str, url: &str| QuickLink {
        icon: icon.to_string(),
        label: label.to_string(),
        url: url.to_string(),
    };
    vec![
        l("mail", "Gmail", "https://mail.google.com"),
        l("calendar", "Calendar", "https://calendar.google.com"),
        l("drive", "Drive", "https://drive.google.com"),
        l("doc", "Docs", "https://docs.google.com"),
        l("video", "YouTube", "https://youtube.com"),
        l("code", "GitHub", "https://github.com"),
        l("globe", "Wikipedia", "https://wikipedia.org"),
        l("map", "Maps", "https://maps.google.com"),
    ]
}

/// Local-AI layer configuration, all editable in the settings panel (so the project is customizable
/// when open-sourced). Every field has a serde default, so an old settings.json without an `ai` block
/// still loads with sensible defaults.
#[derive(Serialize, Deserialize, Clone)]
pub struct AiSettings {
    /// Master switch. When false the assistant is fully off (no sidebar, pill, omnibox, etc.).
    #[serde(default = "df_true")]
    pub enabled: bool,
    /// Ollama model for chat/vision (must support `vision` if the screenshot button is used).
    #[serde(default = "df_model")]
    pub model: String,
    /// Ollama base URL (loopback by default; change to point at another host, e.g. over Tailscale).
    #[serde(default = "df_host")]
    pub host: String,
    /// How long Ollama keeps the model resident after a request (e.g. "5m", "30s", "0").
    #[serde(default = "df_keep_alive")]
    pub keep_alive: String,
    /// Unload the model from VRAM when the sidebar closes (frees the GPU between uses).
    #[serde(default = "df_true")]
    pub unload_on_close: bool,
    /// Show the Explain/Translate/Rewrite pill when text is selected on a page.
    #[serde(default = "df_true")]
    pub selection_pill: bool,
    /// Route a leading "?" in the address bar to the AI instead of search.
    #[serde(default = "df_true")]
    pub omnibox_ask: bool,
    /// Offer the Web scope (agentic search). Off = no outbound search requests from the assistant.
    #[serde(default = "df_true")]
    pub web_search: bool,
    /// Offer the screenshot (vision) button.
    #[serde(default = "df_true")]
    pub vision: bool,
    /// How many search results to read for a Web answer.
    #[serde(default = "df_web_results")]
    pub web_results: u8,
    /// Target language for the "Translate page" action.
    #[serde(default = "df_translate_to")]
    pub translate_to: String,
}

fn df_translate_to() -> String {
    "English".to_string()
}

fn df_true() -> bool {
    true
}
fn df_model() -> String {
    "qwen3.5-gpu".to_string()
}
/// The default chat model, exposed so callers can coerce a blank/missing model back to it.
pub fn default_ai_model() -> String {
    df_model()
}
fn df_host() -> String {
    "http://127.0.0.1:11434".to_string()
}
fn df_keep_alive() -> String {
    "5m".to_string()
}
fn df_web_results() -> u8 {
    3
}

impl Default for AiSettings {
    fn default() -> Self {
        AiSettings {
            enabled: true,
            model: df_model(),
            host: df_host(),
            keep_alive: df_keep_alive(),
            unload_on_close: true,
            selection_pill: true,
            omnibox_ask: true,
            web_search: true,
            vision: true,
            web_results: df_web_results(),
            translate_to: df_translate_to(),
        }
    }
}

/// A keyword search engine ("bang"): typing `<keyword> query` in the address bar searches it.
#[derive(Serialize, Deserialize, Clone)]
pub struct SearchEngine {
    /// Trigger word, e.g. "w" or "gh".
    pub keyword: String,
    /// Display name.
    pub name: String,
    /// URL template; "{q}" is replaced with the URL-encoded query.
    pub url: String,
}

fn default_engines() -> Vec<SearchEngine> {
    let e = |keyword: &str, name: &str, url: &str| SearchEngine {
        keyword: keyword.to_string(),
        name: name.to_string(),
        url: url.to_string(),
    };
    vec![
        e("w", "Wikipedia", "https://en.wikipedia.org/w/index.php?search={q}"),
        e("gh", "GitHub", "https://github.com/search?q={q}"),
        e("yt", "YouTube", "https://www.youtube.com/results?search_query={q}"),
    ]
}

/// Color scheme. Six semantic colors the UI is built from; defaults are Aperture's stock dark palette
/// (so an existing settings.json with no theme keeps the current look). The run loop maps these onto
/// each UI's CSS variables at load + on save.
#[derive(Serialize, Deserialize, Clone)]
pub struct Theme {
    /// Deepest background (page/new-tab background).
    #[serde(default = "df_bg")]
    pub background: String,
    /// Raised surfaces: toolbar, panels, cards, inputs.
    #[serde(default = "df_panel")]
    pub panel: String,
    /// Borders / dividers.
    #[serde(default = "df_border")]
    pub border: String,
    /// Primary text.
    #[serde(default = "df_text")]
    pub text: String,
    /// Secondary / muted text.
    #[serde(default = "df_muted")]
    pub muted: String,
    /// Accent (links, active states, highlights).
    #[serde(default = "df_accent")]
    pub accent: String,
}

fn df_bg() -> String {
    "#0d0f13".to_string()
}
fn df_panel() -> String {
    "#14171c".to_string()
}
fn df_border() -> String {
    "#232830".to_string()
}
fn df_text() -> String {
    "#d2d7df".to_string()
}
fn df_muted() -> String {
    "#6f7682".to_string()
}
fn df_accent() -> String {
    "#5b9cff".to_string()
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            background: df_bg(),
            panel: df_panel(),
            border: df_border(),
            text: df_text(),
            muted: df_muted(),
            accent: df_accent(),
        }
    }
}

/// Browsing-trail settings (the navigation graph on the new-tab page).
#[derive(Serialize, Deserialize, Clone)]
pub struct TrailSettings {
    /// Master switch: record visits and show the graph. Off = no recording at all.
    #[serde(default = "df_true")]
    pub enabled: bool,
    /// Default time window (days) the new-tab graph shows. The page has its own range picker;
    /// this is the starting selection.
    #[serde(default = "df_graph_days")]
    pub graph_days: u64,
    /// Visits older than this are pruned from trail.json.
    #[serde(default = "df_retention_days")]
    pub retention_days: u64,
    /// Host suffixes never recorded (e.g. "bank.com" also covers "www.bank.com").
    #[serde(default)]
    pub exclude: Vec<String>,
}

fn df_graph_days() -> u64 {
    7
}
fn df_retention_days() -> u64 {
    30
}

impl Default for TrailSettings {
    fn default() -> Self {
        TrailSettings {
            enabled: true,
            graph_days: df_graph_days(),
            retention_days: df_retention_days(),
            exclude: Vec::new(),
        }
    }
}

/// Air Traffic Control: a link-routing rule. Links whose destination host matches `pattern`
/// (suffix match, subdomains included) open in workspace `workspace` instead of the current one.
#[derive(Serialize, Deserialize, Clone)]
pub struct AtcRule {
    pub pattern: String,
    pub workspace: u32,
}

/// Another Aperture device on the tailnet, for "send tab to device" (and, later, sync).
#[derive(Serialize, Deserialize, Clone)]
pub struct Peer {
    /// Display label ("Laptop").
    pub name: String,
    /// Tailnet address: a MagicDNS name or a 100.x IP. No scheme or port.
    pub address: String,
}

/// Cross-device settings. Transport is Tailscale: a small HTTP listener bound to the tailnet
/// interface, gated by a shared token that must match on every device.
#[derive(Serialize, Deserialize, Clone)]
pub struct SyncSettings {
    /// Turn the listener + send-to-device on.
    #[serde(default)]
    pub enabled: bool,
    /// This device's label, shown to peers.
    #[serde(default)]
    pub device_name: String,
    /// Shared secret; a request is accepted only if its token matches. Generated on first use.
    #[serde(default)]
    pub token: String,
    /// Known peer devices.
    #[serde(default)]
    pub peers: Vec<Peer>,
}

impl Default for SyncSettings {
    fn default() -> Self {
        SyncSettings {
            enabled: false,
            device_name: String::new(),
            token: String::new(),
            peers: Vec::new(),
        }
    }
}

/// Fixed port for the device-to-device listener (bound to the Tailscale IP only).
pub const SYNC_PORT: u16 = 8799;

/// Local safe-browsing (phishing/malware) settings. The blocklist is checked locally at
/// navigation time; only the list is downloaded, never the URLs you visit.
#[derive(Serialize, Deserialize, Clone)]
pub struct SafetySettings {
    /// Warn before loading known-malicious sites. On by default (safety-forward).
    #[serde(default = "df_true")]
    pub enabled: bool,
    /// Hosts-format blocklist feed. Downloaded periodically; empty = seed list only.
    #[serde(default = "df_safety_feed")]
    pub feed_url: String,
    /// Unix seconds of the last successful list update (0 = never).
    #[serde(default)]
    pub last_updated: u64,
}

fn df_safety_feed() -> String {
    "https://urlhaus.abuse.ch/downloads/hostfile/".to_string()
}

impl Default for SafetySettings {
    fn default() -> Self {
        SafetySettings {
            enabled: true,
            feed_url: df_safety_feed(),
            last_updated: 0,
        }
    }
}

/// User preferences edited in the settings panel.
#[derive(Serialize, Deserialize, Clone)]
pub struct Settings {
    /// Shown in the new-tab greeting ("Good morning, <name>"); empty = no name.
    #[serde(default)]
    pub name: String,
    /// Search URL template; "{q}" is replaced with the URL-encoded query.
    #[serde(default = "default_search")]
    pub search_url: String,
    /// Seconds a backgrounded tab idles before sleeping (discarded at ~2x). Applied at launch.
    #[serde(default = "default_idle")]
    pub idle_secs: u64,
    /// Local-AI layer settings.
    #[serde(default)]
    pub ai: AiSettings,
    /// Color scheme.
    #[serde(default)]
    pub theme: Theme,
    /// Keyword search engines (bangs).
    #[serde(default = "default_engines")]
    pub search_engines: Vec<SearchEngine>,
    /// Privacy: on startup, clear cookies for every site EXCEPT `keep_logged_in` (so you start each
    /// session signed out of everything but your chosen sites). Off by default (persistent profile).
    #[serde(default)]
    pub clear_other_on_launch: bool,
    /// Domains to stay signed in to when `clear_other_on_launch` is on (e.g. "google.com").
    #[serde(default)]
    pub keep_logged_in: Vec<String>,
    /// Bookmark folder names, in bar order. Stored explicitly so an empty (just-created) folder
    /// persists and shows as a chip before anything is dragged into it.
    #[serde(default)]
    pub bookmark_folders: Vec<String>,
    /// Tab strip orientation. false = horizontal top strip (default), true = vertical left column.
    #[serde(default)]
    pub vertical_tabs: bool,
    /// Keyboard shortcut overrides: action id -> binding string ("ctrl+shift+t"). Only actions the
    /// user changed are stored; absent actions use the built-in default (main.rs SHORTCUT_ACTIONS).
    #[serde(default)]
    pub shortcuts: std::collections::HashMap<String, String>,
    /// Browsing-trail (navigation graph) settings.
    #[serde(default)]
    pub trail: TrailSettings,
    /// Tab auto-archive: unpinned tabs untouched this long quietly close into the archive
    /// (searchable from the history viewer). Pinned, private, audible, and downloading tabs
    /// are exempt.
    #[serde(default = "df_true")]
    pub archive_enabled: bool,
    #[serde(default = "df_archive_days")]
    pub archive_days: u64,
    /// Link-routing rules (Air Traffic Control): destination host -> workspace.
    #[serde(default)]
    pub atc_rules: Vec<AtcRule>,
    /// Cross-device (Tailscale) settings.
    #[serde(default)]
    pub sync: SyncSettings,
    /// Local safe-browsing settings.
    #[serde(default)]
    pub safety: SafetySettings,
}

fn df_archive_days() -> u64 {
    14
}

fn default_search() -> String {
    // The real DuckDuckGo page: its own dark mode, logo, and rich results (images/instant answers).
    "https://duckduckgo.com/?q={q}".to_string()
}
fn default_idle() -> u64 {
    300
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            name: String::new(),
            search_url: default_search(),
            idle_secs: default_idle(),
            ai: AiSettings::default(),
            theme: Theme::default(),
            search_engines: default_engines(),
            clear_other_on_launch: false,
            keep_logged_in: Vec::new(),
            bookmark_folders: Vec::new(),
            vertical_tabs: false,
            shortcuts: std::collections::HashMap::new(),
            trail: TrailSettings::default(),
            archive_enabled: true,
            archive_days: df_archive_days(),
            atc_rules: Vec::new(),
            sync: SyncSettings::default(),
            safety: SafetySettings::default(),
        }
    }
}

pub fn load_settings() -> Settings {
    fs::read_to_string(data_dir().join("settings.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_settings(settings: &Settings) {
    if let Ok(s) = serde_json::to_string_pretty(settings) {
        let _ = fs::write(data_dir().join("settings.json"), s);
    }
}

/// Per-site zoom factors, keyed by host (e.g. "github.com" -> 1.1). Hosts at default 100% are
/// dropped from the map rather than stored.
pub fn load_zoom() -> HashMap<String, f64> {
    fs::read_to_string(data_dir().join("zoom.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_zoom(zoom: &HashMap<String, f64>) {
    if let Ok(s) = serde_json::to_string(zoom) {
        let _ = fs::write(data_dir().join("zoom.json"), s);
    }
}

pub const HISTORY_CAP: usize = 1000;

#[derive(Serialize, Deserialize, Clone)]
#[derive(Debug)]
pub struct HistoryEntry {
    pub url: String,
    pub title: String,
}

pub fn load_history() -> Vec<HistoryEntry> {
    fs::read_to_string(data_dir().join("history.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_history(history: &[HistoryEntry]) {
    if let Ok(s) = serde_json::to_string(history) {
        let _ = fs::write(data_dir().join("history.json"), s);
    }
}

const ARCHIVE_CAP: usize = 500;

/// A tab the auto-archiver closed (or nothing, if archiving is off). Newest first.
#[derive(Serialize, Deserialize, Clone)]
pub struct ArchivedTab {
    pub url: String,
    pub title: String,
    /// Unix seconds when it was archived.
    pub ts: u64,
}

pub fn load_archive() -> Vec<ArchivedTab> {
    fs::read_to_string(data_dir().join("archive.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_archive(archive: &[ArchivedTab]) {
    if let Ok(s) = serde_json::to_string(archive) {
        let _ = fs::write(data_dir().join("archive.json"), s);
    }
}

/// Add an archived tab to the front, de-duplicated by URL, capped.
pub fn record_archive(archive: &mut Vec<ArchivedTab>, url: &str, title: &str) {
    archive.retain(|a| a.url != url);
    archive.insert(
        0,
        ArchivedTab {
            url: url.to_string(),
            title: title.to_string(),
            ts: unix_now(),
        },
    );
    archive.truncate(ARCHIVE_CAP);
}

const CLOSED_GROUPS_CAP: usize = 25;

/// One tab within a closed group.
#[derive(Serialize, Deserialize, Clone)]
pub struct ClosedTab {
    pub url: String,
    pub title: String,
}

/// A set of tabs that were closed together (a window close, a profile switch, a closed tab group,
/// or a deleted workspace), so the whole batch can be reopened as a unit from the History viewer.
#[derive(Serialize, Deserialize, Clone)]
pub struct ClosedGroup {
    /// Unix seconds when the batch closed.
    pub ts: u64,
    /// What was closed ("Window", a group/workspace name, ...).
    pub label: String,
    pub tabs: Vec<ClosedTab>,
}

pub fn load_closed_groups() -> Vec<ClosedGroup> {
    fs::read_to_string(data_dir().join("closed_groups.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_closed_groups(groups: &[ClosedGroup]) {
    if let Ok(s) = serde_json::to_string(groups) {
        let _ = fs::write(data_dir().join("closed_groups.json"), s);
    }
}

/// Record a batch of closed tabs (newest first, capped). Ignores empty batches.
pub fn record_closed_group(groups: &mut Vec<ClosedGroup>, label: &str, tabs: Vec<ClosedTab>) {
    if tabs.is_empty() {
        return;
    }
    groups.insert(
        0,
        ClosedGroup {
            ts: unix_now(),
            label: label.to_string(),
            tabs,
        },
    );
    groups.truncate(CLOSED_GROUPS_CAP);
}

const TRAIL_CAP: usize = 4000;

/// One node-visit on the browsing trail (feeds the new-tab navigation graph). Unlike history
/// entries these keep timestamps and the page they were reached FROM, so edges can be drawn.
#[derive(Serialize, Deserialize, Clone)]
pub struct TrailVisit {
    pub url: String,
    pub title: String,
    /// Unix seconds.
    pub ts: u64,
    /// URL this page was reached from: the tab's previous page, or the opener tab's page for
    /// links opened in a new tab. None for direct entries (omnibox, shortcuts, restored tabs).
    #[serde(default)]
    pub from: Option<String>,
}

pub fn load_trail() -> Vec<TrailVisit> {
    fs::read_to_string(data_dir().join("trail.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_trail(trail: &[TrailVisit]) {
    if let Ok(s) = serde_json::to_string(trail) {
        let _ = fs::write(data_dir().join("trail.json"), s);
    }
}

/// Only real web pages belong on the trail (never the home page, app pages, or extensions).
pub fn trailable(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Drop trail entries past the retention window, and enforce the size cap (oldest out first;
/// entries are stored oldest-first).
pub fn prune_trail(trail: &mut Vec<TrailVisit>, retention_days: u64) {
    let cutoff = unix_now().saturating_sub(retention_days.max(1) * 86_400);
    trail.retain(|v| v.ts >= cutoff);
    if trail.len() > TRAIL_CAP {
        let excess = trail.len() - TRAIL_CAP;
        trail.drain(..excess);
    }
}

/// Append a visit (newest last). Same-page reloads (same url AND same origin edge) are collapsed
/// into the existing entry rather than stacking duplicates.
pub fn record_trail(
    trail: &mut Vec<TrailVisit>,
    url: &str,
    title: &str,
    from: Option<String>,
    retention_days: u64,
) {
    if !trailable(url) {
        return;
    }
    let from = from.filter(|f| trailable(f) && f != url);
    if let Some(last) = trail.last_mut() {
        if last.url == url && last.from == from {
            last.ts = unix_now();
            return;
        }
    }
    trail.push(TrailVisit {
        url: url.to_string(),
        title: title.to_string(),
        ts: unix_now(),
        from,
    });
    prune_trail(trail, retention_days);
}

/// Promote a visited page to the front (most-recent-first), de-duplicated by URL, capped.
pub fn record_visit(history: &mut Vec<HistoryEntry>, url: &str, title: &str) {
    if url.is_empty() || url.starts_with("about:") || url.starts_with("file://") {
        return;
    }
    history.retain(|h| h.url != url);
    history.insert(
        0,
        HistoryEntry {
            url: url.to_string(),
            title: title.to_string(),
        },
    );
    history.truncate(HISTORY_CAP);
}

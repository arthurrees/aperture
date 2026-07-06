//! Import from Google Chrome: profile discovery, bookmarks, history, and unpacked extensions.
//! Passwords are deliberately NOT imported: Chrome encrypts them with app-bound encryption that
//! other processes cannot (and should not) unwrap. The supported route is Chrome's password CSV
//! export into a password manager (e.g. Bitwarden, which Aperture fills from).

use std::path::{Path, PathBuf};

use crate::store;

/// One Chrome profile found on disk. `dir` is the folder name ("Default", "Profile 1"),
/// `name` the user-visible name from Local State when available.
pub struct ChromeProfile {
    pub dir: String,
    pub name: String,
}

/// Chrome's user-data root, if Chrome has ever run on this machine.
fn user_data_dir() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)?;
    let dir = base.join("Google").join("Chrome").join("User Data");
    dir.is_dir().then_some(dir)
}

/// The on-disk folder for a profile by its dir name, validated against the real listing (so a
/// crafted name can't escape the user-data root).
pub fn profile_dir(dir_name: &str) -> Option<PathBuf> {
    let ud = user_data_dir()?;
    detect_profiles()
        .iter()
        .find(|p| p.dir == dir_name)
        .map(|p| ud.join(&p.dir))
}

/// Chrome profiles that actually hold data (a Bookmarks or History file).
pub fn detect_profiles() -> Vec<ChromeProfile> {
    let Some(ud) = user_data_dir() else {
        return Vec::new();
    };
    let local_state: serde_json::Value = std::fs::read_to_string(ud.join("Local State"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let cache = &local_state["profile"]["info_cache"];
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&ud) {
        for e in rd.flatten() {
            let p = e.path();
            if !p.join("Bookmarks").is_file() && !p.join("History").is_file() {
                continue;
            }
            let dir = e.file_name().to_string_lossy().to_string();
            let name = cache[&dir]["name"]
                .as_str()
                .unwrap_or(&dir)
                .to_string();
            out.push(ChromeProfile { dir, name });
        }
    }
    out.sort_by(|a, b| a.dir.cmp(&b.dir));
    out
}

/// Bookmarks parsed out of Chrome's Bookmarks JSON, mapped onto Aperture's flat folder model.
pub struct ImportedBookmarks {
    pub items: Vec<store::Bookmark>,
    /// Folder names referenced by `items`, in first-seen order.
    pub folders: Vec<String>,
}

const BOOKMARK_IMPORT_CAP: usize = 2000;

/// Read a profile's bookmarks. Chrome nests folders arbitrarily; Aperture has one flat folder
/// level, so an item keeps its TOP-MOST Chrome folder ("bar/Dev/Rust/x" lands in "Dev"). Items
/// loose on the bookmarks bar stay folderless; everything under "Other bookmarks" (and the
/// mobile/synced root) gets that as its folder.
pub fn read_bookmarks(profile: &Path) -> ImportedBookmarks {
    let mut out = ImportedBookmarks {
        items: Vec::new(),
        folders: Vec::new(),
    };
    let Some(v) = std::fs::read_to_string(profile.join("Bookmarks"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
    else {
        return out;
    };
    walk_bookmarks(&v["roots"]["bookmark_bar"], None, &mut out);
    walk_bookmarks(&v["roots"]["other"], Some("Other bookmarks"), &mut out);
    walk_bookmarks(&v["roots"]["synced"], Some("Other bookmarks"), &mut out);
    out
}

fn walk_bookmarks(node: &serde_json::Value, folder: Option<&str>, out: &mut ImportedBookmarks) {
    let Some(children) = node["children"].as_array() else {
        return;
    };
    for c in children {
        match c["type"].as_str() {
            Some("url") => {
                if out.items.len() >= BOOKMARK_IMPORT_CAP {
                    return;
                }
                let url = c["url"].as_str().unwrap_or_default();
                if !url.starts_with("http://") && !url.starts_with("https://") {
                    continue;
                }
                if let Some(f) = folder {
                    if !out.folders.iter().any(|x| x == f) {
                        out.folders.push(f.to_string());
                    }
                }
                out.items.push(store::Bookmark {
                    title: c["name"].as_str().unwrap_or(url).to_string(),
                    url: url.to_string(),
                    folder: folder.map(str::to_string),
                });
            }
            Some("folder") => {
                let name = c["name"].as_str().unwrap_or("Imported");
                // The outermost folder wins; deeper nesting flattens into it.
                let next = folder.unwrap_or(name);
                walk_bookmarks(c, Some(next), out);
            }
            _ => {}
        }
    }
}

/// Read a profile's most recent history entries (newest first). Chrome locks the live database,
/// so it is copied to a temp file first; the copy is deleted after.
pub fn read_history(profile: &Path, limit: usize) -> Vec<store::HistoryEntry> {
    let src = profile.join("History");
    let tmp = std::env::temp_dir().join(format!(
        "aperture_chrome_history_{}.db",
        std::process::id()
    ));
    if std::fs::copy(&src, &tmp).is_err() {
        return Vec::new();
    }
    let mut out = Vec::new();
    if let Ok(conn) =
        rusqlite::Connection::open_with_flags(&tmp, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
    {
        let q = "SELECT url, title FROM urls WHERE hidden = 0 ORDER BY last_visit_time DESC LIMIT ?1";
        if let Ok(mut stmt) = conn.prepare(q) {
            let rows = stmt.query_map([limit as i64], |r| {
                let url: String = r.get(0)?;
                let title: Option<String> = r.get(1)?;
                Ok((url, title.unwrap_or_default()))
            });
            if let Ok(rows) = rows {
                for row in rows.flatten() {
                    let (url, title) = row;
                    if url.starts_with("http://") || url.starts_with("https://") {
                        out.push(store::HistoryEntry {
                            title: if title.is_empty() { url.clone() } else { title },
                            url,
                        });
                    }
                }
            }
        }
    }
    let _ = std::fs::remove_file(&tmp);
    out
}

/// Copy a profile's unpacked extensions into Aperture's extensions dir. Returns one
/// (display name, note) line per extension found. Themes are skipped; loading happens at the
/// next launch (the extension option is set when the WebView2 environment is created).
pub fn copy_extensions(profile: &Path, dest_root: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let root = profile.join("Extensions");
    let Ok(rd) = std::fs::read_dir(&root) else {
        return out;
    };
    for e in rd.flatten() {
        let id = e.file_name().to_string_lossy().to_string();
        // Newest version folder by modification time.
        let Some(ver_dir) = std::fs::read_dir(e.path())
            .ok()
            .and_then(|vd| {
                vd.flatten()
                    .filter(|v| v.path().join("manifest.json").is_file())
                    .max_by_key(|v| {
                        v.metadata()
                            .and_then(|m| m.modified())
                            .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
                    })
            })
            .map(|v| v.path())
        else {
            continue;
        };
        let Some(manifest) = std::fs::read_to_string(ver_dir.join("manifest.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        else {
            continue;
        };
        let name = extension_name(&manifest, &ver_dir).unwrap_or_else(|| id.clone());
        if !manifest["theme"].is_null() {
            out.push((name, "skipped (theme)".to_string()));
            continue;
        }
        let dest = dest_root.join(&id);
        if dest.exists() {
            out.push((name, "already installed".to_string()));
            continue;
        }
        match copy_dir(&ver_dir, &dest) {
            Ok(()) => out.push((name, "copied; loads on next launch".to_string())),
            Err(err) => out.push((name, format!("copy failed: {err}"))),
        }
    }
    out
}

/// Resolve a manifest's name, following Chrome's "__MSG_key__" indirection into the default
/// locale's messages.json when needed.
fn extension_name(manifest: &serde_json::Value, ext_dir: &Path) -> Option<String> {
    let raw = manifest["name"].as_str()?;
    let Some(key) = raw
        .strip_prefix("__MSG_")
        .and_then(|s| s.strip_suffix("__"))
    else {
        return Some(raw.to_string());
    };
    let locale = manifest["default_locale"].as_str().unwrap_or("en");
    let messages: serde_json::Value = std::fs::read_to_string(
        ext_dir.join("_locales").join(locale).join("messages.json"),
    )
    .ok()
    .and_then(|s| serde_json::from_str(&s).ok())?;
    // Message keys are case-insensitive in Chrome.
    messages
        .as_object()?
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .and_then(|(_, v)| v["message"].as_str())
        .map(str::to_string)
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)?.flatten() {
        let from = e.path();
        let to = dst.join(e.file_name());
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bookmarks_flatten_to_topmost_folder() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{
              "roots": {
                "bookmark_bar": { "children": [
                  { "type": "url", "name": "Loose", "url": "https://a.com/" },
                  { "type": "folder", "name": "Dev", "children": [
                    { "type": "url", "name": "Rust", "url": "https://rust-lang.org/" },
                    { "type": "folder", "name": "Deep", "children": [
                      { "type": "url", "name": "Nested", "url": "https://b.com/" }
                    ]}
                  ]}
                ]},
                "other": { "children": [
                  { "type": "url", "name": "Misc", "url": "https://c.com/" },
                  { "type": "url", "name": "Script", "url": "javascript:alert(1)" }
                ]},
                "synced": {}
              }
            }"#,
        )
        .unwrap();
        let mut out = ImportedBookmarks {
            items: Vec::new(),
            folders: Vec::new(),
        };
        walk_bookmarks(&json["roots"]["bookmark_bar"], None, &mut out);
        walk_bookmarks(&json["roots"]["other"], Some("Other bookmarks"), &mut out);
        walk_bookmarks(&json["roots"]["synced"], Some("Other bookmarks"), &mut out);

        let by_url: Vec<(&str, Option<&str>)> = out
            .items
            .iter()
            .map(|b| (b.url.as_str(), b.folder.as_deref()))
            .collect();
        assert_eq!(
            by_url,
            vec![
                ("https://a.com/", None),
                ("https://rust-lang.org/", Some("Dev")),
                ("https://b.com/", Some("Dev")),
                ("https://c.com/", Some("Other bookmarks")),
            ]
        );
        assert_eq!(out.folders, vec!["Dev", "Other bookmarks"]);
    }
}

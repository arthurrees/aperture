//! On-demand Bitwarden autofill via the official `bw` CLI.
//!
//! Locked by default. When the user asks to fill a login we prompt for the master password, unlock
//! just long enough to fetch the matching credential, then lock again. Nothing stays decrypted in
//! the background, and the master password is zeroized as soon as `bw unlock` has consumed it.
//!
//! These calls spawn the Node-based `bw` (1-3s each), so the caller runs `fetch` on a background
//! thread and posts the result back to the UI thread - never block the event loop on it.

use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};

use zeroize::Zeroize;

/// Spawn `bw` without flashing a console window (cmd /C would otherwise pop a blank terminal).
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Outcome of an autofill attempt (Send, so it can travel back over the event-loop proxy).
#[derive(Debug)]
pub enum FillOutcome {
    Found { username: String, password: String },
    NoMatch,
    NotLoggedIn,
    Error(String),
}

/// Outcome of fetching a passkey for a relying party from the vault.
pub enum PasskeyOutcome {
    Found(crate::webauthn::Passkey),
    NoMatch,
    NotLoggedIn,
    Error(String),
}

// Manual Debug so the private key in `Found` is never written to logs.
impl std::fmt::Debug for PasskeyOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PasskeyOutcome::Found(_) => write!(f, "PasskeyOutcome::Found(<redacted>)"),
            PasskeyOutcome::NoMatch => write!(f, "PasskeyOutcome::NoMatch"),
            PasskeyOutcome::NotLoggedIn => write!(f, "PasskeyOutcome::NotLoggedIn"),
            PasskeyOutcome::Error(e) => write!(f, "PasskeyOutcome::Error({e:?})"),
        }
    }
}

/// Unlock the vault, find a passkey (FIDO2 credential) whose rpId matches `rp_id`, return its key
/// material, then lock. `master_password` is zeroized before returning. Mirrors `fetch` but pulls
/// `login.fido2Credentials` instead of username/password.
pub fn fetch_passkey(master_password: &mut String, rp_id: &str) -> PasskeyOutcome {
    let Some(bw) = bw_cmd() else {
        master_password.zeroize();
        return PasskeyOutcome::Error("Bitwarden CLI not found.".into());
    };

    let unlock = Command::new("cmd")
        .creation_flags(CREATE_NO_WINDOW)
        .arg("/C")
        .arg(&bw)
        .args(["unlock", "--passwordenv", "BW_PASSWORD", "--raw"])
        .env("BW_PASSWORD", &*master_password)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    master_password.zeroize();

    let session = match unlock {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() {
                return PasskeyOutcome::Error("Unlock returned an empty session".into());
            }
            s
        }
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr).to_lowercase();
            if err.contains("not logged in") {
                return PasskeyOutcome::NotLoggedIn;
            }
            return PasskeyOutcome::Error(trim_err(&o.stderr, "Unlock failed"));
        }
        Err(e) => return PasskeyOutcome::Error(format!("Couldn't run bw: {e}")),
    };

    let list = Command::new("cmd")
        .creation_flags(CREATE_NO_WINDOW)
        .arg("/C")
        .arg(&bw)
        .args(["list", "items", "--session", &session])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();

    let _ = Command::new("cmd")
        .creation_flags(CREATE_NO_WINDOW)
        .arg("/C")
        .arg(&bw)
        .arg("lock")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output();

    match list {
        Ok(o) if o.status.success() => {
            match parse_passkey(&String::from_utf8_lossy(&o.stdout), rp_id) {
                Some(pk) => PasskeyOutcome::Found(pk),
                None => PasskeyOutcome::NoMatch,
            }
        }
        Ok(o) => PasskeyOutcome::Error(trim_err(&o.stderr, "Lookup failed")),
        Err(e) => PasskeyOutcome::Error(format!("Couldn't run bw: {e}")),
    }
}

/// Find the first vault item with a `login.fido2Credentials` entry whose rpId matches, and build a
/// Passkey from it.
fn parse_passkey(json: &str, rp_id: &str) -> Option<crate::webauthn::Passkey> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    for item in v.as_array()? {
        let creds = match item["login"]["fido2Credentials"].as_array() {
            Some(c) => c,
            None => continue,
        };
        for c in creds {
            let cred_rp = c["rpId"].as_str().unwrap_or_default();
            if cred_rp != rp_id {
                continue;
            }
            let key_value = c["keyValue"].as_str().unwrap_or_default();
            let private_key_pkcs8 = crate::webauthn::b64_any_decode(key_value)?;
            let counter = c["counter"]
                .as_str()
                .and_then(|s| s.parse::<u32>().ok())
                .or_else(|| c["counter"].as_u64().map(|n| n as u32))
                .unwrap_or(0);
            return Some(crate::webauthn::Passkey {
                credential_id: c["credentialId"].as_str().unwrap_or_default().to_string(),
                user_handle: c["userHandle"].as_str().unwrap_or_default().to_string(),
                private_key_pkcs8,
                rp_id: cred_rp.to_string(),
                counter,
            });
        }
    }
    None
}

/// Outcome of a save-to-vault attempt.
#[derive(Debug)]
pub enum SaveOutcome {
    Saved,
    /// A login with this username already exists for the site (left untouched).
    AlreadyExists,
    NotLoggedIn,
    Error(String),
}

/// Locate the npm-global `bw` shim. Returned path is run via `cmd /C` (it's a .cmd batch shim).
fn bw_cmd() -> Option<std::path::PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    let p = std::path::Path::new(&appdata).join("npm").join("bw.cmd");
    p.exists().then_some(p)
}

/// Unlock with the master password, find a credential whose URI matches `url`, return it, then lock.
/// `master_password` is zeroized before the first network/list step regardless of outcome.
pub fn fetch(master_password: &mut String, url: &str) -> FillOutcome {
    let Some(bw) = bw_cmd() else {
        master_password.zeroize();
        return FillOutcome::Error(
            "Bitwarden CLI not found. Install it: npm i -g @bitwarden/cli".into(),
        );
    };

    // 1. Unlock -> session key. Password passed via env (bw's recommended non-interactive path).
    let unlock = Command::new("cmd")
        .creation_flags(CREATE_NO_WINDOW)
        .arg("/C")
        .arg(&bw)
        .args(["unlock", "--passwordenv", "BW_PASSWORD", "--raw"])
        .env("BW_PASSWORD", &*master_password)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    master_password.zeroize();

    let session = match unlock {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() {
                return FillOutcome::Error("Unlock returned an empty session".into());
            }
            s
        }
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr).to_lowercase();
            if err.contains("not logged in") {
                return FillOutcome::NotLoggedIn;
            }
            return FillOutcome::Error(trim_err(&o.stderr, "Unlock failed"));
        }
        Err(e) => return FillOutcome::Error(format!("Couldn't run bw: {e}")),
    };

    // 2. List vault items whose URIs match this site.
    let list = Command::new("cmd")
        .creation_flags(CREATE_NO_WINDOW)
        .arg("/C")
        .arg(&bw)
        .args(["list", "items", "--url", url, "--session", &session])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();

    // 3. Lock again (forget the session) - best effort.
    let _ = Command::new("cmd")
        .creation_flags(CREATE_NO_WINDOW)
        .arg("/C")
        .arg(&bw)
        .arg("lock")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output();

    match list {
        Ok(o) if o.status.success() => match parse_first_login(&String::from_utf8_lossy(&o.stdout))
        {
            Some((u, p)) => FillOutcome::Found {
                username: u,
                password: p,
            },
            None => FillOutcome::NoMatch,
        },
        Ok(o) => FillOutcome::Error(trim_err(&o.stderr, "Lookup failed")),
        Err(e) => FillOutcome::Error(format!("Couldn't run bw: {e}")),
    }
}

/// Save a login to the vault: unlock, skip if a same-username item already exists for this site,
/// otherwise create it, then lock. Both `master_password` and `password` are zeroized before return.
pub fn save(
    master_password: &mut String,
    url: &str,
    username: &str,
    password: &mut String,
) -> SaveOutcome {
    use base64::Engine as _;

    let Some(bw) = bw_cmd() else {
        master_password.zeroize();
        password.zeroize();
        return SaveOutcome::Error(
            "Bitwarden CLI not found. Install it: npm i -g @bitwarden/cli".into(),
        );
    };

    // 1. Unlock -> session key (master password via env, then zeroized).
    let unlock = Command::new("cmd")
        .creation_flags(CREATE_NO_WINDOW)
        .arg("/C")
        .arg(&bw)
        .args(["unlock", "--passwordenv", "BW_PASSWORD", "--raw"])
        .env("BW_PASSWORD", &*master_password)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    master_password.zeroize();

    let session = match unlock {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() {
                password.zeroize();
                return SaveOutcome::Error("Unlock returned an empty session".into());
            }
            s
        }
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr).to_lowercase();
            password.zeroize();
            if err.contains("not logged in") {
                return SaveOutcome::NotLoggedIn;
            }
            return SaveOutcome::Error(trim_err(&o.stderr, "Unlock failed"));
        }
        Err(e) => {
            password.zeroize();
            return SaveOutcome::Error(format!("Couldn't run bw: {e}"));
        }
    };

    // 2. De-dupe: if a login with this username already exists for the site, do nothing.
    if !username.is_empty() {
        let existing = Command::new("cmd")
            .arg("/C")
            .arg(&bw)
            .args(["list", "items", "--url", url, "--session", &session])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output();
        if let Ok(o) = &existing {
            if o.status.success() && username_exists(&String::from_utf8_lossy(&o.stdout), username)
            {
                lock(&bw);
                password.zeroize();
                return SaveOutcome::AlreadyExists;
            }
        }
    }

    // 3. Build the login item, base64-encode it, and create it.
    let item = serde_json::json!({
        "organizationId": null,
        "folderId": null,
        "type": 1,
        "name": host_of(url),
        "notes": null,
        "favorite": false,
        "login": {
            "username": username,
            "password": password.as_str(),
            "uris": [{ "match": null, "uri": url }],
        },
    });
    let mut json = item.to_string();
    password.zeroize();
    let mut encoded = base64::engine::general_purpose::STANDARD.encode(json.as_bytes());
    json.zeroize();

    let create = Command::new("cmd")
        .creation_flags(CREATE_NO_WINDOW)
        .arg("/C")
        .arg(&bw)
        .args(["create", "item", &encoded, "--session", &session])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    encoded.zeroize();
    lock(&bw);

    match create {
        Ok(o) if o.status.success() => SaveOutcome::Saved,
        Ok(o) => SaveOutcome::Error(trim_err(&o.stderr, "Save failed")),
        Err(e) => SaveOutcome::Error(format!("Couldn't run bw: {e}")),
    }
}

/// Lock the vault (forget the session) - best effort.
fn lock(bw: &std::path::Path) {
    let _ = Command::new("cmd")
        .creation_flags(CREATE_NO_WINDOW)
        .arg("/C")
        .arg(bw)
        .arg("lock")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output();
}

/// True if any returned vault item has a login with this username (case-insensitive).
fn username_exists(json: &str, username: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return false;
    };
    let Some(arr) = v.as_array() else {
        return false;
    };
    let target = username.to_lowercase();
    arr.iter().any(|item| {
        item["login"]["username"]
            .as_str()
            .map(|u| u.to_lowercase() == target)
            .unwrap_or(false)
    })
}

/// Bare host (scheme + leading www. stripped), used as the new item's name.
fn host_of(url: &str) -> String {
    let s = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let host = s.split('/').next().unwrap_or(s);
    host.strip_prefix("www.").unwrap_or(host).to_string()
}

fn trim_err(stderr: &[u8], prefix: &str) -> String {
    let msg = String::from_utf8_lossy(stderr);
    let msg = msg.trim();
    if msg.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}: {msg}")
    }
}

/// First vault item that has a login with a username or password.
fn parse_first_login(json: &str) -> Option<(String, String)> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    for item in v.as_array()? {
        let login = &item["login"];
        let u = login["username"].as_str().unwrap_or_default();
        let p = login["password"].as_str().unwrap_or_default();
        if !u.is_empty() || !p.is_empty() {
            return Some((u.to_string(), p.to_string()));
        }
    }
    None
}

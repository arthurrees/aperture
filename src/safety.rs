//! Local safe-browsing: a host blocklist checked at navigation time to warn on known phishing and
//! malware sites. Privacy stance that sets this apart from Chrome's Safe Browsing: the check is a
//! purely LOCAL set lookup, so the addresses you visit are never sent anywhere. Only the blocklist
//! itself is downloaded (a feed of bad hosts), which reveals nothing about your browsing.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::store;

// Default feed (abuse.ch URLhaus malware host list, hosts-file format) is set in
// store::SafetySettings; the user can point it at any hosts-format list in Settings.

/// Shared, cheap-to-clone safe-browsing state. Blocklist + a session bypass (hosts the user chose
/// to visit anyway) + an on/off flag, all behind atomics/locks so the nav handler can consult it
/// and a worker thread can swap in an updated list.
#[derive(Clone)]
pub struct SafeBrowsing {
    blocklist: Arc<Mutex<HashSet<String>>>,
    bypass: Arc<Mutex<HashSet<String>>>,
    enabled: Arc<AtomicBool>,
}

impl SafeBrowsing {
    /// Build from the bundled seed list plus any downloaded cache in the data dir.
    pub fn load(enabled: bool) -> Self {
        let mut set = parse_list(include_str!("ui/safebrowse.txt"));
        if let Ok(cached) = std::fs::read_to_string(cache_path()) {
            set.extend(parse_list(&cached));
        }
        SafeBrowsing {
            blocklist: Arc::new(Mutex::new(set)),
            bypass: Arc::new(Mutex::new(HashSet::new())),
            enabled: Arc::new(AtomicBool::new(enabled)),
        }
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    /// Whether a navigation to `host` should be stopped: safe-browsing is on, the host (or a
    /// parent domain) is listed, and the user hasn't chosen to bypass it this session.
    pub fn is_blocked(&self, host: &str) -> bool {
        if !self.enabled.load(Ordering::Relaxed) {
            return false;
        }
        let host = normalize(host);
        if host.is_empty() {
            return false;
        }
        if self.bypass.lock().unwrap().contains(&host) {
            return false;
        }
        let list = self.blocklist.lock().unwrap();
        // Exact host, or any parent domain (so "login.evil.com" is caught by "evil.com").
        candidate_domains(&host).iter().any(|d| list.contains(d))
    }

    /// Let this host through for the rest of the session (the interstitial's "continue anyway").
    pub fn bypass_host(&self, host: &str) {
        let host = normalize(host);
        if !host.is_empty() {
            self.bypass.lock().unwrap().insert(host);
        }
    }

    pub fn count(&self) -> usize {
        self.blocklist.lock().unwrap().len()
    }

    /// Replace the blocklist with a freshly parsed list (seed always kept as a floor).
    pub fn replace(&self, text: &str) {
        let mut set = parse_list(include_str!("ui/safebrowse.txt"));
        set.extend(parse_list(text));
        *self.blocklist.lock().unwrap() = set;
    }
}

fn cache_path() -> std::path::PathBuf {
    store::data_dir().join("safebrowse.txt")
}

/// Fetch the feed, cache it to disk, and return its text. Blocking; call from a worker thread.
pub fn fetch(feed_url: &str) -> Result<String, String> {
    // TLS via native-tls (SChannel), matching search.rs: with default-features off, a bare
    // ureq::get has no TLS backend, so we must build an agent with the connector.
    let tls = native_tls::TlsConnector::new().map_err(|e| format!("TLS init failed: {e}"))?;
    let agent = ureq::builder()
        .tls_connector(std::sync::Arc::new(tls))
        .timeout(std::time::Duration::from_secs(20))
        .build();
    // A browser-like User-Agent: some feed hosts (abuse.ch is behind Cloudflare) reject a
    // default library UA.
    let text = agent
        .get(feed_url)
        .set(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Aperture/0.1 safe-browsing",
        )
        .call()
        .map_err(|e| format!("download failed: {e}"))?
        .into_string()
        .map_err(|e| format!("read failed: {e}"))?;
    if text.trim().is_empty() {
        return Err("feed was empty".into());
    }
    let _ = std::fs::write(cache_path(), &text);
    Ok(text)
}

/// True if the cache is missing or older than a day (so launch can refresh in the background).
pub fn cache_is_stale() -> bool {
    match std::fs::metadata(cache_path()).and_then(|m| m.modified()) {
        Ok(modified) => modified
            .elapsed()
            .map(|e| e.as_secs() > 86_400)
            .unwrap_or(true),
        Err(_) => true,
    }
}

/// Lowercase, trim, drop a leading "www.".
fn normalize(host: &str) -> String {
    host.trim().to_lowercase().trim_start_matches("www.").to_string()
}

/// The host and each of its parent domains, e.g. "a.b.example.com" ->
/// ["a.b.example.com", "b.example.com", "example.com"]. Stops at the last two labels.
fn candidate_domains(host: &str) -> Vec<String> {
    let labels: Vec<&str> = host.split('.').collect();
    let n = labels.len();
    (0..n.saturating_sub(1)).map(|i| labels[i..].join(".")).collect()
}

/// Parse a blocklist: plain hosts, one per line, and hosts-file format ("0.0.0.0 evil.com"),
/// skipping comments, blanks, and the loopback placeholders.
fn parse_list(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        // Hosts-file rows are "<ip> <host>"; a bare line is just "<host>".
        let host = line.split_whitespace().last().unwrap_or("");
        let host = normalize(host);
        if host.is_empty()
            || host == "localhost"
            || host == "0.0.0.0"
            || host == "broadcasthost"
            || host.parse::<std::net::IpAddr>().is_ok()
            || !host.contains('.')
        {
            continue;
        }
        out.insert(host);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hosts_and_plain_lines() {
        let s = parse_list(
            "# comment\n0.0.0.0 evil.com\n127.0.0.1 bad.example.org\nplain-phish.net\nlocalhost\n\n  www.Mixed.COM  ",
        );
        assert!(s.contains("evil.com"));
        assert!(s.contains("bad.example.org"));
        assert!(s.contains("plain-phish.net"));
        assert!(s.contains("mixed.com")); // lowercased, www stripped
        assert!(!s.contains("localhost"));
        assert_eq!(s.len(), 4);
    }

    #[test]
    fn blocks_host_and_subdomains_respecting_bypass() {
        let sb = SafeBrowsing {
            blocklist: Arc::new(Mutex::new(parse_list("evil.com"))),
            bypass: Arc::new(Mutex::new(HashSet::new())),
            enabled: Arc::new(AtomicBool::new(true)),
        };
        assert!(sb.is_blocked("evil.com"));
        assert!(sb.is_blocked("login.evil.com")); // parent-domain match
        assert!(sb.is_blocked("www.evil.com"));
        assert!(!sb.is_blocked("notevil.com"));
        assert!(!sb.is_blocked("good.org"));
        sb.bypass_host("evil.com");
        assert!(!sb.is_blocked("evil.com")); // bypassed this session
        sb.set_enabled(false);
        assert!(!sb.is_blocked("login.evil.com")); // disabled -> never blocks
    }
}

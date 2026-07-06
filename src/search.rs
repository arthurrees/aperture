//! Built-in metasearch. Fetches DuckDuckGo's no-JS "lite" page and parses the organic results, so
//! Aperture can render them ad-free in its own page. This is HTML scraping of a stable-ish endpoint:
//! it can break if DDG changes its markup or rate-limits. Always run on a BACKGROUND thread (blocking
//! I/O) - never on the event loop.

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

#[derive(Serialize)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Fetch + parse results for a query. Returns an empty list on any network/TLS/parse failure.
pub fn search(query: &str) -> Vec<Hit> {
    let Ok(tls) = native_tls::TlsConnector::new() else {
        return Vec::new();
    };
    let agent = ureq::builder().tls_connector(Arc::new(tls)).build();
    let url = format!("https://lite.duckduckgo.com/lite/?q={}", urlencode(query));
    let body = match agent
        .get(&url)
        .set("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64)")
        .timeout(Duration::from_secs(12))
        .call()
    {
        Ok(resp) => resp.into_string().unwrap_or_default(),
        Err(_) => return Vec::new(),
    };
    parse(&body)
}

fn parse(html: &str) -> Vec<Hit> {
    use regex::Regex;
    // <a ... href="//duckduckgo.com/l/?uddg=<encoded real url>&rut=..." class='result-link'>TITLE</a>
    let link_re = Regex::new(
        r#"(?s)href="//duckduckgo\.com/l/\?uddg=([^&"]+)[^"]*"\s+class=['"]result-link['"]>(.*?)</a>"#,
    )
    .unwrap();
    let snip_re = Regex::new(r#"(?s)class=['"]result-snippet['"]>(.*?)</td>"#).unwrap();
    // Snippets with their byte offset, in document order. We pair each link with the snippet that
    // follows it in the document, NOT by sharing an index: a result without a snippet, or a skipped
    // link, would otherwise shift every later link onto the wrong snippet (silent mis-citation).
    let snippets: Vec<(usize, String)> = snip_re
        .captures_iter(html)
        .filter_map(|c| Some((c.get(0)?.start(), clean(c.get(1)?.as_str()))))
        .collect();
    let mut hits = Vec::new();
    let mut scur = 0usize;
    let mut links = link_re.captures_iter(html).peekable();
    while let Some(c) = links.next() {
        let link_start = c.get(0).map(|m| m.start()).unwrap_or(0);
        let next_link_start = links
            .peek()
            .and_then(|n| n.get(0))
            .map(|m| m.start())
            .unwrap_or(usize::MAX);
        // The snippet for this result is the first one between this link and the next; if none falls in
        // that window the result simply has no snippet (rather than borrowing a neighbour's).
        while scur < snippets.len() && snippets[scur].0 < link_start {
            scur += 1;
        }
        let snippet = if scur < snippets.len() && snippets[scur].0 < next_link_start {
            let s = snippets[scur].1.clone();
            scur += 1;
            s
        } else {
            String::new()
        };
        let url = percent_decode(&c[1]);
        let title = clean(&c[2]);
        if title.is_empty() || !url.starts_with("http") {
            continue;
        }
        hits.push(Hit { title, url, snippet });
        if hits.len() >= 20 {
            break;
        }
    }
    if hits.is_empty() && !html.trim().is_empty() && std::env::var("APERTURE_DEBUG_SEARCH").is_ok() {
        eprintln!("search: 0 results parsed from {} bytes - DDG markup may have changed", html.len());
    }
    hits
}

/// Strip HTML tags, unescape the common entities, trim.
fn clean(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .trim()
        .to_string()
}

/// Decode %XX sequences (used for the uddg-wrapped real URL and the query in the page hash).
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Fetch a page over HTTPS and return its readable text: script/style/head/svg blocks dropped, tags
/// removed, entities unescaped, whitespace collapsed. Empty on any failure. Background-thread only.
pub fn fetch_readable(url: &str) -> String {
    let Ok(tls) = native_tls::TlsConnector::new() else {
        return String::new();
    };
    let agent = ureq::builder()
        .tls_connector(Arc::new(tls))
        .timeout(Duration::from_secs(12))
        .build();
    let html = match agent
        .get(url)
        .set("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64)")
        .call()
    {
        Ok(resp) => resp.into_string().unwrap_or_default(),
        Err(_) => return String::new(),
    };
    html_to_text(&html)
}

fn html_to_text(html: &str) -> String {
    use regex::Regex;
    // Drop blocks whose text content isn't readable page content.
    let stripped = Regex::new(r"(?is)<(script|style|noscript|head|svg)\b[^>]*>.*?</\s*\1\s*>")
        .map(|re| re.replace_all(html, " ").into_owned())
        .unwrap_or_else(|_| html.to_string());
    let text = clean(&stripped); // strip remaining tags + unescape common entities
    // Collapse runs of whitespace to single spaces.
    let mut out = String::with_capacity(text.len());
    let mut prev_ws = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    out.trim().to_string()
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            b' ' => "+".to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

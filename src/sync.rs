//! Cross-device transport over Tailscale. A tiny HTTP listener bound to this machine's tailnet
//! IP accepts "open this URL" requests from peer Aperture devices, gated by a shared token. The
//! send side POSTs to a peer's tailnet address. This is the foundation for send-tab-to-device
//! (and, later, store sync).
//!
//! Scope note: the listener binds ONLY to the Tailscale interface (never 0.0.0.0), so the port is
//! not reachable from the local LAN or the internet, and every request must still carry the token.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

/// This machine's Tailscale IPv4, via the `tailscale` CLI. None if Tailscale isn't installed or
/// the node is logged out (in which case the receiver can't start, but sending still works).
pub fn tailscale_ip() -> Option<Ipv4Addr> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let candidates = [
        "tailscale",
        r"C:\Program Files\Tailscale\tailscale.exe",
        r"C:\Program Files (x86)\Tailscale\tailscale.exe",
    ];
    for exe in candidates {
        let out = std::process::Command::new(exe)
            .args(["ip", "-4"])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        if let Ok(out) = out {
            if out.status.success() {
                if let Some(ip) = String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .map(str::trim)
                    .find_map(|l| l.parse::<Ipv4Addr>().ok())
                {
                    return Some(ip);
                }
            }
        }
    }
    None
}

/// Start the listener on a background thread. `on_open` is called with a received URL (already
/// token-checked); the caller wires it to the run loop. Returns immediately; if the socket can't
/// bind, the thread just exits (receiving is unavailable, sending still works).
pub fn spawn_listener<F>(ip: Ipv4Addr, port: u16, token: String, on_open: F)
where
    F: Fn(String) + Send + 'static,
{
    std::thread::spawn(move || {
        let Ok(listener) = TcpListener::bind(SocketAddr::from((ip, port))) else {
            eprintln!("[sync] could not bind {ip}:{port}; receiver disabled");
            return;
        };
        eprintln!("[sync] listening on {ip}:{port}");
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            if let Some(url) = handle_conn(stream, &token) {
                on_open(url);
            }
        }
    });
}

/// Read one request, validate method/path/token, reply, and return the URL to open (if any).
fn handle_conn(mut stream: TcpStream, token: &str) -> Option<String> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    // Read until end of headers, then the declared body.
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let mut header_end = None;
    while header_end.is_none() {
        let Ok(n) = stream.read(&mut chunk) else {
            return None;
        };
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        header_end = find_header_end(&buf);
        if buf.len() > 64 * 1024 {
            return None; // oversized; drop
        }
    }
    let header_end = header_end?;
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or_default();

    // Health check: any peer can confirm reachability, no token needed, no side effect.
    if method == "GET" && path == "/ping" {
        let _ = stream.write_all(reply(200, "aperture").as_bytes());
        return None;
    }
    if method != "POST" || path != "/open" {
        let _ = stream.write_all(reply(404, "not found").as_bytes());
        return None;
    }

    let mut got_token = String::new();
    let mut content_length = 0usize;
    for line in lines {
        if let Some(v) = header_value(line, "x-aperture-token") {
            got_token = v.to_string();
        } else if let Some(v) = header_value(line, "content-length") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    if token.is_empty() || got_token != token {
        let _ = stream.write_all(reply(401, "unauthorized").as_bytes());
        return None;
    }

    // Read the rest of the body up to content-length.
    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let Ok(n) = stream.read(&mut chunk) else { break };
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);

    let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
    let url = v["url"].as_str().unwrap_or_default().to_string();
    if url.starts_with("http://") || url.starts_with("https://") {
        let _ = stream.write_all(reply(200, "ok").as_bytes());
        Some(url)
    } else {
        let _ = stream.write_all(reply(400, "bad url").as_bytes());
        None
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// Case-insensitive "Name: value" match; returns the trimmed value.
fn header_value<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let (k, v) = line.split_once(':')?;
    k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
}

fn reply(code: u16, body: &str) -> String {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        _ => "Not Found",
    };
    format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Send a URL to a peer. `address` is a tailnet name or 100.x IP (no scheme/port). Blocking;
/// call from a worker thread. Returns Ok on a 2xx, else a short error for the toast.
pub fn send_open(address: &str, port: u16, token: &str, url: &str, from_device: &str) -> Result<(), String> {
    let endpoint = format!("http://{address}:{port}/open");
    let body = serde_json::json!({ "url": url, "from": from_device }).to_string();
    match ureq::post(&endpoint)
        .timeout(Duration::from_secs(6))
        .set("X-Aperture-Token", token)
        .set("Content-Type", "application/json")
        .send_string(&body)
    {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(401, _)) => Err("token mismatch on the other device".into()),
        Err(ureq::Error::Status(code, _)) => Err(format!("device replied {code}")),
        Err(_) => Err("device unreachable (is Aperture open there?)".into()),
    }
}

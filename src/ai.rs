//! Local AI layer. Talks to Ollama on 127.0.0.1:11434 over plain loopback HTTP (no TLS): a chat model
//! (default qwen3.5-gpu) plus nomic-embed-text for embeddings. Everything here is synchronous and
//! blocking - callers run it on a worker thread and forward results back to the run loop as UserEvents.
//! The model runs locally; web access is a separate capability (Aperture fetches real pages itself, see
//! the web-search path), so a prompt only leaves the machine when the user explicitly runs a web search.

use serde::Deserialize;
use std::io::{BufRead, BufReader};
use std::time::Duration;

// Host, model, and keep_alive are all configurable (store::AiSettings) and passed in by callers, so
// the project works against any Ollama instance / model without code changes.

/// A chat message. `role` is "system" | "user" | "assistant". `images` carries base64-encoded PNGs
/// for vision requests (Ollama reads them off the message); omitted from the wire when empty.
#[derive(serde::Serialize, Clone)]
pub struct Msg {
    pub role: &'static str,
    pub content: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
}

impl Msg {
    pub fn system(content: impl Into<String>) -> Self {
        Msg { role: "system", content: content.into(), images: Vec::new() }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Msg { role: "user", content: content.into(), images: Vec::new() }
    }
    /// A prior assistant turn, replayed as conversation history so follow-ups have context.
    pub fn assistant(content: impl Into<String>) -> Self {
        Msg { role: "assistant", content: content.into(), images: Vec::new() }
    }
    /// A user message carrying one or more base64 PNG screenshots (vision fallback).
    pub fn user_image(content: impl Into<String>, images: Vec<String>) -> Self {
        Msg { role: "user", content: content.into(), images }
    }
}

fn agent() -> ureq::Agent {
    // Generous connect timeout; NO read timeout, so a long stream isn't cut off mid-answer.
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(4))
        .build()
}

/// Turn a ureq error into a short, human-readable line for the sidebar.
fn friendly(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, _) => format!("Ollama returned HTTP {code}"),
        ureq::Error::Transport(t) => {
            format!("can't reach Ollama - is it running, and is the host correct in Settings? ({t})")
        }
    }
}

/// List the models installed in the Ollama instance at `host` (for the settings dropdown). Empty on
/// failure (e.g. Ollama not running).
pub fn list_models(host: &str) -> Vec<String> {
    let Ok(resp) = agent().get(&format!("{host}/api/tags")).call() else {
        return Vec::new();
    };
    #[derive(Deserialize)]
    struct Model {
        name: String,
    }
    #[derive(Deserialize)]
    struct Tags {
        #[serde(default)]
        models: Vec<Model>,
    }
    match resp.into_json::<Tags>() {
        Ok(t) => t.models.into_iter().map(|m| m.name).collect(),
        Err(_) => Vec::new(),
    }
}

/// Stream a chat completion from `model` on the Ollama instance at `host`. Invokes `on_token` for each
/// content delta as it arrives, and polls `cancel` between lines (returning true abandons the stream).
/// Blocks until done or error. `think:false` keeps qwen out of reasoning mode (user preference).
pub fn chat_stream(
    host: &str,
    model: &str,
    keep_alive: &str,
    messages: &[Msg],
    mut on_token: impl FnMut(&str),
    cancel: impl Fn() -> bool,
) -> Result<(), String> {
    let body = serde_json::json!({
        "model": model,
        "messages": messages,
        "stream": true,
        "think": false,
        "keep_alive": keep_alive,
    });
    let resp = agent()
        .post(&format!("{host}/api/chat"))
        .send_json(body)
        .map_err(friendly)?;

    #[derive(Deserialize)]
    struct ChunkMsg {
        #[serde(default)]
        content: String,
    }
    #[derive(Deserialize)]
    struct Chunk {
        message: Option<ChunkMsg>,
        #[serde(default)]
        done: bool,
    }

    let reader = BufReader::new(resp.into_reader());
    let mut saw_done = false;
    let mut parse_failures = 0u32;
    for line in reader.lines() {
        if cancel() {
            // User abandoned the stream - that's not an error.
            return Ok(());
        }
        let line = line.map_err(|e| e.to_string())?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Chunk>(line) {
            Ok(chunk) => {
                if let Some(m) = chunk.message {
                    if !m.content.is_empty() {
                        on_token(&m.content);
                    }
                }
                if chunk.done {
                    saw_done = true;
                    break;
                }
            }
            Err(_) => parse_failures += 1,
        }
    }
    // Ollama always closes a successful chat with a done:true chunk. If we never saw one, the stream was
    // truncated or returned an unexpected format; surface that instead of silently showing a short answer.
    if !saw_done {
        if std::env::var("APERTURE_DEBUG_AI").is_ok() {
            eprintln!("ai: stream ended without done (parse_failures={parse_failures})");
        }
        return Err("the model stream ended early or returned an unexpected format".to_string());
    }
    Ok(())
}

/// Tell Ollama to unload a model from VRAM right now (keep_alive 0). Best-effort and blocking, so
/// call it off the UI thread. Used when the user closes the AI sidebar, to free the GPU at once.
pub fn unload(host: &str, model: &str) {
    let body = serde_json::json!({ "model": model, "keep_alive": 0 });
    let _ = agent().post(&format!("{host}/api/generate")).send_json(body);
}

/// Embed a single string with `model` (nomic-embed-text). Reserved for a future semantic-memory
/// feature; not wired into the UI.
#[allow(dead_code)]
pub fn embed(host: &str, model: &str, input: &str) -> Result<Vec<f32>, String> {
    let body = serde_json::json!({ "model": model, "input": input });
    let resp = agent()
        .post(&format!("{host}/api/embed"))
        .send_json(body)
        .map_err(friendly)?;
    #[derive(Deserialize)]
    struct EmbedResp {
        #[serde(default)]
        embeddings: Vec<Vec<f32>>,
    }
    let parsed: EmbedResp = resp.into_json().map_err(|e| e.to_string())?;
    parsed
        .embeddings
        .into_iter()
        .next()
        .ok_or_else(|| "Ollama returned no embedding".to_string())
}

/// Cosine similarity between two equal-length vectors (semantic-memory ranking).
#[allow(dead_code)]
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

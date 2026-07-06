// Hide the console window in release builds (keep it in debug for logs / BROWSER_DEBUG).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// v1 (memory) - tabs with a three-tier memory lifecycle.
//   * one tao window; a "chrome" webview (tab strip + omnibox, src/ui/chrome.html) docked on top
//   * N "content" webviews (one per tab); only the active one is visible
//   * DIM    : backgrounded tabs drop to WebView2 Low memory level immediately
//   * SLEEP  : after one idle interval, backgrounded tabs are TrySuspend-ed (CPU paused, memory
//              reclaimable; instant auto-resume when shown)
//   * DISCARD: still idle after another interval -> the WebView is dropped, killing its renderer
//              process for a real, immediate RAM reclaim. Recreated (URL reloaded) when reactivated.
//
// The DISCARD tier is what actually frees RAM on a machine with no memory pressure (sleep only makes
// memory reclaimable). Recreating on activation loses in-page state, the documented discard tradeoff,
// so it only happens after a tab has been idle through a sleep cycle first.
//
// Chrome webview and content webviews never touch each other directly: handlers post UserEvents and
// the run loop is the single owner that mutates window / chrome / Vec<Tab>. Content is untrusted.

use tao::{
    event::{Event, StartCause, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy},
    platform::windows::IconExtWindows,
    window::{Icon, Window, WindowBuilder},
};
use webview2_com::{
    AcceleratorKeyPressedEventHandler, CapturePreviewCompletedHandler,
    DocumentTitleChangedEventHandler, DownloadStartingEventHandler, FaviconChangedEventHandler,
    GetCookiesCompletedHandler, IsDocumentPlayingAudioChangedEventHandler,
    Microsoft::Web::WebView2::Win32::{
        ICoreWebView2Deferral, ICoreWebView2PermissionRequestedEventArgs,
        ICoreWebView2PermissionRequestedEventArgs2, ICoreWebView2Profile7,
        ICoreWebView2Settings4, ICoreWebView2_13, ICoreWebView2_15,
        ICoreWebView2_16, ICoreWebView2_2, ICoreWebView2_3, ICoreWebView2_4, ICoreWebView2_8,
        COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG, COREWEBVIEW2_DOWNLOAD_STATE,
        COREWEBVIEW2_DOWNLOAD_STATE_COMPLETED, COREWEBVIEW2_DOWNLOAD_STATE_INTERRUPTED,
        COREWEBVIEW2_KEY_EVENT_KIND, COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN,
        COREWEBVIEW2_KEY_EVENT_KIND_SYSTEM_KEY_DOWN, COREWEBVIEW2_PERMISSION_KIND,
        COREWEBVIEW2_PERMISSION_STATE_ALLOW, COREWEBVIEW2_PERMISSION_STATE_DENY,
        COREWEBVIEW2_PRINT_DIALOG_KIND_BROWSER,
    },
    PermissionRequestedEventHandler, ProfileGetBrowserExtensionsCompletedHandler,
    StateChangedEventHandler, TrySuspendCompletedHandler, ZoomFactorChangedEventHandler,
};
use base64::Engine as _;
use windows::Win32::Foundation::HGLOBAL;
use windows::Win32::System::Com::StructuredStorage::{
    CreateStreamOnHGlobal, GetHGlobalFromStream,
};
use windows::Win32::System::Com::{CoTaskMemFree, IStream};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState;
use windows_core::Interface;
use wry::{
    dpi::{LogicalPosition, LogicalSize},
    MemoryUsageLevel, NewWindowResponse, Rect, WebContext, WebView, WebViewBuilder,
    WebViewBuilderExtWindows, WebViewExtWindows,
};

mod ai;
mod bitwarden;
mod blocker;
mod search;
mod store;
mod webauthn;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::Arc;

/// Monotonic id for each download, so the chrome popover can track items across state changes.
static DL_COUNTER: AtomicU32 = AtomicU32::new(1);

use zeroize::Zeroize;

/// Injected at document-start in every TOP frame. Currently just the Chrome-style hover-link status
/// bar. (Password saving is handled by WebView2's built-in manager, so we no longer capture login
/// submits here - that avoided a double "save?" prompt.)
const CAPTURE_JS: &str = r#"(function(){
  if (window.top !== window) return;
  if (window.__apInit) return; window.__apInit = 1;

  // Hover-link status bar (Chrome-style, bottom-left). Pure in-page, no host involvement.
  var bar = null;
  function ensureBar(){
    if(bar) return bar;
    bar = document.createElement('div');
    bar.style.cssText = 'position:fixed;left:0;bottom:0;z-index:2147483647;max-width:70vw;'
      + 'overflow:hidden;text-overflow:ellipsis;white-space:nowrap;pointer-events:none;'
      + 'font:12px/1.7 ui-monospace,Consolas,monospace;color:#d2d7df;background:#14171c;'
      + 'border:1px solid #232830;border-radius:0 6px 0 0;padding:1px 10px;display:none';
    (document.body || document.documentElement).appendChild(bar);
    return bar;
  }
  document.addEventListener('mouseover', function(e){
    var a = (e.target && e.target.closest) ? e.target.closest('a[href]') : null;
    if(a && a.href){ var b = ensureBar(); b.textContent = a.href; b.style.display = 'block'; }
  }, true);
  document.addEventListener('mouseout', function(e){
    var a = (e.target && e.target.closest) ? e.target.closest('a[href]') : null;
    if(a && bar){ bar.style.display = 'none'; }
  }, true);

  // --- AI selection pill (rung 2): selecting text offers Explain / Translate / Rewrite. Posts a
  // non-privileged hint (text + action) to the host, which routes it to the local AI sidebar. ---
  var aiPill = null;
  function hidePill(){ if(aiPill) aiPill.style.display = 'none'; }
  function ensurePill(){
    if(aiPill) return aiPill;
    aiPill = document.createElement('div');
    aiPill.style.cssText = 'position:fixed;z-index:2147483647;display:none;gap:2px;'
      + 'background:#14171c;border:1px solid #2a313b;border-radius:8px;padding:3px;'
      + 'box-shadow:0 6px 20px rgba(0,0,0,.45);';
    var mk = function(label, action){
      var b = document.createElement('button');
      b.textContent = label;
      b.style.cssText = 'border:0;background:transparent;color:#d2d7df;padding:5px 9px;'
        + 'border-radius:6px;cursor:pointer;font:12px/1 "Segoe UI",sans-serif;';
      b.addEventListener('mouseenter', function(){ b.style.background = '#222831'; });
      b.addEventListener('mouseleave', function(){ b.style.background = 'transparent'; });
      // Keep the selection alive: don't let mousedown clear it or bubble to the dismiss handler.
      b.addEventListener('mousedown', function(e){ e.preventDefault(); e.stopPropagation(); });
      b.addEventListener('click', function(e){
        e.preventDefault(); e.stopPropagation();
        var sel = (window.getSelection && window.getSelection().toString()) || '';
        sel = sel.trim();
        if(sel){ try{ window.ipc.postMessage(JSON.stringify({ cmd:'ai_selection', action:action, text:sel.slice(0,8000) })); }catch(_){} }
        hidePill();
      });
      return b;
    };
    aiPill.appendChild(mk('Explain','explain'));
    aiPill.appendChild(mk('Translate','translate'));
    aiPill.appendChild(mk('Rewrite','rewrite'));
    (document.body || document.documentElement).appendChild(aiPill);
    return aiPill;
  }
  function showPillForSelection(){
    if(window.__apPill === false){ hidePill(); return; } // disabled in settings
    var s = window.getSelection && window.getSelection();
    if(!s || s.isCollapsed){ hidePill(); return; }
    var txt = s.toString().trim();
    if(txt.length < 2){ hidePill(); return; }
    var rng; try{ rng = s.getRangeAt(0); }catch(_){ return; }
    var r = rng.getBoundingClientRect();
    if(!r || (!r.width && !r.height)){ hidePill(); return; }
    var p = ensurePill();
    p.style.display = 'flex';
    var top = r.top - p.offsetHeight - 8;
    if(top < 6) top = r.bottom + 8;
    var left = r.left;
    var maxL = window.innerWidth - p.offsetWidth - 8;
    if(left > maxL) left = maxL;
    if(left < 6) left = 6;
    p.style.top = top + 'px';
    p.style.left = left + 'px';
  }
  document.addEventListener('mouseup', function(){ setTimeout(showPillForSelection, 10); }, true);
  document.addEventListener('keyup', function(e){ if(e.shiftKey || e.ctrlKey){ setTimeout(showPillForSelection, 10); } }, true);
  document.addEventListener('mousedown', function(e){ if(aiPill && !aiPill.contains(e.target)) hidePill(); }, true);
  document.addEventListener('scroll', hidePill, true);
  document.addEventListener('keydown', function(e){ if(e.key === 'Escape') hidePill(); }, true);

  // --- Bitwarden-backed passkey provider (WebAuthn assertion) ---
  // The Bitwarden extension can't run its passkey popout in WebView2, so we intercept
  // navigator.credentials.get(), have the host sign the challenge with the vault passkey, and hand
  // back a credential. https top-frame only. If the host has no matching passkey it tells us to fall
  // back to the native call, so non-Bitwarden flows are untouched.
  if (location.protocol === 'https:' && navigator.credentials && navigator.credentials.get) {
    var realGet = navigator.credentials.get.bind(navigator.credentials);
    var waPending = {};
    var waSeq = 0;
    var b64uDec = function(s){ s = String(s).replace(/-/g,'+').replace(/_/g,'/'); while(s.length%4) s+='='; var bin=atob(s); var u=new Uint8Array(bin.length); for(var i=0;i<bin.length;i++) u[i]=bin.charCodeAt(i); return u.buffer; };
    var b64uEnc = function(buf){ var u=new Uint8Array(buf); var s=''; for(var i=0;i<u.length;i++) s+=String.fromCharCode(u[i]); return btoa(s).replace(/\+/g,'-').replace(/\//g,'_').replace(/=+$/,''); };
    window.__apWaResolve = function(id, a){
      var p = waPending[id]; if(!p) return; delete waPending[id];
      try {
        var cred = {
          id: a.credentialId, rawId: b64uDec(a.credentialId), type: 'public-key',
          authenticatorAttachment: 'cross-platform',
          response: {
            authenticatorData: b64uDec(a.authenticatorData),
            clientDataJSON: b64uDec(a.clientDataJSON),
            signature: b64uDec(a.signature),
            userHandle: a.userHandle ? b64uDec(a.userHandle) : null
          },
          getClientExtensionResults: function(){ return {}; }
        };
        try { Object.setPrototypeOf(cred, PublicKeyCredential.prototype); } catch(e){}
        p.resolve(cred);
      } catch(e){ p.reject(e); }
    };
    window.__apWaReject = function(id, msg){ var p=waPending[id]; if(!p) return; delete waPending[id]; p.reject(new DOMException(msg||'Passkey failed','NotAllowedError')); };
    window.__apWaFallback = function(id){ var p=waPending[id]; if(!p) return; delete waPending[id]; realGet(p.opts).then(p.resolve, p.reject); };
    var ourGet = function(opts){
      if(!opts || !opts.publicKey || !opts.publicKey.challenge){ return realGet(opts); }
      var pk = opts.publicKey;
      var id = 'wa'+(++waSeq);
      var allow = (pk.allowCredentials||[]).map(function(c){ return b64uEnc(c.id); });
      return new Promise(function(resolve, reject){
        waPending[id] = { resolve: resolve, reject: reject, opts: opts };
        try {
          window.ipc.postMessage(JSON.stringify({ cmd:'webauthn_get', reqId:id,
            rpId: pk.rpId || location.hostname,
            challenge: b64uEnc(pk.challenge), allow: allow }));
        } catch(e){ delete waPending[id]; realGet(opts).then(resolve, reject); }
      });
    };
    // We run at document-creation, BEFORE the Bitwarden extension's page script - which also
    // overrides navigator.credentials.get (and would win a plain assignment race, then open its
    // popout that can't render here). Lock our override non-writable/non-configurable so the
    // extension's later assignment / defineProperty can't replace it. Passwords are untouched
    // (the extension doesn't autofill via navigator.credentials).
    try {
      Object.defineProperty(navigator.credentials, 'get', { value: ourGet, writable: false, configurable: false, enumerable: true });
    } catch(e) {
      try { navigator.credentials.get = ourGet; } catch(e2){}
    }
  }
})();"#;

/// Reader mode: extract the main article from the active page and re-render it with Aperture's
/// reading styles. Host-injected into the content webview (the page never drives this). Toggling it
/// again reloads the page to restore the original. A heuristic, not full Readability: it scores
/// containers by the text length of their paragraphs and keeps the densest one.
const READER_JS: &str = r#"(function(){
  if (window.__apReader) { location.reload(); return; }
  try {
    function score(el){ var ps = el.getElementsByTagName('p'); var n=0;
      for (var i=0;i<ps.length;i++){ n += (ps[i].innerText||'').length; } return n; }
    var best = document.body, bestScore = 0;
    var cands = document.querySelectorAll('article, main, [role=main], section, div');
    for (var i=0;i<cands.length;i++){ var s = score(cands[i]); if (s > bestScore){ bestScore = s; best = cands[i]; } }
    if (bestScore < 250 || !best) { return; }
    var title = '';
    var h1 = document.querySelector('h1'); if (h1) title = h1.innerText || '';
    if (!title) title = document.title || '';
    var clone = best.cloneNode(true);
    clone.querySelectorAll('script,style,noscript,iframe,form,nav,aside,header,footer,button,svg,video,[role=navigation],[aria-hidden=true]').forEach(function(n){ n.remove(); });
    clone.querySelectorAll('*').forEach(function(n){ n.removeAttribute('style'); n.removeAttribute('class'); n.removeAttribute('id'); n.removeAttribute('width'); n.removeAttribute('height'); });
    var css = "html,body{margin:0;background:#14171c;color:#d6dae0;}"
      + ".ap-rd{max-width:720px;margin:0 auto;padding:64px 24px 140px;font:18px/1.75 Georgia,'Times New Roman',serif;}"
      + ".ap-rd h1{font:600 31px/1.25 'Segoe UI',-apple-system,sans-serif;color:#fff;margin:0 0 10px;}"
      + ".ap-rd .ap-by{font:13px 'Segoe UI',sans-serif;color:#7f8893;margin:0 0 36px;padding-bottom:24px;border-bottom:1px solid #232830;}"
      + ".ap-rd h2{font:600 23px/1.3 'Segoe UI',sans-serif;color:#fff;margin:40px 0 12px;}"
      + ".ap-rd h3{font:600 19px/1.3 'Segoe UI',sans-serif;color:#eef;margin:30px 0 10px;}"
      + ".ap-rd p{margin:0 0 22px;}.ap-rd a{color:#5b9cff;text-decoration:none;}.ap-rd a:hover{text-decoration:underline;}"
      + ".ap-rd img{max-width:100%;height:auto;border-radius:8px;margin:14px 0;display:block;}"
      + ".ap-rd pre{background:#0d0f13;padding:14px 16px;border-radius:8px;overflow:auto;font:14px/1.55 ui-monospace,Consolas,monospace;}"
      + ".ap-rd code{font:14px ui-monospace,Consolas,monospace;background:#0d0f13;padding:1px 5px;border-radius:4px;}"
      + ".ap-rd pre code{background:none;padding:0;}"
      + ".ap-rd blockquote{margin:22px 0;padding:2px 0 2px 18px;border-left:3px solid #5b9cff;color:#aeb4bd;}"
      + ".ap-rd ul,.ap-rd ol{padding-left:24px;margin:0 0 22px;}.ap-rd li{margin:7px 0;}"
      + ".ap-rd figure{margin:18px 0;}.ap-rd figcaption{font:13px 'Segoe UI',sans-serif;color:#7f8893;margin-top:6px;}";
    document.head.innerHTML = '';
    var st = document.createElement('style'); st.textContent = css; document.head.appendChild(st);
    document.body.innerHTML = '';
    document.body.removeAttribute('class'); document.body.removeAttribute('style');
    var wrap = document.createElement('div'); wrap.className = 'ap-rd';
    var h = document.createElement('h1'); h.textContent = title; wrap.appendChild(h);
    var host = location.host || ''; if (host){ var by = document.createElement('div'); by.className='ap-by'; by.textContent = host; wrap.appendChild(by); }
    wrap.appendChild(clone);
    document.body.appendChild(wrap);
    window.scrollTo(0,0);
    window.__apReader = true;
  } catch(e){}
})();"#;

/// Find-in-page: highlight all matches of a query on the page and report {count, index}. Defined
/// once per page (idempotent); the host appends a `window.__apFind.search(q, forward)` call and reads
/// the returned object via evaluate-with-callback. Highlights wrap text-node matches in <mark>;
/// clear() unwraps them. A heuristic (skips script/style, escapes the query), good enough for a
/// daily driver and gives the "3 / 12" counter window.find can't.
const FIND_JS: &str = r#"if(!window.__apFind){ window.__apFind=(function(){
  var marks=[],cur=-1,curQ=null;
  function unwrap(){ for(var i=0;i<marks.length;i++){ var m=marks[i],p=m.parentNode; if(p){ p.replaceChild(document.createTextNode(m.textContent),m); p.normalize(); } } marks=[]; cur=-1; }
  function highlight(q){
    unwrap(); if(!q) return;
    var rx; try{ rx=new RegExp(q.replace(/[.*+?^${}()|[\]\\]/g,'\\$&'),'gi'); }catch(e){ return; }
    var body=document.body; if(!body) return;
    var walker=document.createTreeWalker(body,NodeFilter.SHOW_TEXT,{ acceptNode:function(n){
      if(!n.nodeValue||!n.nodeValue.trim()) return NodeFilter.FILTER_REJECT;
      var p=n.parentNode; if(!p) return NodeFilter.FILTER_REJECT;
      var tag=p.nodeName; if(tag==='SCRIPT'||tag==='STYLE'||tag==='NOSCRIPT'||tag==='TEXTAREA') return NodeFilter.FILTER_REJECT;
      if(p.classList&&p.classList.contains('ap-find')) return NodeFilter.FILTER_REJECT;
      return NodeFilter.FILTER_ACCEPT;
    }});
    var nodes=[],n; while(n=walker.nextNode()) nodes.push(n);
    for(var k=0;k<nodes.length;k++){
      var node=nodes[k],text=node.nodeValue; rx.lastIndex=0; if(!rx.test(text)) continue; rx.lastIndex=0;
      var frag=document.createDocumentFragment(),last=0,m;
      while(m=rx.exec(text)){
        if(m.index>last) frag.appendChild(document.createTextNode(text.slice(last,m.index)));
        var mk=document.createElement('mark'); mk.className='ap-find'; mk.textContent=m[0];
        mk.style.cssText='background:#5b9cff;color:#fff;border-radius:2px;'; frag.appendChild(mk); marks.push(mk);
        last=m.index+m[0].length; if(m.index===rx.lastIndex) rx.lastIndex++;
      }
      if(last<text.length) frag.appendChild(document.createTextNode(text.slice(last)));
      node.parentNode.replaceChild(frag,node);
    }
  }
  function focus(i){
    if(!marks.length){ cur=-1; return; }
    if(cur>=0&&marks[cur]){ marks[cur].style.background='#5b9cff'; marks[cur].style.color='#fff'; }
    cur=((i%marks.length)+marks.length)%marks.length;
    var m=marks[cur]; m.style.background='#ffd166'; m.style.color='#000';
    if(m.scrollIntoView) m.scrollIntoView({block:'center'});
  }
  return {
    search:function(q,forward){
      if(q!==curQ){ highlight(q); curQ=q; cur=-1; }
      if(!marks.length){ cur=-1; return {count:0,index:0}; }
      focus(cur<0?0:(forward?cur+1:cur-1));
      return {count:marks.length,index:cur+1};
    },
    clear:function(){ unwrap(); curQ=null; }
  };
})(); }"#;

const TAB_H: f64 = 40.0;
const TOOLBAR_H: f64 = 44.0;
const BOOKMARK_H: f64 = 36.0;
const CHROME_H: f64 = TAB_H + TOOLBAR_H + BOOKMARK_H;
const W0: f64 = 1200.0;
const H0: f64 = 800.0;
/// Product name - window title + used to find the existing window for the single-instance guard.
const APP_TITLE: &str = "Aperture";
/// Privacy-hardening WebView2/Chromium switches, applied to every webview. Re-includes wry's own
/// defaults (msWebOOUI/msPdfOOUI/msSmartScreenProtection) because setting our own args REPLACES them.
/// Disables sync + SmartScreen + Edge's experimentation-driven UA override; forces secure
/// DNS-over-HTTPS; auto-upgrades http->https; partitions third-party storage to cut cross-site
/// tracking. (Switches are best-effort/unsupported per MS, so treat as defense-in-depth.)
const PRIVACY_ARGS: &str = concat!(
    "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection ",
    "--disable-sync --disable-domain-action-user-agent-override --no-default-browser-check ",
    "--dns-over-https-mode=secure --dns-over-https-templates=https://cloudflare-dns.com/dns-query ",
    "--enable-features=HttpsUpgrades,ThirdPartyStoragePartitioning,PartitionedCookies",
);
// Idle interval before a backgrounded tab sleeps (discarded ~one interval later) is configurable in
// settings (store::Settings::idle_secs, default 300); BROWSER_IDLE_SECS env overrides it for tests.

/// AI sidebar width (logical px) when open. The panel is a separate privileged webview docked on the
/// right; `AI_PANEL_W` holds its current width (0 = closed). content_rect/ai_rect read it so the
/// active tab narrows to make room without threading a flag through activate()'s many call sites.
const AI_W: f64 = 380.0;
static AI_PANEL_W: AtomicU32 = AtomicU32::new(0);
/// Split view: the tab id shown in the right pane (0 = no split). The active tab is the left pane.
static SPLIT_TAB: AtomicU32 = AtomicU32::new(0);
/// Tab strip orientation. false = horizontal top strip; true = vertical left column (Arc-style). Set
/// from settings at launch and toggled live from Settings. Read by the layout helpers so chrome
/// becomes a full-height left column and content shifts right instead of down.
static VERTICAL: AtomicBool = AtomicBool::new(false);
/// Width (logical px) of the chrome column in vertical-tab mode.
const SIDEBAR_W: f64 = 270.0;
/// True while the Settings Keyboard section is recording a new key combination. The accelerator
/// handler then suppresses shortcut actions and browser defaults so the combo reaches the settings
/// page's capture listener instead of firing the action it's currently bound to.
static KB_CAPTURING: AtomicBool = AtomicBool::new(false);
/// The chrome webview's Win32 child window (HWND as isize; 0 = unknown), captured at startup while
/// it is the only WebView2 child. Raised above the later-created content webviews whenever the
/// grown transparent chrome must float over the live page (dropdowns, the history viewer).
static CHROME_HWND: AtomicIsize = AtomicIsize::new(0);

/// Raise the chrome webview above the content webviews in z-order. Returns false when the chrome's
/// window was never identified (callers then fall back to hiding the content instead).
fn raise_chrome() -> bool {
    let raw = CHROME_HWND.load(Ordering::Relaxed);
    if raw == 0 {
        return false;
    }
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_TOP, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    };
    unsafe {
        SetWindowPos(
            windows::Win32::Foundation::HWND(raw as _),
            Some(HWND_TOP),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )
        .is_ok()
    }
}

fn vertical() -> bool {
    VERTICAL.load(Ordering::Relaxed)
}

// Pending site-permission prompts (notifications, camera, mic, location, ...). WebView2 fires
// PermissionRequested on the UI thread; we defer it, show our own styled prompt in the chrome, and
// complete the deferral when the user decides. Both the WebView2 callback and the decision handler
// run on the same (UI) thread, so a thread-local map of the (args, deferral) COM objects is safe and
// avoids needing Send. Keyed by a monotonic id round-tripped through the chrome UI.
thread_local! {
    static PENDING_PERMS: RefCell<HashMap<u32, (ICoreWebView2PermissionRequestedEventArgs, ICoreWebView2Deferral)>> =
        RefCell::new(HashMap::new());
}
static PERM_SEQ: AtomicU32 = AtomicU32::new(1);

/// Friendly label for a COREWEBVIEW2_PERMISSION_KIND code (the enum's integer values are stable).
fn permission_label(kind: i32) -> &'static str {
    match kind {
        1 => "use your microphone",
        2 => "use your camera",
        3 => "know your location",
        4 => "show notifications",
        5 => "use motion or light sensors",
        6 => "read your clipboard",
        7 => "play audio and video automatically",
        8 => "see the fonts installed on your device",
        9 => "send MIDI messages to your devices",
        10 => "manage windows across your displays",
        11 => "read and write local files",
        _ => "use a browser permission",
    }
}
/// Max page characters fed to the model as context (qwen3.5-gpu runs num_ctx 24576).
const AI_CTX_CHARS: usize = 14000;

/// How many past conversation turns (one turn = user + assistant message pair) to replay as context
/// on each new question. Bounded so a long chat doesn't blow num_ctx; oldest turns drop off.
const AI_HISTORY_TURNS: usize = 8;
/// Cap on a remembered assistant answer (chars). A long reply (page summary, translation) stays in
/// history only up to this; keeps replayed context small without losing the gist for follow-ups.
const AI_HISTORY_ANSWER_CHARS: usize = 4000;

/// Connection config for an AI request, snapshotted from the user's settings at spawn time and moved
/// into the worker thread (so settings can be changed without disturbing an in-flight request).
#[derive(Clone)]
struct AiCfg {
    host: String,
    model: String,
    keep_alive: String,
}

fn ai_cfg(s: &store::Settings) -> AiCfg {
    // serde's default only fills an ABSENT model; a present-but-empty string (hand-edited or partially
    // imported settings.json) would survive and Ollama rejects "". Coerce blank to the default here, the
    // single point every request's config flows through, so a blank model can never reach the model.
    let model = if s.ai.model.trim().is_empty() {
        store::default_ai_model()
    } else {
        s.ai.model.clone()
    };
    AiCfg {
        host: s.ai.host.clone(),
        model,
        keep_alive: s.ai.keep_alive.clone(),
    }
}

/// Pull the main readable text off the active page for the AI. Scores containers by paragraph text
/// (same heuristic as reader mode) and returns {title,url,text}. Returned as an object so the
/// evaluate-with-callback result is its JSON (parsed straight into PageContext).
const AI_EXTRACT_JS: &str = r#"(function(){
  try{
    function score(el){ var ps=el.getElementsByTagName('p'); var n=0; for(var i=0;i<ps.length;i++){ n+=(ps[i].innerText||'').length; } return n; }
    var best=document.body,bs=0,c=document.querySelectorAll('article,main,[role=main],section,div');
    for(var i=0;i<c.length;i++){ var s=score(c[i]); if(s>bs){ bs=s; best=c[i]; } }
    var t=((best&&best.innerText)||(document.body&&document.body.innerText)||'');
    t=t.replace(/[ \t]+\n/g,'\n').replace(/\n{3,}/g,'\n\n').trim();
    return { title: document.title||'', url: location.href||'', text: t.slice(0,20000) };
  }catch(e){ return { title: document.title||'', url: location.href||'', text: '' }; }
})()"#;

#[derive(Debug)]
enum UserEvent {
    Navigate(String),
    Back,
    Forward,
    Reload,
    NewTab,
    /// Open a new private/temporary tab on the isolated, ephemeral profile (Ctrl+Shift+N).
    NewPrivateTab,
    /// A page asked for a site permission (notifications/camera/mic/...). `kind` is the
    /// COREWEBVIEW2_PERMISSION_KIND integer. We show our own prompt and complete the deferral later.
    PermissionRequest { id: u32, origin: String, kind: i32 },
    /// The user answered a permission prompt: allow or block.
    PermissionDecide { id: u32, allow: bool },
    /// Open a URL in a new tab (from a page's new-window request: _blank / window.open / Ctrl-click).
    /// The second field is the OPENER tab's id when the request came from a page, so the trail can
    /// draw an edge from the opener's page to the new tab's page. None for app-initiated opens.
    NewTabUrl(String, Option<u32>),
    /// Reopen the most recently closed tab (Ctrl+Shift+T).
    ReopenClosed,
    /// Zoom the active tab: >0 in, <0 out, 0 reset.
    Zoom(i32),
    /// A tab's zoom factor changed (Ctrl+scroll or programmatic) -> remember it per-site + update chip.
    ZoomChanged(u32),
    /// Switch to the tab at this index (Ctrl+1..8; usize::MAX = last, Ctrl+9).
    SwitchToIndex(usize),
    /// Stop the active tab loading (Esc).
    Stop,
    /// A page's <title> changed -> update the tab label + window title.
    PageTitleChanged(u32, String),
    /// A page's favicon changed.
    PageFaviconChanged(u32, String),
    /// A page started/stopped playing audio.
    PageAudioChanged(u32, bool),
    /// A download started / finished on a tab (keeps the tab alive while active).
    DownloadStarted(u32),
    DownloadEnded(u32),
    /// A new download appeared (for the downloads popover): unique id, file name, target path.
    DownloadAdded {
        dl_id: u32,
        name: String,
        path: String,
    },
    /// A tracked download changed state: 1 = completed, 2 = interrupted/failed.
    DownloadStateChanged {
        dl_id: u32,
        state: u8,
    },
    /// Open a finished download in Explorer (with the file selected).
    OpenDownload(String),
    /// Clear the downloads popover list.
    ClearDownloads,
    /// Toggle mute on a tab (audio indicator clicked).
    ToggleMute(u32),
    /// Pointer entered a sleeping tab in the strip: resume it now so it's awake by the click.
    TabPrewake(u32),
    /// One frame of the AI panel's slide animation. `gen` guards against superseded toggles;
    /// `t` runs 0..=1; `opening` slides in from the right edge, closing slides out.
    AiSlide { gen: u64, t: f64, opening: bool },
    /// Tab right-click menu: grow the chrome to full window to draw the menu / shrink it back.
    MenuOpen,
    MenuClose,
    /// Tab context-menu actions.
    ReloadTab(u32),
    DuplicateTab(u32),
    /// Find-in-page (Ctrl+F): toggle the find bar, run a search on the active tab, clear highlights.
    ToggleFind,
    FindInPage(String, bool),
    FindClear,
    /// Result of a find (JSON `{count,index}` from the page) -> update the find bar's counter.
    FindResult(String),
    /// Restore tabs from the most recent full-window close.
    RestoreClosedWindow,
    /// Command palette (Ctrl+K): open/close the jump-to overlay.
    OpenPalette,
    ClosePalette,
    /// Reader mode: extract + restyle the active page's main content (toggle reloads to restore).
    ToggleReader,
    /// Native WebView2 print dialog for the active tab.
    Print,
    /// Capture the active page to a PNG in Downloads and reveal it in Explorer.
    Screenshot,
    /// Toggle split view: show the active tab beside another in two side-by-side panes.
    ToggleSplit,
    /// History viewer (Ctrl+H), drawn full-window in the chrome webview.
    OpenHistory,
    CloseHistory,
    /// Open a history entry in the active tab.
    HistoryOpen(String),
    /// Remove one history entry by URL.
    HistoryDelete(String),
    /// Clear all browsing history.
    HistoryClear,
    /// Wipe the browsing trail (Settings > Trail "clear" button).
    TrailClear,
    /// Reload the active tab bypassing the HTTP cache (Ctrl+Shift+R / Ctrl+F5 / Shift+F5).
    HardReload,
    /// A URL handed to Aperture from outside (default-browser open or a second launch).
    OpenExternal(String),
    /// Relaunch into another profile (this instance closes like a normal window close).
    SwitchProfile(String),
    /// Create a profile in the registry, then switch to it.
    CreateProfile(String),
    /// Remove a profile and its data directory (never Default, never the current one).
    DeleteProfile(String),
    /// Toggle the ask-which-profile-at-launch picker.
    ProfilesAsk(bool),
    /// Register Aperture in Windows' default-apps list and open Settings to confirm.
    SetDefaultBrowser,
    /// Show another workspace's tabs in the strip.
    SwitchWorkspace(u32),
    /// Create a workspace and switch to it.
    NewWorkspace,
    RenameWorkspace { ws: u32, name: String },
    SetWorkspaceColor { ws: u32, color: String },
    /// Delete a workspace; its tabs close (recoverable via Ctrl+Shift+T). Workspace 0 stays.
    DeleteWorkspace(u32),
    /// Move a tab into another workspace (drops its group).
    MoveTabToWorkspace { tab: u32, ws: u32 },
    /// Add (or replace) an Air Traffic Control rule: links to `pattern` open in workspace `ws`.
    AtcAdd { pattern: String, ws: u32 },
    /// Open an auto-archived tab in a new tab (and drop it from the archive).
    ArchiveOpen(String),
    /// Remove one archived entry by URL.
    ArchiveDelete(String),
    /// Clear the whole tab archive.
    ArchiveClear,
    CloseTab(u32),
    SwitchTab(u32),
    /// Reorder the tab strip (drag-to-reorder): the new left-to-right order of tab ids.
    ReorderTabs(Vec<u32>),
    /// Tab groups.
    AddTabToNewGroup(u32),
    AddTabToGroup {
        tab: u32,
        group: u32,
    },
    RemoveTabFromGroup(u32),
    ToggleGroupCollapse(u32),
    RenameGroup {
        group: u32,
        name: String,
    },
    SetGroupColor {
        group: u32,
        color: String,
    },
    Ungroup(u32),
    CloseGroup(u32),
    PageUrlChanged(u32, String),
    ChromeReady,
    /// AI sidebar: toggle the docked panel open/closed (Alt+G or its close button).
    ToggleAi,
    /// AI sidebar finished loading -> push the model name.
    AiReady,
    /// User asked the AI something. `scope` = "page" (use current page) | "general".
    AiAsk { scope: String, prompt: String },
    /// Active-page text came back from extraction; now build the prompt + start streaming.
    AiContext { req: u64, prompt: String, page_text: String },
    /// A screenshot of the active page was captured (base64 PNG); start a vision request.
    AiVisionContext { req: u64, prompt: String, image: String },
    /// A streamed token (content delta) for request `req`.
    AiToken { req: u64, delta: String },
    /// Streaming for `req` finished; `error` set if it failed.
    AiDone { req: u64, error: Option<String> },
    /// Stop the in-flight AI request.
    AiStop,
    /// Start a fresh conversation: drop the remembered turns so the AI stops carrying old context.
    AiNewChat,
    /// Translate the active page into the configured target language (via the local model).
    TranslatePage,
    /// Replace the whole bookmark list (from the Settings bookmarks editor; carries folders).
    SaveBookmarks(String),
    /// Create a new (empty) bookmark folder by name (right-click the bookmark bar).
    BookmarkNewFolder(String),
    /// Move a bookmark into a folder (drag onto a folder chip / pick in the add popup); None = loose.
    BookmarkSetFolder { url: String, folder: Option<String> },
    /// Delete a bookmark folder: remove the folder and unfile its bookmarks (they go loose).
    BookmarkDeleteFolder(String),
    /// Reorder: move bookmark `url` to just before/after `target` in the bar.
    BookmarkReorder { url: String, target: String, before: bool },
    /// Reading list: save the current page, open a saved page, remove one.
    ReadingAdd,
    ReadingOpen(String),
    ReadingRemove(String),
    /// AI: organize open tabs into topic groups.
    AiOrganizeTabs,
    /// AI: the grouping plan came back (JSON from the model) -> apply it.
    AiTabPlan { req: u64, plan: String },
    /// A question typed in the omnibox with a leading "?" -> ask the AI (general, no page context).
    AiOmnibox(String),
    /// A selection-pill action on a page: explain / translate / rewrite the selected text.
    AiSelection { action: String, text: String },
    /// Web-search progress note shown in the sidebar (Searching…/Reading…/Answering…).
    AiWebStatus { req: u64, text: String },
    /// The sources a web-search answer is grounded in (title, url), rendered as citation chips.
    AiSources { req: u64, sources: Vec<(String, String)> },
    /// Installed Ollama models, fetched when the settings panel opens (for the model dropdown).
    AiModels(Vec<String>),
    /// Idle-timer tick: escalate backgrounded tabs (sleep, then discard).
    ReclaimIdleTabs,
    /// Bitwarden: key button clicked -> open the unlock modal. (Legacy CLI path; superseded by the
    /// real extension, kept for now.)
    BwFill,
    /// Open the loaded password-manager extension's popup (Bitwarden unlock/vault UI) in a new tab.
    OpenExtension,
    /// Bitwarden: master password submitted from the modal.
    BwSubmit(String),
    /// Bitwarden: modal cancelled.
    BwCancel,
    /// Bitwarden: background unlock+lookup finished.
    BwResult(bitwarden::FillOutcome),
    /// A page called navigator.credentials.get() -> sign the challenge with a vault passkey.
    WebauthnGet {
        tab_id: u32,
        req_id: String,
        rp_id: String,
        challenge: String,
    },
    /// Background passkey fetch finished -> sign + deliver the assertion (or fall back / error).
    WebauthnPasskey(bitwarden::PasskeyOutcome),
    /// Bookmark the active page.
    BookmarkAdd,
    /// Toggle the active page's bookmark (add if absent, remove if present) - the star button.
    BookmarkToggle,
    /// Open a bookmarked URL in the active tab.
    BookmarkOpen(String),
    /// Remove a bookmark by URL.
    BookmarkRemove(String),
    /// Close the active tab (Ctrl+W).
    CloseActiveTab,
    /// Focus the omnibox (Ctrl+L).
    FocusOmnibox,
    /// Cycle tabs (Ctrl+Tab forward, Ctrl+Shift+Tab back).
    CycleTab(bool),
    /// Toggle a tab's pinned ("keep awake") state.
    TogglePin(u32),
    /// Omnibox text changed -> compute an inline autocomplete.
    OmniboxTyping(String),
    /// The new-tab/home page finished loading -> inject live pinned/recent/links data.
    HomeReady(u32),
    /// A page finished loading -> refresh the back/forward enabled state.
    PageLoaded(u32),
    /// Open the settings panel (drawn inside the chrome webview).
    OpenSettings,
    /// Persist edited home-page quick links (JSON array of {icon,label,url}).
    SaveLinks(String),
    /// Persist edited preferences (JSON {name, search_url, idle_secs}).
    SaveSettings(String),
    /// Privacy: delete all cookies now (sign out everywhere).
    ClearCookies,
    /// Export bookmarks + settings to a JSON file in Downloads.
    ExportData,
    /// Import bookmarks + settings from a JSON string (read from a file the user picked).
    ImportData(String),
    /// Close the settings panel.
    CloseSettings,
    /// A page reported a login submit -> offer to save it to Bitwarden.
    BwSaveCandidate {
        url: String,
        username: String,
        password: String,
    },
    /// Master password entered in the save prompt -> write the pending login to the vault.
    BwSaveSubmit(String),
    /// Save prompt dismissed.
    BwSaveCancel,
    /// Background save-to-vault finished.
    BwSaveResult(bitwarden::SaveOutcome),
}

/// A login captured from a page, held until the user confirms (or dismisses) saving it. The
/// password is zeroized when this is dropped.
struct PendingSave {
    url: String,
    username: String,
    password: String,
}

impl Drop for PendingSave {
    fn drop(&mut self) {
        self.password.zeroize();
    }
}

/// A passkey assertion request awaiting the master-password unlock. Origin is the host-verified
/// page origin (never page-supplied) so the signed clientDataJSON can't be spoofed.
struct PendingWebauthn {
    tab_id: u32,
    req_id: String,
    rp_id: String,
    challenge: String,
    origin: String,
}

struct Tab {
    id: u32,
    /// None once the tab has been discarded (renderer freed); recreated on reactivation.
    webview: Option<WebView>,
    url: String,
    title: String,
    suspended: bool,
    discarded: bool,
    /// When the user last had this tab active (stamped on activation and refreshed for exempt tabs
    /// each sweep). The idle sweep suspends/discards based on real per-tab idle time, not sweep
    /// cadence, so switching away from a tab never puts it to sleep moments later.
    last_active: std::time::Instant,
    /// Wall-clock twin of `last_active` (unix seconds), persisted in the session so the tab
    /// auto-archiver can measure idle time across restarts.
    last_used: u64,
    /// Pinned ("keep awake") tabs are never slept or discarded by the idle sweep.
    pinned: bool,
    /// Favicon URI (http/data), empty if none yet.
    favicon: String,
    /// Whether the page is currently playing audio, and whether the user muted it.
    audio: bool,
    muted: bool,
    /// In-progress downloads on this tab; exempt from the idle sweep while > 0.
    downloading: u32,
    /// Tab-group id this tab belongs to, if any.
    group: Option<u32>,
    /// Workspace this tab lives in (0 = "Main").
    workspace: u32,
    /// Private/temporary tab: built on a separate, ephemeral WebContext (its cookies/storage are
    /// isolated from the main profile and wiped on next launch). Never persisted, never auto-discarded.
    private: bool,
    /// Current top-level URL, shared with this tab's adblock handler as the request source.
    page_url: Rc<RefCell<String>>,
    /// Pending trail-edge source for this tab's NEXT recorded visit: the opener tab's page when
    /// this tab was spawned from a link. Consumed by the first PageUrlChanged. After that, edges
    /// come from the tab's own previous URL.
    trail_from: Option<String>,
}

/// A named tab context. The strip shows one workspace's tabs at a time; the others' tabs stay in
/// `tabs` (hidden, still aging through the sleep/discard/archive tiers). Workspace 0 ("Main")
/// always exists and cannot be deleted.
struct Workspace {
    id: u32,
    name: String,
    /// One of GROUP_COLORS (same palette as tab groups).
    color: String,
}

/// A color-labeled, optionally-collapsed tab group (Chrome-style). Groups are rendered as a chip in
/// the tab strip before their (contiguous) member tabs.
struct TabGroup {
    id: u32,
    name: String,
    /// One of the named swatches in GROUP_COLORS (a CSS color the chrome UI knows how to tint).
    color: String,
    collapsed: bool,
}

/// The cycle of group colors offered when creating a new group / recoloring one.
const GROUP_COLORS: [&str; 7] = ["grey", "blue", "red", "yellow", "green", "purple", "cyan"];

/// One entry in the downloads popover. `state`: 0 = in progress, 1 = completed, 2 = failed.
struct DownloadItem {
    id: u32,
    name: String,
    path: String,
    state: u8,
}

fn main() -> wry::Result<()> {
    // Command line: an optional `--profile <name>` plus an optional URL (or .html file). Windows
    // hands links here when Aperture is the default browser, and "Open with" hands files.
    let (profile_arg, url_arg) = parse_args();
    // Resolve the profile BEFORE anything touches the store: the explicit argument wins, then the
    // last-used profile, then Default. Unknown names fall back to Default rather than silently
    // creating a data dir.
    let mut profiles = store::load_profiles();
    let asked_for_profile = profile_arg.is_some();
    let current_profile = profile_arg
        .filter(|p| p == "Default" || profiles.list.contains(p))
        .unwrap_or_else(|| {
            let l = profiles.last.clone();
            if l == "Default" || profiles.list.contains(&l) {
                l
            } else {
                "Default".to_string()
            }
        });
    store::set_profile(&current_profile);
    profiles.last = current_profile.clone();
    store::save_profiles(&profiles);
    let _ = PROFILE_SUFFIX.set(if current_profile == "Default" {
        String::new()
    } else {
        format!(" [{current_profile}]")
    });
    let cli_url = cli_url_from(url_arg);
    // Single-instance PER PROFILE: a second launch of the same profile would collide on its shared
    // WebView2 user-data dir. Forward any URL to the running instance over the open-url pipe,
    // focus its window, and bail. Different profiles run side by side (separate data dirs).
    if another_instance_running(&current_profile) {
        if let Some(u) = &cli_url {
            send_url_to_running_instance(u);
        }
        focus_existing_window();
        return Ok(());
    }

    ensure_webview2_runtime();

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let window = WindowBuilder::new()
        .with_title(APP_TITLE)
        .with_window_icon(Icon::from_resource(1, None).ok())
        .with_inner_size(LogicalSize::new(W0, H0))
        .build(&event_loop)
        .expect("failed to create window");

    let proxy_ipc = event_loop.create_proxy();
    let spawn_proxy = event_loop.create_proxy();
    // Listen for URLs forwarded by later launches (links clicked in other apps).
    spawn_open_url_pipe(event_loop.create_proxy());

    // Idle timer: periodically ask the run loop to reclaim backgrounded tabs. The tick is a fraction
    // of the idle threshold (the sweep itself checks per-tab elapsed idle), so a tab is judged on how
    // long IT has been idle, with fine enough granularity that the timing feels fair.
    let idle_proxy = event_loop.create_proxy();
    let idle = idle_secs();
    // Sweep thresholds derived from the user's idle setting: sleep after `idle`, discard (drop the
    // renderer, full reload on return) only after 6x that - discarding is the expensive tier to wake,
    // so it's reserved for tabs that are genuinely abandoned, not merely backgrounded.
    let suspend_after = std::time::Duration::from_secs(idle);
    let discard_after = std::time::Duration::from_secs(idle.saturating_mul(6));
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs((idle / 4).max(10)));
        if idle_proxy.send_event(UserEvent::ReclaimIdleTabs).is_err() {
            break;
        }
    });

    // One shared, persistent WebView2 profile for the chrome + every tab, kept in a STABLE location
    // (under %APPDATA%/RustBrowser, not next to the exe) so logins/cookies/sessions survive rebuilds,
    // moving the exe, and packaging. Sharing one context = one cookie jar: log into Gmail in any tab
    // and you are logged in everywhere, and you stay logged in across restarts.
    let mut web_context = WebContext::new(Some(store::data_dir().join("WebView2")));

    // Separate, ephemeral profile for private/temporary tabs: its own user-data folder, isolated from
    // the main cookie jar and wiped at every launch so private browsing never carries across sessions.
    // (WebView2 environments with distinct folders are independent, so this can't collide with the
    // persistent profile.) Created up front so opening a private tab is instant.
    let private_profile = store::data_dir().join("PrivateWebView2");
    let _ = std::fs::remove_dir_all(&private_profile);
    let mut private_context = WebContext::new(Some(private_profile));

    // Browser extensions (e.g. the real Bitwarden extension, for password + passkey autofill) are
    // a profile-level feature. The env option is set when the environment is created - i.e. by the
    // FIRST webview built on this shared WebContext, which is the chrome below. Loading the unpacked
    // extensions here adds them to the shared profile; every content tab then runs their content
    // scripts. We only enable it when something is actually present to load.
    let load_extensions = store::has_extensions();
    let extensions_dir = store::extensions_dir();
    // Tab orientation is a layout-time flag the rect helpers read; set it from settings before any
    // webview is positioned. (Settings are fully loaded again below for the rest of the run loop.)
    VERTICAL.store(store::load_settings().vertical_tabs, Ordering::Relaxed);
    let chrome = {
        let mut b = WebViewBuilder::new_with_web_context(&mut web_context)
            .with_html(include_str!("ui/chrome.html"))
            .with_additional_browser_args(PRIVACY_ARGS)
            // Transparent default background: the chrome paints its own opaque strip, so when it is
            // grown full-window for a dropdown/popover the rest of it stays see-through and the page
            // remains visible underneath (raised above the content webviews at MenuOpen).
            .with_transparent(true)
            .with_bounds(chrome_rect(W0, H0));
        if load_extensions {
            b = b
                .with_browser_extensions_enabled(true)
                .with_extensions_path(extensions_dir.clone());
        }
        b
    }
    .with_ipc_handler(move |req| {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(req.body()) {
            match v["cmd"].as_str() {
                Some("navigate") => {
                    if let Some(u) = v["url"].as_str() {
                        let _ = proxy_ipc.send_event(UserEvent::Navigate(u.to_string()));
                    }
                }
                Some("back") => {
                    let _ = proxy_ipc.send_event(UserEvent::Back);
                }
                Some("forward") => {
                    let _ = proxy_ipc.send_event(UserEvent::Forward);
                }
                Some("reload") => {
                    let _ = proxy_ipc.send_event(UserEvent::Reload);
                }
                Some("new_tab") => {
                    let _ = proxy_ipc.send_event(UserEvent::NewTab);
                }
                Some("new_private_tab") => {
                    let _ = proxy_ipc.send_event(UserEvent::NewPrivateTab);
                }
                Some("permission_decide") => {
                    let id = v["id"].as_u64().unwrap_or(0) as u32;
                    let allow = v["allow"].as_bool().unwrap_or(false);
                    if id != 0 {
                        let _ = proxy_ipc.send_event(UserEvent::PermissionDecide { id, allow });
                    }
                }
                Some("close_tab") => {
                    if let Some(i) = v["id"].as_u64() {
                        let _ = proxy_ipc.send_event(UserEvent::CloseTab(i as u32));
                    }
                }
                Some("switch_tab") => {
                    if let Some(i) = v["id"].as_u64() {
                        let _ = proxy_ipc.send_event(UserEvent::SwitchTab(i as u32));
                    }
                }
                Some("reorder_tabs") => {
                    if let Some(arr) = v["order"].as_array() {
                        let order: Vec<u32> = arr
                            .iter()
                            .filter_map(|x| x.as_u64().map(|n| n as u32))
                            .collect();
                        let _ = proxy_ipc.send_event(UserEvent::ReorderTabs(order));
                    }
                }
                Some("group_new") => {
                    if let Some(i) = v["id"].as_u64() {
                        let _ = proxy_ipc.send_event(UserEvent::AddTabToNewGroup(i as u32));
                    }
                }
                Some("group_add") => {
                    if let (Some(t), Some(g)) = (v["id"].as_u64(), v["group"].as_u64()) {
                        let _ = proxy_ipc.send_event(UserEvent::AddTabToGroup {
                            tab: t as u32,
                            group: g as u32,
                        });
                    }
                }
                Some("group_remove") => {
                    if let Some(i) = v["id"].as_u64() {
                        let _ = proxy_ipc.send_event(UserEvent::RemoveTabFromGroup(i as u32));
                    }
                }
                Some("group_collapse") => {
                    if let Some(g) = v["group"].as_u64() {
                        let _ = proxy_ipc.send_event(UserEvent::ToggleGroupCollapse(g as u32));
                    }
                }
                Some("group_rename") => {
                    if let (Some(g), Some(n)) = (v["group"].as_u64(), v["name"].as_str()) {
                        let _ = proxy_ipc.send_event(UserEvent::RenameGroup {
                            group: g as u32,
                            name: n.to_string(),
                        });
                    }
                }
                Some("group_color") => {
                    if let (Some(g), Some(c)) = (v["group"].as_u64(), v["color"].as_str()) {
                        let _ = proxy_ipc.send_event(UserEvent::SetGroupColor {
                            group: g as u32,
                            color: c.to_string(),
                        });
                    }
                }
                Some("group_ungroup") => {
                    if let Some(g) = v["group"].as_u64() {
                        let _ = proxy_ipc.send_event(UserEvent::Ungroup(g as u32));
                    }
                }
                Some("group_close") => {
                    if let Some(g) = v["group"].as_u64() {
                        let _ = proxy_ipc.send_event(UserEvent::CloseGroup(g as u32));
                    }
                }
                Some("ready") => {
                    let _ = proxy_ipc.send_event(UserEvent::ChromeReady);
                }
                Some("restore_closed") => {
                    let _ = proxy_ipc.send_event(UserEvent::RestoreClosedWindow);
                }
                Some("bw_fill") => {
                    let _ = proxy_ipc.send_event(UserEvent::BwFill);
                }
                Some("open_extension") => {
                    let _ = proxy_ipc.send_event(UserEvent::OpenExtension);
                }
                Some("bw_submit") => {
                    if let Some(pw) = v["password"].as_str() {
                        let _ = proxy_ipc.send_event(UserEvent::BwSubmit(pw.to_string()));
                    }
                }
                Some("bw_cancel") => {
                    let _ = proxy_ipc.send_event(UserEvent::BwCancel);
                }
                Some("bookmark_add") => {
                    let _ = proxy_ipc.send_event(UserEvent::BookmarkAdd);
                }
                Some("bookmark_toggle") => {
                    let _ = proxy_ipc.send_event(UserEvent::BookmarkToggle);
                }
                Some("bookmark_open") => {
                    if let Some(u) = v["url"].as_str() {
                        let _ = proxy_ipc.send_event(UserEvent::BookmarkOpen(u.to_string()));
                    }
                }
                Some("bookmark_remove") => {
                    if let Some(u) = v["url"].as_str() {
                        let _ = proxy_ipc.send_event(UserEvent::BookmarkRemove(u.to_string()));
                    }
                }
                Some("save_bookmarks") => {
                    let _ = proxy_ipc.send_event(UserEvent::SaveBookmarks(v["bookmarks"].to_string()));
                }
                Some("bookmark_new_folder") => {
                    if let Some(n) = v["name"].as_str() {
                        let _ = proxy_ipc.send_event(UserEvent::BookmarkNewFolder(n.to_string()));
                    }
                }
                Some("bookmark_set_folder") => {
                    if let Some(u) = v["url"].as_str() {
                        let folder = v["folder"].as_str().filter(|s| !s.is_empty()).map(|s| s.to_string());
                        let _ = proxy_ipc.send_event(UserEvent::BookmarkSetFolder {
                            url: u.to_string(),
                            folder,
                        });
                    }
                }
                Some("bookmark_delete_folder") => {
                    if let Some(n) = v["name"].as_str() {
                        let _ = proxy_ipc.send_event(UserEvent::BookmarkDeleteFolder(n.to_string()));
                    }
                }
                Some("bookmark_reorder") => {
                    if let (Some(u), Some(t)) = (v["url"].as_str(), v["target"].as_str()) {
                        let _ = proxy_ipc.send_event(UserEvent::BookmarkReorder {
                            url: u.to_string(),
                            target: t.to_string(),
                            before: v["before"].as_bool().unwrap_or(true),
                        });
                    }
                }
                Some("reading_add") => {
                    let _ = proxy_ipc.send_event(UserEvent::ReadingAdd);
                }
                Some("reading_open") => {
                    if let Some(u) = v["url"].as_str() {
                        let _ = proxy_ipc.send_event(UserEvent::ReadingOpen(u.to_string()));
                    }
                }
                Some("reading_remove") => {
                    if let Some(u) = v["url"].as_str() {
                        let _ = proxy_ipc.send_event(UserEvent::ReadingRemove(u.to_string()));
                    }
                }
                Some("toggle_pin") => {
                    if let Some(i) = v["id"].as_u64() {
                        let _ = proxy_ipc.send_event(UserEvent::TogglePin(i as u32));
                    }
                }
                Some("toggle_mute") => {
                    if let Some(i) = v["id"].as_u64() {
                        let _ = proxy_ipc.send_event(UserEvent::ToggleMute(i as u32));
                    }
                }
                Some("tab_prewake") => {
                    if let Some(i) = v["id"].as_u64() {
                        let _ = proxy_ipc.send_event(UserEvent::TabPrewake(i as u32));
                    }
                }
                Some("kb_capture") => {
                    KB_CAPTURING.store(v["on"].as_bool().unwrap_or(false), Ordering::Relaxed);
                }
                Some("menu_open") => {
                    let _ = proxy_ipc.send_event(UserEvent::MenuOpen);
                }
                Some("menu_close") => {
                    let _ = proxy_ipc.send_event(UserEvent::MenuClose);
                }
                Some("reload_tab") => {
                    if let Some(i) = v["id"].as_u64() {
                        let _ = proxy_ipc.send_event(UserEvent::ReloadTab(i as u32));
                    }
                }
                Some("duplicate_tab") => {
                    if let Some(i) = v["id"].as_u64() {
                        let _ = proxy_ipc.send_event(UserEvent::DuplicateTab(i as u32));
                    }
                }
                Some("find") => {
                    if let Some(q) = v["q"].as_str() {
                        let fwd = v["forward"].as_bool().unwrap_or(true);
                        let _ = proxy_ipc.send_event(UserEvent::FindInPage(q.to_string(), fwd));
                    }
                }
                Some("find_clear") => {
                    let _ = proxy_ipc.send_event(UserEvent::FindClear);
                }
                Some("close_palette") => {
                    let _ = proxy_ipc.send_event(UserEvent::ClosePalette);
                }
                Some("open_download") => {
                    if let Some(p) = v["path"].as_str() {
                        let _ = proxy_ipc.send_event(UserEvent::OpenDownload(p.to_string()));
                    }
                }
                Some("clear_downloads") => {
                    let _ = proxy_ipc.send_event(UserEvent::ClearDownloads);
                }
                Some("reader") => {
                    let _ = proxy_ipc.send_event(UserEvent::ToggleReader);
                }
                Some("ai_toggle") => {
                    let _ = proxy_ipc.send_event(UserEvent::ToggleAi);
                }
                Some("ai_omnibox") => {
                    if let Some(q) = v["q"].as_str() {
                        let _ = proxy_ipc.send_event(UserEvent::AiOmnibox(q.to_string()));
                    }
                }
                Some("translate_page") => {
                    let _ = proxy_ipc.send_event(UserEvent::TranslatePage);
                }
                Some("organize_tabs") => {
                    let _ = proxy_ipc.send_event(UserEvent::AiOrganizeTabs);
                }
                Some("print") => {
                    let _ = proxy_ipc.send_event(UserEvent::Print);
                }
                Some("screenshot") => {
                    let _ = proxy_ipc.send_event(UserEvent::Screenshot);
                }
                Some("split_view") => {
                    let _ = proxy_ipc.send_event(UserEvent::ToggleSplit);
                }
                Some("zoom_reset") => {
                    let _ = proxy_ipc.send_event(UserEvent::Zoom(0));
                }
                Some("open_history") => {
                    let _ = proxy_ipc.send_event(UserEvent::OpenHistory);
                }
                Some("close_history") => {
                    let _ = proxy_ipc.send_event(UserEvent::CloseHistory);
                }
                Some("history_open") => {
                    if let Some(u) = v["url"].as_str() {
                        let _ = proxy_ipc.send_event(UserEvent::HistoryOpen(u.to_string()));
                    }
                }
                Some("history_delete") => {
                    if let Some(u) = v["url"].as_str() {
                        let _ = proxy_ipc.send_event(UserEvent::HistoryDelete(u.to_string()));
                    }
                }
                Some("history_clear") => {
                    let _ = proxy_ipc.send_event(UserEvent::HistoryClear);
                }
                Some("trail_clear") => {
                    let _ = proxy_ipc.send_event(UserEvent::TrailClear);
                }
                Some("set_default_browser") => {
                    let _ = proxy_ipc.send_event(UserEvent::SetDefaultBrowser);
                }
                Some("profile_switch") => {
                    if let Some(n) = v["name"].as_str() {
                        let _ = proxy_ipc.send_event(UserEvent::SwitchProfile(n.to_string()));
                    }
                }
                Some("profile_create") => {
                    if let Some(n) = v["name"].as_str() {
                        let _ = proxy_ipc.send_event(UserEvent::CreateProfile(n.to_string()));
                    }
                }
                Some("profile_delete") => {
                    if let Some(n) = v["name"].as_str() {
                        let _ = proxy_ipc.send_event(UserEvent::DeleteProfile(n.to_string()));
                    }
                }
                Some("profiles_ask") => {
                    if let Some(on) = v["on"].as_bool() {
                        let _ = proxy_ipc.send_event(UserEvent::ProfilesAsk(on));
                    }
                }
                Some("ws_switch") => {
                    if let Some(id) = v["id"].as_u64() {
                        let _ = proxy_ipc.send_event(UserEvent::SwitchWorkspace(id as u32));
                    }
                }
                Some("ws_new") => {
                    let _ = proxy_ipc.send_event(UserEvent::NewWorkspace);
                }
                Some("ws_rename") => {
                    if let (Some(id), Some(name)) = (v["id"].as_u64(), v["name"].as_str()) {
                        let _ = proxy_ipc.send_event(UserEvent::RenameWorkspace {
                            ws: id as u32,
                            name: name.to_string(),
                        });
                    }
                }
                Some("ws_color") => {
                    if let (Some(id), Some(color)) = (v["id"].as_u64(), v["color"].as_str()) {
                        let _ = proxy_ipc.send_event(UserEvent::SetWorkspaceColor {
                            ws: id as u32,
                            color: color.to_string(),
                        });
                    }
                }
                Some("ws_delete") => {
                    if let Some(id) = v["id"].as_u64() {
                        let _ = proxy_ipc.send_event(UserEvent::DeleteWorkspace(id as u32));
                    }
                }
                Some("atc_add") => {
                    if let (Some(pattern), Some(ws)) = (v["pattern"].as_str(), v["ws"].as_u64()) {
                        let _ = proxy_ipc.send_event(UserEvent::AtcAdd {
                            pattern: pattern.to_string(),
                            ws: ws as u32,
                        });
                    }
                }
                Some("ws_move_tab") => {
                    if let (Some(tab), Some(ws)) = (v["tab"].as_u64(), v["ws"].as_u64()) {
                        let _ = proxy_ipc.send_event(UserEvent::MoveTabToWorkspace {
                            tab: tab as u32,
                            ws: ws as u32,
                        });
                    }
                }
                Some("archive_open") => {
                    if let Some(u) = v["url"].as_str() {
                        let _ = proxy_ipc.send_event(UserEvent::ArchiveOpen(u.to_string()));
                    }
                }
                Some("archive_delete") => {
                    if let Some(u) = v["url"].as_str() {
                        let _ = proxy_ipc.send_event(UserEvent::ArchiveDelete(u.to_string()));
                    }
                }
                Some("archive_clear") => {
                    let _ = proxy_ipc.send_event(UserEvent::ArchiveClear);
                }
                Some("omnibox_typing") => {
                    if let Some(txt) = v["text"].as_str() {
                        let _ = proxy_ipc.send_event(UserEvent::OmniboxTyping(txt.to_string()));
                    }
                }
                Some("open_settings") => {
                    let _ = proxy_ipc.send_event(UserEvent::OpenSettings);
                }
                Some("save_links") => {
                    let _ = proxy_ipc.send_event(UserEvent::SaveLinks(v["links"].to_string()));
                }
                Some("save_settings") => {
                    let _ = proxy_ipc.send_event(UserEvent::SaveSettings(v["prefs"].to_string()));
                }
                Some("close_settings") => {
                    let _ = proxy_ipc.send_event(UserEvent::CloseSettings);
                }
                Some("clear_cookies") => {
                    let _ = proxy_ipc.send_event(UserEvent::ClearCookies);
                }
                Some("export_data") => {
                    let _ = proxy_ipc.send_event(UserEvent::ExportData);
                }
                Some("import_data") => {
                    if let Some(d) = v["data"].as_str() {
                        let _ = proxy_ipc.send_event(UserEvent::ImportData(d.to_string()));
                    }
                }
                Some("bw_save_submit") => {
                    if let Some(pw) = v["password"].as_str() {
                        let _ = proxy_ipc.send_event(UserEvent::BwSaveSubmit(pw.to_string()));
                    }
                }
                Some("bw_save_cancel") => {
                    let _ = proxy_ipc.send_event(UserEvent::BwSaveCancel);
                }
                _ => {}
            }
        }
    })
    .build_as_child(&window)?;

    // The chrome webview's Win32 child window, captured NOW while it is the only WebView2 child of
    // the main window. Webviews created later sit above it in z-order, so floating a dropdown or
    // the history viewer over the live page requires raising this handle (raise_chrome).
    unsafe {
        use tao::platform::windows::WindowExtWindows;
        use windows::Win32::UI::WindowsAndMessaging::{GetWindow, GW_CHILD};
        if let Ok(h) = GetWindow(
            windows::Win32::Foundation::HWND(window.hwnd() as _),
            GW_CHILD,
        ) {
            CHROME_HWND.store(h.0 as isize, Ordering::Relaxed);
        }
    }

    attach_shortcuts(&chrome, event_loop.create_proxy());

    // AI sidebar: a second privileged webview docked on the right. Like the chrome it talks to Rust
    // over IPC, but it never gets host powers over navigation - it only drives the local-AI flow.
    // Built on the same shared WebContext (so it must agree with the extensions env option), starts
    // hidden and zero-width (AI_PANEL_W = 0) until toggled open.
    let ai_proxy = event_loop.create_proxy();
    let ai_sidebar = WebViewBuilder::new_with_web_context(&mut web_context)
        .with_html(include_str!("ui/ai.html"))
        .with_additional_browser_args(PRIVACY_ARGS)
        .with_background_color((13, 15, 19, 255))
        .with_browser_extensions_enabled(load_extensions)
        .with_visible(false)
        .with_bounds(ai_rect(W0, H0))
        .with_ipc_handler(move |req| {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(req.body()) {
                match v["cmd"].as_str() {
                    Some("ai_ready") => {
                        let _ = ai_proxy.send_event(UserEvent::AiReady);
                    }
                    Some("ai_toggle") => {
                        let _ = ai_proxy.send_event(UserEvent::ToggleAi);
                    }
                    Some("ai_stop") => {
                        let _ = ai_proxy.send_event(UserEvent::AiStop);
                    }
                    Some("ai_new_chat") => {
                        let _ = ai_proxy.send_event(UserEvent::AiNewChat);
                    }
                    Some("ai_ask") => {
                        if let Some(p) = v["prompt"].as_str() {
                            let _ = ai_proxy.send_event(UserEvent::AiAsk {
                                scope: v["scope"].as_str().unwrap_or("page").to_string(),
                                prompt: p.to_string(),
                            });
                        }
                    }
                    Some("ai_open_url") => {
                        // The AI proposes source links; only follow web URLs, never javascript:/data:/file:.
                        if let Some(u) = v["url"].as_str() {
                            if url_is_safe(u) {
                                let _ = ai_proxy.send_event(UserEvent::NewTabUrl(u.to_string(), None));
                            }
                        }
                    }
                    _ => {}
                }
            }
        })
        .build_as_child(&window)?;
    attach_shortcuts(&ai_sidebar, event_loop.create_proxy());

    // Shared ad/tracker blocking engine (one instance, all tabs).
    let engine = Rc::new(blocker::build_engine());
    let mut bookmarks = store::load_bookmarks();
    let mut reading = store::load_reading();
    let mut history = store::load_history();
    let mut links = store::load_links();
    let mut settings = store::load_settings();
    // Browsing trail (the new-tab navigation graph); prune to the retention window at launch.
    let mut trail = store::load_trail();
    store::prune_trail(&mut trail, settings.trail.retention_days);
    // Tabs the auto-archiver has closed (History viewer > Archived).
    let mut archive = store::load_archive();
    // Keyboard shortcuts: defaults overlaid with the user's rebinds (Settings > Keyboard).
    rebuild_shortcut_map(&settings.shortcuts);
    // Privacy: at startup, forget cookies for every site except the keep-signed-in allowlist.
    if settings.clear_other_on_launch {
        clear_non_allowlisted_cookies(&chrome, settings.keep_logged_in.clone());
    }
    // A login captured from a page, awaiting the user's save decision; plus sites the user said
    // "not now" to (so we stop re-prompting them this session).
    let mut pending_save: Option<PendingSave> = None;
    let mut dismissed_save_hosts: HashSet<String> = HashSet::new();
    let mut pending_webauthn: Option<PendingWebauthn> = None;
    // AI: monotonic request id (lets late tokens from a superseded/stopped request be dropped) and a
    // shared cancel flag the worker thread polls.
    let mut ai_req: u64 = 0;
    let mut ai_cancel: Option<Arc<AtomicBool>> = None;
    // Conversation memory: past turns (user/assistant alternating, no system, page text, or images),
    // replayed in front of each new question so follow-ups have context. `ai_turn_user` is the
    // user-visible text of the in-flight turn (None for one-shot utility runs like Organize Tabs and
    // Translate Page, which must not pollute the chat); `ai_turn_answer` accumulates the streamed reply
    // so it can be committed to history when the stream finishes. Cleared by the New chat button.
    let mut ai_history: Vec<ai::Msg> = Vec::new();
    let mut ai_turn_user: Option<String> = None;
    let mut ai_turn_answer = String::new();
    // Generation counter for the AI panel's slide animation; a bump orphans in-flight frames.
    let mut ai_anim_gen: u64 = 0;
    // Whether the history viewer is up, so Ctrl+H can toggle it closed.
    let mut history_open = false;
    // URLs of recently closed tabs, for Ctrl+Shift+T (most-recent on top).
    let mut closed_stack: Vec<String> = Vec::new();
    // Downloads shown in the toolbar popover (most-recent on top), this session only.
    let mut downloads: Vec<DownloadItem> = Vec::new();
    // Per-site zoom factors (host -> factor), remembered across navigations and restarts.
    let mut zoom_levels: HashMap<String, f64> = store::load_zoom();
    // Tab groups, restored from groups.json.
    let mut groups: Vec<TabGroup> = store::load_groups()
        .into_iter()
        .map(|g| TabGroup {
            id: g.id,
            name: g.name,
            color: g.color,
            collapsed: g.collapsed,
        })
        .collect();
    let mut next_group_id: u32 = groups.iter().map(|g| g.id).max().unwrap_or(0) + 1;
    // Workspaces (named tab contexts). Workspace 0 "Main" always exists; the strip shows one
    // workspace at a time and the rest of the tabs stay hidden but alive.
    let ws_file = store::load_workspaces();
    let mut workspaces: Vec<Workspace> = ws_file
        .list
        .iter()
        .map(|w| Workspace {
            id: w.id,
            name: w.name.clone(),
            color: w.color.clone(),
        })
        .collect();
    if !workspaces.iter().any(|w| w.id == 0) {
        workspaces.insert(
            0,
            Workspace {
                id: 0,
                name: "Main".to_string(),
                color: "grey".to_string(),
            },
        );
    }
    let mut next_ws_id: u32 = workspaces.iter().map(|w| w.id).max().unwrap_or(0) + 1;
    let mut active_ws: u32 = if workspaces.iter().any(|w| w.id == ws_file.active) {
        ws_file.active
    } else {
        0
    };
    // Where the user last was in each workspace, so switching back lands on the same tab.
    let mut ws_last_active: HashMap<u32, u32> = HashMap::new();
    let home_url = setup_home_page();

    let mut tabs: Vec<Tab> = Vec::new();
    let mut next_id: u32 = 1;

    // Which tabs to open: BROWSER_TABS env (for tests) > saved session > a single start page.
    let (startup_tabs, restore_active): (Vec<store::SessionTab>, usize) =
        if let Ok(env_tabs) = std::env::var("BROWSER_TABS") {
            let v: Vec<store::SessionTab> = env_tabs
                .split(',')
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty())
                .map(|u| store::SessionTab {
                    url: u,
                    pinned: false,
                    group: None,
                    workspace: 0,
                    last_used: 0,
                })
                .collect();
            let v = if v.is_empty() {
                vec![store::SessionTab {
                    url: home_url.clone(),
                    pinned: false,
                    group: None,
                    workspace: 0,
                    last_used: 0,
                }]
            } else {
                v
            };
            (v, 0)
        } else {
            let s = store::load_session();
            if s.tabs.is_empty() {
                (
                    vec![store::SessionTab {
                        url: home_url.clone(),
                        pinned: false,
                        group: None,
                        workspace: 0,
                        last_used: 0,
                    }],
                    0,
                )
            } else {
                (s.tabs, s.active)
            }
        };

    let restore_active = restore_active.min(startup_tabs.len().saturating_sub(1));
    let mut active: u32 = 0;
    for (i, st) in startup_tabs.iter().enumerate() {
        let is_active = i == restore_active;
        let mut t = if is_active || st.pinned {
            make_tab(
                &window,
                next_id,
                &st.url,
                spawn_proxy.clone(),
                &engine,
                &mut web_context,
                false,
            )
        } else {
            make_lazy_tab(next_id, &st.url)
        };
        t.pinned = st.pinned;
        // Restored tabs keep their real last-used time so the archiver measures true idleness
        // across restarts; older session files without it start the clock now.
        if st.last_used > 0 {
            t.last_used = st.last_used;
        }
        // Only keep a group ref if that group still exists.
        t.group = st.group.filter(|g| groups.iter().any(|gr| gr.id == *g));
        t.workspace = if workspaces.iter().any(|w| w.id == st.workspace) {
            st.workspace
        } else {
            0
        };
        if active == 0 || is_active {
            active = t.id;
        }
        tabs.push(t);
        next_id += 1;
    }
    // Drop any saved group that ended up with no member tabs.
    groups.retain(|g| tabs.iter().any(|t| t.group == Some(g.id)));
    // Keep the visible workspace consistent with the restored active tab.
    if let Some(t) = tabs.iter().find(|t| t.id == active) {
        active_ws = t.workspace;
    }
    // A URL passed on the command line opens as its own tab on top of the restored session.
    if let Some(u) = &cli_url {
        let mut t = make_tab(&window, next_id, u, spawn_proxy.clone(), &engine, &mut web_context, false);
        t.workspace = active_ws;
        active = t.id;
        tabs.push(t);
        next_id += 1;
    }

    relayout(&window, &chrome, &ai_sidebar, &tabs);
    activate(&window, &mut tabs, active);

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::NewEvents(StartCause::Init) => {
                relayout(&window, &chrome, &ai_sidebar, &tabs);
                activate(&window, &mut tabs, active);
            }

            Event::UserEvent(ue) => match ue {
                UserEvent::Navigate(input) => {
                    // A URL loads directly; a plain query goes to the configured search engine
                    // (DuckDuckGo /html/ by default), which Aperture re-skins on load.
                    let url = resolve_query(&input, &settings);
                    if let Some(t) = tabs.iter_mut().find(|t| t.id == active) {
                        t.url = url.clone();
                        if let Some(wv) = &t.webview {
                            let _ = wv.load_url(&url);
                        }
                    }
                }
                UserEvent::Back => nav_active(&tabs, active, false),
                UserEvent::Forward => nav_active(&tabs, active, true),
                UserEvent::Reload => act_on_active(&tabs, active, "location.reload()"),
                UserEvent::HardReload => {
                    if let Some(t) = tabs.iter().find(|t| t.id == active) {
                        if let Some(wv) = &t.webview {
                            hard_reload(wv);
                        }
                    }
                }

                UserEvent::NewTab => {
                    let mut t = make_tab(&window, next_id, &home_url, spawn_proxy.clone(), &engine, &mut web_context, false);
                    t.workspace = active_ws;
                    active = t.id;
                    tabs.push(t);
                    next_id += 1;
                    relayout(&window, &chrome, &ai_sidebar, &tabs);
                    activate(&window, &mut tabs, active);
                    push_tabs(&chrome, &tabs, active);
                    push_url_star(&chrome, &tabs, active, &bookmarks);
                    push_can_go(&chrome, &tabs, active);
                    persist_session(&tabs, active);
                    // A new tab is the new-tab page: put the cursor in the omnibox so typing starts
                    // there immediately. (HomeReady re-asserts this after the page loads.)
                    let _ = chrome.focus();
                    let _ = chrome.evaluate_script("window.__chrome&&window.__chrome.focusOmnibox()");
                }

                UserEvent::NewPrivateTab => {
                    // Built on the ephemeral private_context so its cookies/storage are isolated and
                    // gone next launch. Exempt from the idle sweep (see ReclaimIdleTabs), so it's never
                    // discarded and never rebuilt on the wrong context.
                    let mut t = make_tab(&window, next_id, &home_url, spawn_proxy.clone(), &engine, &mut private_context, true);
                    t.workspace = active_ws;
                    active = t.id;
                    tabs.push(t);
                    next_id += 1;
                    relayout(&window, &chrome, &ai_sidebar, &tabs);
                    activate(&window, &mut tabs, active);
                    push_tabs(&chrome, &tabs, active);
                    push_url_star(&chrome, &tabs, active, &bookmarks);
                    push_can_go(&chrome, &tabs, active);
                    persist_session(&tabs, active);
                    let _ = chrome.focus();
                    let _ = chrome.evaluate_script("window.__chrome&&window.__chrome.focusOmnibox()");
                }

                UserEvent::NewTabUrl(url, opener) => {
                    // The opener's page becomes the trail edge source for the new tab's first visit
                    // (private opener pages never enter the trail).
                    let opener_url = opener
                        .and_then(|oid| tabs.iter().find(|t| t.id == oid))
                        .filter(|t| !t.private)
                        .map(|t| t.url.clone());
                    // Air Traffic Control: a destination-host rule routes the link into its
                    // workspace (first match wins) and the view follows it there.
                    let dest_ws = settings
                        .atc_rules
                        .iter()
                        .find(|r| {
                            host_matches(&url, &r.pattern)
                                && workspaces.iter().any(|w| w.id == r.workspace)
                        })
                        .map(|r| r.workspace)
                        .unwrap_or(active_ws);
                    if dest_ws != active_ws {
                        ws_last_active.insert(active_ws, active);
                        active_ws = dest_ws;
                        SPLIT_TAB.store(0, Ordering::Relaxed);
                        persist_workspaces(&workspaces, active_ws);
                        push_workspaces(&chrome, &workspaces, active_ws);
                    }
                    let mut t = make_tab(&window, next_id, &url, spawn_proxy.clone(), &engine, &mut web_context, false);
                    t.trail_from = opener_url;
                    t.workspace = active_ws;
                    active = t.id;
                    tabs.push(t);
                    next_id += 1;
                    relayout(&window, &chrome, &ai_sidebar, &tabs);
                    activate(&window, &mut tabs, active);
                    push_tabs(&chrome, &tabs, active);
                    push_url_star(&chrome, &tabs, active, &bookmarks);
                    push_can_go(&chrome, &tabs, active);
                    persist_session(&tabs, active);
                }

                UserEvent::ReopenClosed => {
                    if let Some(url) = closed_stack.pop() {
                        let mut t = make_tab(&window, next_id, &url, spawn_proxy.clone(), &engine, &mut web_context, false);
                        t.workspace = active_ws;
                        active = t.id;
                        tabs.push(t);
                        next_id += 1;
                        relayout(&window, &chrome, &ai_sidebar, &tabs);
                        activate(&window, &mut tabs, active);
                        push_tabs(&chrome, &tabs, active);
                        push_url_star(&chrome, &tabs, active, &bookmarks);
                        push_can_go(&chrome, &tabs, active);
                        persist_session(&tabs, active);
                    }
                }
                UserEvent::Zoom(dir) => {
                    if let Some(z) = zoom_active(&tabs, active, dir) {
                        let host = host_of(&active_url(&tabs, active));
                        if !host.is_empty() {
                            if (z - 1.0).abs() < 0.001 {
                                zoom_levels.remove(&host);
                            } else {
                                zoom_levels.insert(host, z);
                            }
                            store::save_zoom(&zoom_levels);
                        }
                    }
                    push_zoom(&chrome, &tabs, active);
                }
                UserEvent::ZoomChanged(id) => {
                    // Fires on Ctrl+scroll (native WebView2 zoom) and on programmatic SetZoomFactor.
                    remember_zoom(&tabs, id, &mut zoom_levels);
                    if id == active {
                        push_zoom(&chrome, &tabs, active);
                    }
                }
                UserEvent::Stop => stop_active(&tabs, active),

                UserEvent::SwitchToIndex(i) => {
                    // Ctrl+1..9 index into the visible workspace's strip, not the global tab list.
                    let ids: Vec<u32> = tabs
                        .iter()
                        .filter(|t| t.workspace == active_ws)
                        .map(|t| t.id)
                        .collect();
                    let idx = if i == usize::MAX { ids.len().saturating_sub(1) } else { i };
                    if let Some(id) = ids.get(idx) {
                        active = *id;
                        if let Some(t) = tabs.iter_mut().find(|t| t.id == active) {
                            ensure_live(t, &window, spawn_proxy.clone(), &engine, &mut web_context);
                        }
                        relayout(&window, &chrome, &ai_sidebar, &tabs);
                        activate(&window, &mut tabs, active);
                        push_tabs(&chrome, &tabs, active);
                        push_url_star(&chrome, &tabs, active, &bookmarks);
                        push_can_go(&chrome, &tabs, active);
                        push_zoom(&chrome, &tabs, active);
                        persist_session(&tabs, active);
                    }
                }

                UserEvent::PageTitleChanged(id, title) => {
                    if !title.is_empty() {
                        // Visits are recorded at navigation time with a placeholder label; upgrade
                        // trail + history entries to the real document title once it arrives.
                        let mut titled_url: Option<String> = None;
                        if let Some(t) = tabs.iter_mut().find(|t| t.id == id) {
                            if !t.private {
                                titled_url = Some(t.url.clone());
                            }
                            t.title = title.clone();
                        }
                        if let Some(u) = titled_url {
                            if let Some(h) = history.iter_mut().find(|h| h.url == u) {
                                if h.title != title {
                                    h.title = title.clone();
                                    store::save_history(&history);
                                    push_history_source(&chrome, &history);
                                }
                            }
                            if settings.trail.enabled {
                                if let Some(v) = trail.iter_mut().rev().find(|v| v.url == u) {
                                    if v.title != title {
                                        v.title = title.clone();
                                        store::save_trail(&trail);
                                    }
                                }
                            }
                        }
                        push_tabs(&chrome, &tabs, active);
                        if id == active {
                            update_window_title(&window, &tabs, active);
                        }
                    }
                }
                UserEvent::PageFaviconChanged(id, uri) => {
                    if let Some(t) = tabs.iter_mut().find(|t| t.id == id) {
                        t.favicon = uri;
                    }
                    push_tabs(&chrome, &tabs, active);
                }
                UserEvent::PageAudioChanged(id, playing) => {
                    if let Some(t) = tabs.iter_mut().find(|t| t.id == id) {
                        t.audio = playing;
                    }
                    push_tabs(&chrome, &tabs, active);
                }
                UserEvent::DownloadStarted(id) => {
                    if let Some(t) = tabs.iter_mut().find(|t| t.id == id) {
                        t.downloading += 1;
                    }
                }
                UserEvent::DownloadEnded(id) => {
                    if let Some(t) = tabs.iter_mut().find(|t| t.id == id) {
                        t.downloading = t.downloading.saturating_sub(1);
                    }
                }
                UserEvent::DownloadAdded { dl_id, name, path } => {
                    let nj = serde_json::to_string(&name).unwrap_or_else(|_| "\"\"".into());
                    downloads.insert(0, DownloadItem { id: dl_id, name, path, state: 0 });
                    downloads.truncate(50);
                    push_downloads(&chrome, &downloads);
                    // Proactively surface the start (the toolbar button is easy to miss).
                    let _ = chrome.evaluate_script(&format!(
                        "window.__chrome&&window.__chrome.downloadStarted({nj})"
                    ));
                }
                UserEvent::DownloadStateChanged { dl_id, state } => {
                    if let Some(d) = downloads.iter_mut().find(|d| d.id == dl_id) {
                        d.state = state;
                    }
                    push_downloads(&chrome, &downloads);
                }
                UserEvent::OpenDownload(path) => {
                    if !path.is_empty() {
                        let _ = std::process::Command::new("explorer")
                            .arg(format!("/select,{path}"))
                            .spawn();
                    }
                }
                UserEvent::ClearDownloads => {
                    // Keep any still-in-progress downloads; drop the finished/failed ones.
                    downloads.retain(|d| d.state == 0);
                    push_downloads(&chrome, &downloads);
                }

                UserEvent::TabPrewake(id) => {
                    // Hover prewake: resume a merely-suspended tab (cheap) so the switch feels
                    // instant. Discarded tabs are NOT rebuilt on hover - that's a full renderer +
                    // page load, far too heavy to trigger by the pointer passing over the strip.
                    let mut woke = false;
                    if let Some(t) = tabs.iter_mut().find(|t| t.id == id) {
                        if t.suspended {
                            if let Some(wv) = &t.webview {
                                if resume_webview(wv) {
                                    t.suspended = false;
                                    t.last_active = std::time::Instant::now();
                                    t.last_used = store::unix_now();
                                    woke = true;
                                }
                            }
                        }
                    }
                    if woke {
                        push_tabs(&chrome, &tabs, active);
                    }
                }
                UserEvent::ToggleMute(id) => {
                    if let Some(t) = tabs.iter_mut().find(|t| t.id == id) {
                        if let Some(wv) = &t.webview {
                            if let Ok(c8) = wv.webview().cast::<ICoreWebView2_8>() {
                                unsafe {
                                    let mut m = windows_core::BOOL(0);
                                    let _ = c8.IsMuted(&mut m);
                                    let nm = !m.as_bool();
                                    let _ = c8.SetIsMuted(nm);
                                    t.muted = nm;
                                }
                            }
                        }
                    }
                    push_tabs(&chrome, &tabs, active);
                }

                UserEvent::MenuOpen => {
                    let size = window.inner_size().to_logical::<f64>(window.scale_factor());
                    // Float the dropdown over the live page: raise the chrome above the content
                    // webviews (they're created later, so they normally sit higher in z-order), then
                    // grow it full-window. Its background is transparent, so only the strip and the
                    // open dropdown paint; the page stays visible underneath. Clicks anywhere else
                    // land on the chrome, whose click-away handlers close the overlay.
                    if !raise_chrome() {
                        // Couldn't identify the chrome's child window at startup: fall back to the
                        // old behavior (hide the page while the overlay is up) so dropdowns never
                        // end up invisible underneath the content webviews.
                        for t in tabs.iter() {
                            if let Some(wv) = &t.webview {
                                let _ = wv.set_visible(false);
                            }
                        }
                        let _ = ai_sidebar.set_visible(false);
                    }
                    let _ = chrome.set_bounds(Rect {
                        position: LogicalPosition::new(0.0, 0.0).into(),
                        size: LogicalSize::new(size.width, size.height).into(),
                    });
                }
                UserEvent::MenuClose => {
                    let size = window.inner_size().to_logical::<f64>(window.scale_factor());
                    let _ = chrome.set_bounds(chrome_rect(size.width, size.height));
                    activate(&window, &mut tabs, active);
                    let _ = ai_sidebar.set_visible(AI_PANEL_W.load(Ordering::Relaxed) > 0);
                }
                UserEvent::PermissionRequest { id, origin, kind } => {
                    let host = host_of(&origin);
                    let host = if host.is_empty() { origin } else { host };
                    let oj = serde_json::to_string(&host).unwrap_or_else(|_| "\"\"".into());
                    let lj = serde_json::to_string(permission_label(kind))
                        .unwrap_or_else(|_| "\"\"".into());
                    let _ = chrome.evaluate_script(&format!(
                        "window.__chrome&&window.__chrome.showPermission({id},{oj},{lj})"
                    ));
                }
                UserEvent::PermissionDecide { id, allow } => {
                    PENDING_PERMS.with(|m| {
                        if let Some((args, def)) = m.borrow_mut().remove(&id) {
                            unsafe {
                                let _ = args.SetState(if allow {
                                    COREWEBVIEW2_PERMISSION_STATE_ALLOW
                                } else {
                                    COREWEBVIEW2_PERMISSION_STATE_DENY
                                });
                                let _ = def.Complete();
                            }
                        }
                    });
                }
                UserEvent::ReloadTab(id) => {
                    if let Some(t) = tabs.iter_mut().find(|t| t.id == id) {
                        ensure_live(t, &window, spawn_proxy.clone(), &engine, &mut web_context);
                        if let Some(wv) = &t.webview {
                            let _ = wv.evaluate_script("location.reload()");
                        }
                    }
                }
                UserEvent::DuplicateTab(id) => {
                    let src = tabs
                        .iter()
                        .find(|t| t.id == id)
                        .map(|t| (t.url.clone(), t.workspace));
                    if let Some((url, ws)) = src {
                        let mut t = make_tab(&window, next_id, &url, spawn_proxy.clone(), &engine, &mut web_context, false);
                        t.workspace = ws;
                        active = t.id;
                        tabs.push(t);
                        next_id += 1;
                        relayout(&window, &chrome, &ai_sidebar, &tabs);
                        activate(&window, &mut tabs, active);
                        push_tabs(&chrome, &tabs, active);
                        push_url_star(&chrome, &tabs, active, &bookmarks);
                        push_can_go(&chrome, &tabs, active);
                        persist_session(&tabs, active);
                    }
                }
                UserEvent::ToggleAi => {
                    // Master switch: if the assistant is disabled, the toggle is a no-op.
                    if settings.ai.enabled {
                        let opening = AI_PANEL_W.load(Ordering::Relaxed) == 0;
                        ai_anim_gen += 1;
                        if opening {
                            // The panel slides in from the right edge and the page's edge follows
                            // it frame by frame (AiSlide resizes the content each step), so the
                            // page reflows gradually instead of snapping to its narrower width.
                            let _ = ai_sidebar.set_bounds(ai_slide_rect(&window, 1.0));
                            let _ = ai_sidebar.set_visible(true);
                            spawn_ai_slide(spawn_proxy.clone(), ai_anim_gen, true);
                        } else {
                            // Slide out; the final frame hides the panel and gives the column back
                            // to the content (AiSlide arm). Stream/VRAM teardown starts immediately.
                            spawn_ai_slide(spawn_proxy.clone(), ai_anim_gen, false);
                            // Closing: cancel any in-flight stream, and (if enabled) free the model
                            // from VRAM. The model lazily reloads on the next ask.
                            if let Some(flag) = &ai_cancel {
                                flag.store(true, Ordering::Relaxed);
                            }
                            ai_cancel = None;
                            ai_req += 1;
                            let _ = ai_sidebar.evaluate_script("window.__ai&&window.__ai.done(null)");
                            if settings.ai.unload_on_close {
                                // Use ai_cfg so we unload the SAME (blank-coerced) model that gets loaded.
                                let cfg = ai_cfg(&settings);
                                std::thread::spawn(move || ai::unload(&cfg.host, &cfg.model));
                            }
                        }
                    }
                }
                UserEvent::AiSlide { gen, t, opening } => {
                    // A stale generation means a newer toggle superseded this slide; drop the frame.
                    if gen == ai_anim_gen {
                        let ease = 1.0 - (1.0 - t).powi(3);
                        let off = if opening { 1.0 - ease } else { ease };
                        // The page's edge tracks the panel's visible width each frame, so opening
                        // squeezes the page gradually and closing hands the space back gradually.
                        AI_PANEL_W.store((AI_W * (1.0 - off)).round() as u32, Ordering::Relaxed);
                        let _ = ai_sidebar.set_bounds(ai_slide_rect(&window, off));
                        resize_content(&window, &tabs, active);
                        if t >= 1.0 {
                            if opening {
                                AI_PANEL_W.store(AI_W as u32, Ordering::Relaxed);
                                relayout(&window, &chrome, &ai_sidebar, &tabs);
                                let _ = ai_sidebar
                                    .evaluate_script("window.__ai&&window.__ai.focus()");
                            } else {
                                // Fully off-screen: hide and settle the final layout.
                                let _ = ai_sidebar.set_visible(false);
                                AI_PANEL_W.store(0, Ordering::Relaxed);
                                relayout(&window, &chrome, &ai_sidebar, &tabs);
                                activate(&window, &mut tabs, active);
                            }
                        }
                    }
                }
                UserEvent::AiReady => {
                    let js = format!(
                        "window.__ai&&window.__ai.setModel({})",
                        serde_json::to_string(&settings.ai.model).unwrap_or_else(|_| "\"\"".into())
                    );
                    let _ = ai_sidebar.evaluate_script(&js);
                    push_sidebar_ai_cfg(&ai_sidebar, &settings);
                    let _ = ai_sidebar.evaluate_script(&sidebar_theme_js(&settings.theme));
                }
                UserEvent::AiAsk { scope, prompt } if settings.ai.enabled => {
                    // New request: cancel any in-flight stream, bump the id.
                    if let Some(flag) = &ai_cancel {
                        flag.store(true, Ordering::Relaxed);
                    }
                    ai_req += 1;
                    let req = ai_req;
                    // Start a conversational turn: remember the question, reset the answer buffer.
                    ai_turn_user = Some(prompt.clone());
                    ai_turn_answer.clear();
                    if scope == "vision" {
                        // Opt-in fallback: screenshot the active page and send it to the vision model
                        // (slower; only when DOM text isn't enough). Capture is async -> AiVisionContext.
                        if settings.ai.vision {
                            capture_active_preview(&tabs, active, req, prompt, spawn_proxy.clone());
                        } else {
                            let _ = spawn_proxy.send_event(UserEvent::AiDone {
                                req,
                                error: Some("the screenshot/vision feature is turned off in settings".into()),
                            });
                        }
                    } else if scope == "web" {
                        // Agentic web search: search DDG, read the top results, answer with citations.
                        if settings.ai.web_search {
                            ai_cancel = Some(spawn_web_research(
                                ai_cfg(&settings),
                                settings.ai.web_results as usize,
                                prompt,
                                ai_history.clone(),
                                req,
                                spawn_proxy.clone(),
                            ));
                        } else {
                            let _ = spawn_proxy.send_event(UserEvent::AiDone {
                                req,
                                error: Some("web search is turned off in settings".into()),
                            });
                        }
                    } else {
                        let page_wv = if scope == "page" {
                            tabs.iter()
                                .find(|t| t.id == active)
                                .and_then(|t| t.webview.as_ref())
                        } else {
                            None
                        };
                        if let Some(wv) = page_wv {
                            let p = spawn_proxy.clone();
                            let pr = prompt.clone();
                            let _ = wv.evaluate_script_with_callback(AI_EXTRACT_JS, move |res| {
                                let _ = p.send_event(UserEvent::AiContext {
                                    req,
                                    prompt: pr.clone(),
                                    page_text: res,
                                });
                            });
                        } else {
                            let _ = spawn_proxy.send_event(UserEvent::AiContext {
                                req,
                                prompt,
                                page_text: String::new(),
                            });
                        }
                    }
                }
                // Assistant disabled: ignore ask requests.
                UserEvent::AiAsk { .. } => {}
                UserEvent::AiContext { req, prompt, page_text } => {
                    if req == ai_req {
                        let base = build_ai_messages(&prompt, &page_text);
                        // Replay prior turns only for chat (ai_turn_user set). Translate Page also routes
                        // through here but is a one-shot command, so it skips the conversation memory.
                        let messages = if ai_turn_user.is_some() {
                            with_ai_history(base, &ai_history)
                        } else {
                            base
                        };
                        ai_cancel =
                            Some(spawn_ai_stream(ai_cfg(&settings), messages, req, spawn_proxy.clone()));
                    }
                }
                UserEvent::AiVisionContext { req, prompt, image } => {
                    if req == ai_req {
                        let question = if prompt.trim().is_empty() {
                            "What's on this page? Describe what you see.".to_string()
                        } else {
                            prompt
                        };
                        let messages = with_ai_history(
                            vec![
                                ai::Msg::system(
                                    "You are Aperture's built-in assistant. The user has \
                                     attached a screenshot of the web page they're looking at. Answer using \
                                     what is visible in the image. Be concise. Plain text, no markdown headers.",
                                ),
                                ai::Msg::user_image(question, vec![image]),
                            ],
                            &ai_history,
                        );
                        ai_cancel =
                            Some(spawn_ai_stream(ai_cfg(&settings), messages, req, spawn_proxy.clone()));
                    }
                }
                UserEvent::AiToken { req, delta } => {
                    if req == ai_req {
                        // Accumulate the reply so it can be saved to conversation memory when done.
                        ai_turn_answer.push_str(&delta);
                        let js = format!(
                            "window.__ai&&window.__ai.token({})",
                            serde_json::to_string(&delta).unwrap_or_else(|_| "\"\"".into())
                        );
                        let _ = ai_sidebar.evaluate_script(&js);
                    }
                }
                UserEvent::AiWebStatus { req, text } => {
                    if req == ai_req {
                        let js = format!(
                            "window.__ai&&window.__ai.note({})",
                            serde_json::to_string(&text).unwrap_or_else(|_| "\"\"".into())
                        );
                        let _ = ai_sidebar.evaluate_script(&js);
                    }
                }
                UserEvent::AiSources { req, sources } => {
                    if req == ai_req {
                        let arr: Vec<serde_json::Value> = sources
                            .iter()
                            .map(|(title, url)| serde_json::json!({ "title": title, "url": url }))
                            .collect();
                        let js = format!(
                            "window.__ai&&window.__ai.sources({})",
                            serde_json::to_string(&arr).unwrap_or_else(|_| "[]".into())
                        );
                        let _ = ai_sidebar.evaluate_script(&js);
                    }
                }
                UserEvent::AiDone { req, error } => {
                    if req == ai_req {
                        ai_cancel = None;
                        // Save this turn to conversation memory so the next question sees it. Only on a
                        // clean finish (no error) and only for conversational turns (ai_turn_user set);
                        // the req == ai_req guard above already excludes superseded/cancelled streams.
                        let turn_user = ai_turn_user.take();
                        if error.is_none() {
                            if let Some(u) = turn_user {
                                commit_ai_turn(&mut ai_history, &u, &ai_turn_answer);
                            }
                        }
                        ai_turn_answer.clear();
                        let arg = match &error {
                            Some(e) => serde_json::to_string(e).unwrap_or_else(|_| "\"error\"".into()),
                            None => "null".to_string(),
                        };
                        let _ = ai_sidebar
                            .evaluate_script(&format!("window.__ai&&window.__ai.done({arg})"));
                    }
                }
                UserEvent::AiStop => {
                    if let Some(flag) = &ai_cancel {
                        flag.store(true, Ordering::Relaxed);
                    }
                    ai_cancel = None;
                    ai_req += 1; // drop late tokens from the stopped stream
                    // A stopped turn isn't remembered (a partial answer is rarely worth replaying, and
                    // the host-initiated paths fire ai_stop to interrupt, which must not commit the new
                    // turn). Drop the in-flight turn state; completed turns are saved in AiDone.
                    ai_turn_user = None;
                    ai_turn_answer.clear();
                    let _ = ai_sidebar.evaluate_script("window.__ai&&window.__ai.done(null)");
                }
                UserEvent::AiNewChat => {
                    // New chat: cancel any stream and forget the remembered turns.
                    if let Some(flag) = &ai_cancel {
                        flag.store(true, Ordering::Relaxed);
                    }
                    ai_cancel = None;
                    ai_req += 1;
                    ai_history.clear();
                    ai_turn_user = None;
                    ai_turn_answer.clear();
                    let _ = ai_sidebar.evaluate_script("window.__ai&&window.__ai.clear()");
                }
                UserEvent::AiModels(models) => {
                    let arr = serde_json::to_string(&models).unwrap_or_else(|_| "[]".into());
                    let _ = chrome.evaluate_script(&format!(
                        "window.__chrome&&window.__chrome.setAiModels({arr})"
                    ));
                }
                UserEvent::AiOmnibox(q) if settings.ai.enabled && settings.ai.omnibox_ask => {
                    open_ai_panel(&window, &chrome, &ai_sidebar, &mut tabs, active, &mut ai_anim_gen, &spawn_proxy);
                    if let Some(flag) = &ai_cancel {
                        flag.store(true, Ordering::Relaxed);
                    }
                    ai_req += 1;
                    let req = ai_req;
                    ai_turn_user = Some(q.clone());
                    ai_turn_answer.clear();
                    let label = serde_json::to_string(&q).unwrap_or_else(|_| "\"\"".into());
                    let _ = ai_sidebar.evaluate_script(&format!(
                        "window.__ai&&window.__ai.startExternal({label})"
                    ));
                    let messages = with_ai_history(
                        vec![
                            ai::Msg::system(
                                "You are Aperture's built-in assistant. Be concise and direct. Plain \
                                 text, no markdown headers.",
                            ),
                            ai::Msg::user(q),
                        ],
                        &ai_history,
                    );
                    ai_cancel =
                        Some(spawn_ai_stream(ai_cfg(&settings), messages, req, spawn_proxy.clone()));
                }
                UserEvent::AiOmnibox(_) => {}
                UserEvent::AiSelection { action, text }
                    if settings.ai.enabled && settings.ai.selection_pill =>
                {
                    open_ai_panel(&window, &chrome, &ai_sidebar, &mut tabs, active, &mut ai_anim_gen, &spawn_proxy);
                    if let Some(flag) = &ai_cancel {
                        flag.store(true, Ordering::Relaxed);
                    }
                    ai_req += 1;
                    let req = ai_req;
                    let snippet: String = text.chars().take(AI_CTX_CHARS).collect();
                    let (instruction, verb) = match action.as_str() {
                        "translate" => (
                            "Translate the following text. If it is English, translate it to Spanish; \
                             otherwise translate it to English. Output only the translation.",
                            "Translate",
                        ),
                        "rewrite" => (
                            "Rewrite the following text to be clearer and more concise while keeping \
                             its meaning. Output only the rewritten text.",
                            "Rewrite",
                        ),
                        _ => ("Explain the following text clearly and concisely.", "Explain"),
                    };
                    let preview: String = snippet.chars().take(70).collect();
                    let ellipsis = if snippet.chars().count() > 70 { "\u{2026}" } else { "" };
                    let label = format!("{verb}: \u{201c}{preview}{ellipsis}\u{201d}");
                    let label_js = serde_json::to_string(&label).unwrap_or_else(|_| "\"\"".into());
                    ai_turn_user = Some(label.clone());
                    ai_turn_answer.clear();
                    let _ = ai_sidebar.evaluate_script(&format!(
                        "window.__ai&&window.__ai.startExternal({label_js})"
                    ));
                    let messages = with_ai_history(
                        vec![
                            ai::Msg::system(
                                "You are Aperture's built-in assistant. Be concise. \
                                 Plain text, no markdown headers.",
                            ),
                            ai::Msg::user(format!("{instruction}\n\n\"\"\"\n{snippet}\n\"\"\"")),
                        ],
                        &ai_history,
                    );
                    ai_cancel =
                        Some(spawn_ai_stream(ai_cfg(&settings), messages, req, spawn_proxy.clone()));
                }
                UserEvent::AiSelection { .. } => {}
                UserEvent::TranslatePage if settings.ai.enabled => {
                    open_ai_panel(&window, &chrome, &ai_sidebar, &mut tabs, active, &mut ai_anim_gen, &spawn_proxy);
                    if let Some(flag) = &ai_cancel {
                        flag.store(true, Ordering::Relaxed);
                    }
                    ai_req += 1;
                    let req = ai_req;
                    // One-shot command: don't replay or record conversation history (AiContext checks
                    // ai_turn_user before splicing; AiDone checks it before committing).
                    ai_turn_user = None;
                    ai_turn_answer.clear();
                    let lang = if settings.ai.translate_to.trim().is_empty() {
                        "English".to_string()
                    } else {
                        settings.ai.translate_to.clone()
                    };
                    let label = format!("Translate page \u{2192} {lang}");
                    let label_js = serde_json::to_string(&label).unwrap_or_else(|_| "\"\"".into());
                    let _ = ai_sidebar.evaluate_script(&format!(
                        "window.__ai&&window.__ai.startExternal({label_js})"
                    ));
                    let prompt = format!(
                        "Translate the page content into {lang}. Output only the translation, \
                         preserving the structure and headings. Do not add commentary."
                    );
                    if let Some(wv) = tabs
                        .iter()
                        .find(|t| t.id == active)
                        .and_then(|t| t.webview.as_ref())
                    {
                        let p = spawn_proxy.clone();
                        let _ = wv.evaluate_script_with_callback(AI_EXTRACT_JS, move |res| {
                            let _ = p.send_event(UserEvent::AiContext {
                                req,
                                prompt: prompt.clone(),
                                page_text: res,
                            });
                        });
                    } else {
                        let _ = spawn_proxy.send_event(UserEvent::AiDone {
                            req,
                            error: Some("no page to translate".into()),
                        });
                    }
                }
                UserEvent::TranslatePage => {}
                UserEvent::AiOrganizeTabs if settings.ai.enabled => {
                    open_ai_panel(&window, &chrome, &ai_sidebar, &mut tabs, active, &mut ai_anim_gen, &spawn_proxy);
                    let _ = ai_sidebar
                        .evaluate_script("window.__ai&&window.__ai.startExternal(\"Organize my open tabs\")");
                    let list: Vec<serde_json::Value> = tabs
                        .iter()
                        .filter(|t| {
                            !t.url.is_empty()
                                && !t.url.contains("home.html")
                                && !t.url.starts_with("about:")
                        })
                        .map(|t| serde_json::json!({ "id": t.id, "title": t.title, "url": t.url }))
                        .collect();
                    if list.len() < 2 {
                        let _ = ai_sidebar.evaluate_script(
                            "window.__ai&&window.__ai.note(\"Open a few more tabs first \\u2014 nothing to organize.\");window.__ai&&window.__ai.done(null)",
                        );
                    } else {
                        let _ = ai_sidebar.evaluate_script(
                            "window.__ai&&window.__ai.note(\"Reading your tabs and grouping by topic...\")",
                        );
                        if let Some(flag) = &ai_cancel {
                            flag.store(true, Ordering::Relaxed);
                        }
                        ai_cancel = None;
                        ai_req += 1;
                        // Utility run (tab grouping), not chat: keep it out of conversation memory.
                        ai_turn_user = None;
                        ai_turn_answer.clear();
                        let req = ai_req;
                        let tabs_json = serde_json::to_string(&list).unwrap_or_else(|_| "[]".into());
                        let messages = vec![
                            ai::Msg::system(
                                "You organize browser tabs into a few topic-based groups. Reply with \
                                 ONLY a JSON array, no prose and no code fences. Each element is \
                                 {\"name\": short group name, \"tabs\": [tab ids]}. Use 2 to 6 groups. \
                                 Each tab id appears in at most one group; omit tabs that don't fit.",
                            ),
                            ai::Msg::user(format!("My open tabs:\n{tabs_json}")),
                        ];
                        let cfg = ai_cfg(&settings);
                        let proxy = spawn_proxy.clone();
                        std::thread::spawn(move || {
                            let mut acc = String::new();
                            let _ = ai::chat_stream(
                                &cfg.host,
                                &cfg.model,
                                &cfg.keep_alive,
                                &messages,
                                |d| acc.push_str(d),
                                || false,
                            );
                            let _ = proxy.send_event(UserEvent::AiTabPlan { req, plan: acc });
                        });
                    }
                }
                UserEvent::AiOrganizeTabs => {}
                UserEvent::AiTabPlan { req, plan } => {
                    if req == ai_req {
                        #[derive(serde::Deserialize)]
                        struct PlanGroup {
                            #[serde(default)]
                            name: String,
                            #[serde(default)]
                            tabs: Vec<u32>,
                        }
                        // Extract the JSON array even if the model wrapped it in text/fences.
                        let parsed: Option<Vec<PlanGroup>> =
                            match (plan.find('['), plan.rfind(']')) {
                                (Some(s), Some(e)) if e > s => serde_json::from_str(&plan[s..=e]).ok(),
                                _ => None,
                            };
                        let mut made = 0usize;
                        let mut grouped = 0usize;
                        if let Some(plan_groups) = parsed {
                            for t in tabs.iter_mut() {
                                t.group = None;
                            }
                            groups.clear();
                            for pg in plan_groups {
                                let valid: Vec<u32> = pg
                                    .tabs
                                    .iter()
                                    .copied()
                                    .filter(|id| tabs.iter().any(|t| t.id == *id))
                                    .collect();
                                if valid.is_empty() {
                                    continue;
                                }
                                let id = next_group_id;
                                next_group_id += 1;
                                let color = GROUP_COLORS[made % GROUP_COLORS.len()].to_string();
                                let name = if pg.name.trim().is_empty() {
                                    format!("Group {}", made + 1)
                                } else {
                                    pg.name.trim().to_string()
                                };
                                groups.push(TabGroup { id, name, color, collapsed: false });
                                for tid in valid {
                                    if let Some(t) = tabs.iter_mut().find(|t| t.id == tid) {
                                        t.group = Some(id);
                                        grouped += 1;
                                    }
                                }
                                made += 1;
                            }
                            normalize_groups(&mut tabs);
                            save_groups(&groups);
                            push_groups(&chrome, &groups);
                            push_tabs(&chrome, &tabs, active);
                            persist_session(&tabs, active);
                        }
                        let msg = if made > 0 {
                            format!("Grouped {grouped} tabs into {made} groups.")
                        } else {
                            "Couldn't organize the tabs (the model didn't return a usable grouping)."
                                .to_string()
                        };
                        let msg_js = serde_json::to_string(&msg).unwrap_or_else(|_| "\"\"".into());
                        let _ = ai_sidebar
                            .evaluate_script(&format!("window.__ai&&window.__ai.note({msg_js})"));
                        let _ = ai_sidebar.evaluate_script("window.__ai&&window.__ai.done(null)");
                    }
                }
                UserEvent::ToggleFind => {
                    let _ = chrome.evaluate_script("window.__chrome&&window.__chrome.toggleFind()");
                }
                UserEvent::FindInPage(q, forward) => {
                    if let Some(t) = tabs.iter().find(|t| t.id == active) {
                        if let Some(wv) = &t.webview {
                            if let Ok(qj) = serde_json::to_string(&q) {
                                let js = format!("{FIND_JS}\nwindow.__apFind.search({qj},{forward})");
                                let p = spawn_proxy.clone();
                                let _ = wv.evaluate_script_with_callback(&js, move |res| {
                                    let _ = p.send_event(UserEvent::FindResult(res));
                                });
                            }
                        }
                    }
                }
                UserEvent::FindClear => {
                    if let Some(t) = tabs.iter().find(|t| t.id == active) {
                        if let Some(wv) = &t.webview {
                            let _ = wv.evaluate_script("window.__apFind&&window.__apFind.clear()");
                        }
                    }
                }
                UserEvent::FindResult(json) => {
                    // `json` is a JS object literal ({count,index}) straight from the page.
                    let arg = if json.trim().is_empty() || json == "null" {
                        "{}".to_string()
                    } else {
                        json
                    };
                    let _ = chrome
                        .evaluate_script(&format!("window.__chrome&&window.__chrome.setFind({arg})"));
                }
                UserEvent::RestoreClosedWindow => {
                    let s = store::load_closed_session();
                    if !s.tabs.is_empty() {
                        tabs.clear();
                        active = 0;
                        let restore_active = s.active.min(s.tabs.len().saturating_sub(1));
                        for (i, st) in s.tabs.iter().enumerate() {
                            let is_active = i == restore_active;
                            let mut t = if is_active || st.pinned {
                                make_tab(
                                    &window,
                                    next_id,
                                    &st.url,
                                    spawn_proxy.clone(),
                                    &engine,
                                    &mut web_context,
                                    false,
                                )
                            } else {
                                make_lazy_tab(next_id, &st.url)
                            };
                            t.pinned = st.pinned;
                            if st.last_used > 0 {
                                t.last_used = st.last_used;
                            }
                            t.group = st.group.filter(|g| groups.iter().any(|gr| gr.id == *g));
                            t.workspace = if workspaces.iter().any(|w| w.id == st.workspace) {
                                st.workspace
                            } else {
                                active_ws
                            };
                            if active == 0 || is_active {
                                active = t.id;
                            }
                            tabs.push(t);
                            next_id += 1;
                        }
                        store::clear_closed_session();
                        let _ = chrome.evaluate_script(
                            "window.__chrome&&window.__chrome.showRestore(false,0)",
                        );
                        relayout(&window, &chrome, &ai_sidebar, &tabs);
                        activate(&window, &mut tabs, active);
                        push_groups(&chrome, &groups);
                        push_tabs(&chrome, &tabs, active);
                        push_url_star(&chrome, &tabs, active, &bookmarks);
                        push_can_go(&chrome, &tabs, active);
                        push_zoom(&chrome, &tabs, active);
                        persist_session(&tabs, active);
                    }
                }
                UserEvent::OpenPalette => {
                    enter_palette_modal(&window, &chrome, &ai_sidebar, &tabs, active);
                }
                UserEvent::ClosePalette => {
                    exit_palette_modal(&window, &chrome, &ai_sidebar, &mut tabs, active);
                }
                UserEvent::ToggleReader => {
                    act_on_active(&tabs, active, READER_JS);
                }
                UserEvent::Print => {
                    print_active(&tabs, active);
                }
                UserEvent::Screenshot => {
                    save_active_screenshot(&tabs, active);
                }
                UserEvent::ToggleSplit => {
                    if SPLIT_TAB.load(Ordering::Relaxed) != 0 {
                        SPLIT_TAB.store(0, Ordering::Relaxed);
                    } else {
                        // Pair with the next tab in this workspace, or the previous one if the
                        // active tab is last.
                        let ws_tabs: Vec<&Tab> = tabs
                            .iter()
                            .filter(|t| t.workspace == active_ws)
                            .collect();
                        let i = ws_tabs.iter().position(|t| t.id == active).unwrap_or(0);
                        let partner = ws_tabs
                            .get(i + 1)
                            .or_else(|| if i > 0 { ws_tabs.get(i - 1) } else { None })
                            .map(|t| t.id);
                        if let Some(p) = partner {
                            if let Some(t) = tabs.iter_mut().find(|t| t.id == p) {
                                ensure_live(t, &window, spawn_proxy.clone(), &engine, &mut web_context);
                            }
                            SPLIT_TAB.store(p, Ordering::Relaxed);
                        }
                    }
                    relayout(&window, &chrome, &ai_sidebar, &tabs);
                    activate(&window, &mut tabs, active);
                    push_tabs(&chrome, &tabs, active);
                }
                UserEvent::OpenHistory => {
                    // Ctrl+H toggles: a second press while the viewer is up closes it. The close
                    // goes through the chrome so the page can fade out first; it sends
                    // close_history when the fade ends, which does the real exit below.
                    if history_open {
                        history_open = false;
                        let _ = chrome.evaluate_script(
                            "window.__chrome&&window.__chrome.closeHistoryAnimated()",
                        );
                    } else {
                        history_open = true;
                        enter_history_modal(&window, &chrome, &ai_sidebar, &tabs, active, &history);
                    }
                }
                UserEvent::CloseHistory => {
                    history_open = false;
                    exit_history_modal(&window, &chrome, &ai_sidebar, &mut tabs, active);
                }
                UserEvent::HistoryOpen(url) => {
                    history_open = false;
                    exit_history_modal(&window, &chrome, &ai_sidebar, &mut tabs, active);
                    if let Some(t) = tabs.iter_mut().find(|t| t.id == active) {
                        t.url = url.clone();
                        if let Some(wv) = &t.webview {
                            let _ = wv.load_url(&url);
                        }
                    }
                    persist_session(&tabs, active);
                }
                UserEvent::HistoryDelete(url) => {
                    history.retain(|h| h.url != url);
                    store::save_history(&history);
                    push_history(&chrome, &history);
                    push_history_source(&chrome, &history);
                }
                UserEvent::TrailClear => {
                    trail.clear();
                    store::save_trail(&trail);
                    inject_home_data(&tabs, active, &history, &links, &settings, &trail);
                }
                UserEvent::OpenExternal(url) => {
                    // Bring the window forward, then open like any page-initiated link.
                    window.set_minimized(false);
                    window.set_focus();
                    let _ = spawn_proxy.send_event(UserEvent::NewTabUrl(url, None));
                }
                UserEvent::SetDefaultBrowser => {
                    let ok = register_as_default_browser();
                    if ok {
                        // Windows owns the final switch; open its Default-apps page to confirm.
                        let _ = std::process::Command::new("explorer.exe")
                            .arg("ms-settings:defaultapps")
                            .spawn();
                    }
                    let _ = chrome.evaluate_script(&format!(
                        "window.__chrome&&window.__chrome.defaultBrowserMsg({ok})"
                    ));
                }
                UserEvent::SwitchProfile(name) => {
                    let valid = name == "Default" || profiles.list.contains(&name);
                    if valid && name != current_profile {
                        // Leave this profile exactly like a window close (session saved for the
                        // restore prompt), point the registry at the target, free our mutex so
                        // the successor can start, and hand off.
                        save_closed_window_session(&tabs, active);
                        store::clear_session();
                        profiles.last = name.clone();
                        store::save_profiles(&profiles);
                        release_instance_mutex();
                        if let Ok(exe) = std::env::current_exe() {
                            let _ = std::process::Command::new(exe)
                                .arg("--profile")
                                .arg(&name)
                                .spawn();
                        }
                        *control_flow = ControlFlow::Exit;
                    }
                }
                UserEvent::CreateProfile(name) => {
                    let name = name.trim().to_string();
                    let ok = !name.is_empty()
                        && name != "Default"
                        && !profiles.list.contains(&name)
                        && !store::slug(&name).is_empty();
                    if ok {
                        profiles.list.push(name.clone());
                        store::save_profiles(&profiles);
                        push_profiles(&chrome, &profiles, &current_profile);
                        let _ = spawn_proxy.send_event(UserEvent::SwitchProfile(name));
                    }
                }
                UserEvent::DeleteProfile(name) => {
                    if name != "Default" && name != current_profile {
                        if let Some(pos) = profiles.list.iter().position(|p| *p == name) {
                            profiles.list.remove(pos);
                            store::save_profiles(&profiles);
                            // Best effort: the directory may be locked if that profile is open
                            // in another window right now.
                            let dir = store::root_dir().join("profiles").join(store::slug(&name));
                            let _ = std::fs::remove_dir_all(&dir);
                            push_profiles(&chrome, &profiles, &current_profile);
                        }
                    }
                }
                UserEvent::ProfilesAsk(on) => {
                    profiles.ask_at_startup = on;
                    store::save_profiles(&profiles);
                }
                UserEvent::SwitchWorkspace(ws) => {
                    if ws != active_ws && workspaces.iter().any(|w| w.id == ws) {
                        ws_last_active.insert(active_ws, active);
                        active_ws = ws;
                        // Split view pairs tabs within one workspace; drop it across a switch.
                        SPLIT_TAB.store(0, Ordering::Relaxed);
                        // Land on this workspace's last active tab, else its first, else a fresh
                        // home tab (a workspace is never shown empty).
                        let target = ws_last_active
                            .get(&ws)
                            .copied()
                            .filter(|id| tabs.iter().any(|t| t.id == *id && t.workspace == ws))
                            .or_else(|| tabs.iter().find(|t| t.workspace == ws).map(|t| t.id));
                        match target {
                            Some(id) => active = id,
                            None => {
                                let mut t = make_tab(&window, next_id, &home_url, spawn_proxy.clone(), &engine, &mut web_context, false);
                                t.workspace = ws;
                                active = t.id;
                                tabs.push(t);
                                next_id += 1;
                            }
                        }
                        if let Some(t) = tabs.iter_mut().find(|t| t.id == active) {
                            ensure_live(t, &window, spawn_proxy.clone(), &engine, &mut web_context);
                        }
                        persist_workspaces(&workspaces, active_ws);
                        push_workspaces(&chrome, &workspaces, active_ws);
                        relayout(&window, &chrome, &ai_sidebar, &tabs);
                        activate(&window, &mut tabs, active);
                        push_tabs(&chrome, &tabs, active);
                        push_url_star(&chrome, &tabs, active, &bookmarks);
                        push_can_go(&chrome, &tabs, active);
                        push_zoom(&chrome, &tabs, active);
                        persist_session(&tabs, active);
                    }
                }
                UserEvent::NewWorkspace => {
                    let id = next_ws_id;
                    next_ws_id += 1;
                    let color = GROUP_COLORS[workspaces.len() % GROUP_COLORS.len()].to_string();
                    workspaces.push(Workspace {
                        id,
                        name: format!("Space {}", workspaces.len() + 1),
                        color,
                    });
                    persist_workspaces(&workspaces, active_ws);
                    push_workspaces(&chrome, &workspaces, active_ws);
                    let _ = spawn_proxy.send_event(UserEvent::SwitchWorkspace(id));
                }
                UserEvent::RenameWorkspace { ws, name } => {
                    if let Some(w) = workspaces.iter_mut().find(|w| w.id == ws) {
                        w.name = name;
                    }
                    persist_workspaces(&workspaces, active_ws);
                    push_workspaces(&chrome, &workspaces, active_ws);
                }
                UserEvent::SetWorkspaceColor { ws, color } => {
                    if let Some(w) = workspaces.iter_mut().find(|w| w.id == ws) {
                        w.color = color;
                    }
                    persist_workspaces(&workspaces, active_ws);
                    push_workspaces(&chrome, &workspaces, active_ws);
                }
                UserEvent::DeleteWorkspace(ws) => {
                    if ws != 0 && workspaces.iter().any(|w| w.id == ws) {
                        // Close the workspace's tabs; each lands on the reopen stack so this is
                        // recoverable with Ctrl+Shift+T.
                        let doomed: Vec<u32> = tabs
                            .iter()
                            .filter(|t| t.workspace == ws)
                            .map(|t| t.id)
                            .collect();
                        for id in doomed {
                            if let Some(pos) = tabs.iter().position(|t| t.id == id) {
                                remember_closed(&mut closed_stack, &tabs[pos].url);
                                tabs.remove(pos);
                            }
                        }
                        if SPLIT_TAB.load(Ordering::Relaxed) != 0
                            && !tabs.iter().any(|t| t.id == SPLIT_TAB.load(Ordering::Relaxed))
                        {
                            SPLIT_TAB.store(0, Ordering::Relaxed);
                        }
                        workspaces.retain(|w| w.id != ws);
                        ws_last_active.remove(&ws);
                        if prune_empty_groups(&mut groups, &tabs) {
                            save_groups(&groups);
                            push_groups(&chrome, &groups);
                        }
                        // If the deleted workspace was on screen, fall back to Main.
                        if active_ws == ws || !tabs.iter().any(|t| t.id == active) {
                            active_ws = 0;
                            let target = ws_last_active
                                .get(&0)
                                .copied()
                                .filter(|id| tabs.iter().any(|t| t.id == *id && t.workspace == 0))
                                .or_else(|| tabs.iter().find(|t| t.workspace == 0).map(|t| t.id));
                            match target {
                                Some(id) => active = id,
                                None => {
                                    let mut t = make_tab(&window, next_id, &home_url, spawn_proxy.clone(), &engine, &mut web_context, false);
                                    t.workspace = 0;
                                    active = t.id;
                                    tabs.push(t);
                                    next_id += 1;
                                }
                            }
                            if let Some(t) = tabs.iter_mut().find(|t| t.id == active) {
                                ensure_live(t, &window, spawn_proxy.clone(), &engine, &mut web_context);
                            }
                        }
                        persist_workspaces(&workspaces, active_ws);
                        push_workspaces(&chrome, &workspaces, active_ws);
                        relayout(&window, &chrome, &ai_sidebar, &tabs);
                        activate(&window, &mut tabs, active);
                        push_tabs(&chrome, &tabs, active);
                        push_url_star(&chrome, &tabs, active, &bookmarks);
                        push_can_go(&chrome, &tabs, active);
                        persist_session(&tabs, active);
                    }
                }
                UserEvent::MoveTabToWorkspace { tab, ws } => {
                    if workspaces.iter().any(|w| w.id == ws) {
                        let mut moved = false;
                        if let Some(t) = tabs.iter_mut().find(|t| t.id == tab) {
                            if t.workspace != ws {
                                t.workspace = ws;
                                t.group = None;
                                moved = true;
                            }
                        }
                        if moved {
                            if SPLIT_TAB.load(Ordering::Relaxed) == tab {
                                SPLIT_TAB.store(0, Ordering::Relaxed);
                            }
                            // If the moved tab was on screen, stay in this workspace on the
                            // nearest remaining tab (or a fresh home tab).
                            if tab == active {
                                let target = tabs
                                    .iter()
                                    .find(|t| t.workspace == active_ws)
                                    .map(|t| t.id);
                                match target {
                                    Some(id) => active = id,
                                    None => {
                                        let mut t = make_tab(&window, next_id, &home_url, spawn_proxy.clone(), &engine, &mut web_context, false);
                                        t.workspace = active_ws;
                                        active = t.id;
                                        tabs.push(t);
                                        next_id += 1;
                                    }
                                }
                                if let Some(t) = tabs.iter_mut().find(|t| t.id == active) {
                                    ensure_live(t, &window, spawn_proxy.clone(), &engine, &mut web_context);
                                }
                            }
                            normalize_groups(&mut tabs);
                            if prune_empty_groups(&mut groups, &tabs) {
                                save_groups(&groups);
                                push_groups(&chrome, &groups);
                            }
                            relayout(&window, &chrome, &ai_sidebar, &tabs);
                            activate(&window, &mut tabs, active);
                            push_tabs(&chrome, &tabs, active);
                            push_url_star(&chrome, &tabs, active, &bookmarks);
                            push_can_go(&chrome, &tabs, active);
                            persist_session(&tabs, active);
                        }
                    }
                }
                UserEvent::AtcAdd { pattern, ws } => {
                    let pattern = pattern.trim().trim_start_matches("www.").to_lowercase();
                    if !pattern.is_empty() && workspaces.iter().any(|w| w.id == ws) {
                        settings.atc_rules.retain(|r| r.pattern != pattern);
                        settings.atc_rules.push(store::AtcRule { pattern, workspace: ws });
                        store::save_settings(&settings);
                    }
                }
                UserEvent::ArchiveOpen(url) => {
                    archive.retain(|a| a.url != url);
                    store::save_archive(&archive);
                    push_archive(&chrome, &archive);
                    let _ = spawn_proxy.send_event(UserEvent::NewTabUrl(url, None));
                }
                UserEvent::ArchiveDelete(url) => {
                    archive.retain(|a| a.url != url);
                    store::save_archive(&archive);
                    push_archive(&chrome, &archive);
                }
                UserEvent::ArchiveClear => {
                    archive.clear();
                    store::save_archive(&archive);
                    push_archive(&chrome, &archive);
                }
                UserEvent::HistoryClear => {
                    history.clear();
                    store::save_history(&history);
                    push_history(&chrome, &history);
                    push_history_source(&chrome, &history);
                }

                UserEvent::CloseTab(id) => {
                    if SPLIT_TAB.load(Ordering::Relaxed) == id {
                        SPLIT_TAB.store(0, Ordering::Relaxed);
                    }
                    if let Some(pos) = tabs.iter().position(|t| t.id == id) {
                        remember_closed(&mut closed_stack, &tabs[pos].url);
                        let closed_ws = tabs[pos].workspace;
                        tabs.remove(pos);
                        // If the closed tab was on screen, land on the nearest remaining tab in
                        // the same workspace, or a fresh home tab (a workspace is never shown
                        // empty; this also covers closing the last tab overall).
                        if active == id {
                            let nearest = tabs
                                .iter()
                                .enumerate()
                                .filter(|(_, t)| t.workspace == closed_ws)
                                .min_by_key(|(i, _)| (*i as i64 - pos as i64).abs())
                                .map(|(_, t)| t.id);
                            match nearest {
                                Some(nid) => active = nid,
                                None => {
                                    let mut t = make_tab(
                                        &window,
                                        next_id,
                                        &home_url,
                                        spawn_proxy.clone(),
                                        &engine,
                                        &mut web_context,
                                        false,
                                    );
                                    t.workspace = closed_ws;
                                    active = t.id;
                                    tabs.push(t);
                                    next_id += 1;
                                }
                            }
                        }
                        if let Some(t) = tabs.iter_mut().find(|t| t.id == active) {
                            ensure_live(t, &window, spawn_proxy.clone(), &engine, &mut web_context);
                        }
                        if prune_empty_groups(&mut groups, &tabs) {
                            save_groups(&groups);
                            push_groups(&chrome, &groups);
                        }
                        relayout(&window, &chrome, &ai_sidebar, &tabs);
                        activate(&window, &mut tabs, active);
                        push_tabs(&chrome, &tabs, active);
                        push_url_star(&chrome, &tabs, active, &bookmarks);
                        push_can_go(&chrome, &tabs, active);
                        persist_session(&tabs, active);
                    }
                }

                UserEvent::SwitchTab(id) => {
                    if tabs.iter().any(|t| t.id == id) {
                        // Switching to a tab in another workspace (palette results span all of
                        // them) follows the tab there.
                        let ws = tabs.iter().find(|t| t.id == id).map(|t| t.workspace).unwrap_or(0);
                        if ws != active_ws {
                            ws_last_active.insert(active_ws, active);
                            active_ws = ws;
                            SPLIT_TAB.store(0, Ordering::Relaxed);
                            persist_workspaces(&workspaces, active_ws);
                            push_workspaces(&chrome, &workspaces, active_ws);
                        }
                        active = id;
                        if let Some(t) = tabs.iter_mut().find(|t| t.id == id) {
                            ensure_live(t, &window, spawn_proxy.clone(), &engine, &mut web_context);
                        }
                        relayout(&window, &chrome, &ai_sidebar, &tabs);
                        activate(&window, &mut tabs, active);
                        push_tabs(&chrome, &tabs, active);
                        push_url_star(&chrome, &tabs, active, &bookmarks);
                        push_can_go(&chrome, &tabs, active);
                        push_zoom(&chrome, &tabs, active);
                        persist_session(&tabs, active);
                    }
                }
                UserEvent::ReorderTabs(order) => {
                    // Rebuild the tab vec to match the strip's new order; ids not in `order`
                    // (shouldn't happen) keep their relative position at the end.
                    let mut reordered: Vec<Tab> = Vec::with_capacity(tabs.len());
                    for id in &order {
                        if let Some(pos) = tabs.iter().position(|t| t.id == *id) {
                            reordered.push(tabs.remove(pos));
                        }
                    }
                    reordered.append(&mut tabs);
                    tabs = reordered;
                    normalize_groups(&mut tabs);
                    push_tabs(&chrome, &tabs, active);
                    persist_session(&tabs, active);
                }
                UserEvent::AddTabToNewGroup(tab_id) => {
                    if tabs.iter().any(|t| t.id == tab_id) {
                        let id = next_group_id;
                        next_group_id += 1;
                        let color = GROUP_COLORS[groups.len() % GROUP_COLORS.len()].to_string();
                        groups.push(TabGroup { id, name: String::new(), color, collapsed: false });
                        if let Some(t) = tabs.iter_mut().find(|t| t.id == tab_id) {
                            t.group = Some(id);
                        }
                        normalize_groups(&mut tabs);
                        save_groups(&groups);
                        push_groups(&chrome, &groups);
                        push_tabs(&chrome, &tabs, active);
                        persist_session(&tabs, active);
                    }
                }
                UserEvent::AddTabToGroup { tab, group } => {
                    if groups.iter().any(|g| g.id == group) {
                        if let Some(t) = tabs.iter_mut().find(|t| t.id == tab) {
                            t.group = Some(group);
                        }
                        normalize_groups(&mut tabs);
                        push_tabs(&chrome, &tabs, active);
                        persist_session(&tabs, active);
                    }
                }
                UserEvent::RemoveTabFromGroup(tab) => {
                    if let Some(t) = tabs.iter_mut().find(|t| t.id == tab) {
                        t.group = None;
                    }
                    let live: HashSet<u32> = tabs.iter().filter_map(|t| t.group).collect();
                    groups.retain(|g| live.contains(&g.id));
                    normalize_groups(&mut tabs);
                    save_groups(&groups);
                    push_groups(&chrome, &groups);
                    push_tabs(&chrome, &tabs, active);
                    persist_session(&tabs, active);
                }
                UserEvent::ToggleGroupCollapse(group) => {
                    let mut now_collapsed = false;
                    if let Some(g) = groups.iter_mut().find(|g| g.id == group) {
                        g.collapsed = !g.collapsed;
                        now_collapsed = g.collapsed;
                    }
                    // If we just hid the active tab, move to the nearest tab outside the group.
                    if now_collapsed && tabs.iter().any(|t| t.id == active && t.group == Some(group)) {
                        if let Some(t) = tabs
                            .iter()
                            .find(|t| t.group != Some(group) && t.workspace == active_ws)
                        {
                            active = t.id;
                            if let Some(t) = tabs.iter_mut().find(|t| t.id == active) {
                                ensure_live(t, &window, spawn_proxy.clone(), &engine, &mut web_context);
                            }
                            activate(&window, &mut tabs, active);
                            push_url_star(&chrome, &tabs, active, &bookmarks);
                            push_can_go(&chrome, &tabs, active);
                        }
                    }
                    save_groups(&groups);
                    push_groups(&chrome, &groups);
                    push_tabs(&chrome, &tabs, active);
                }
                UserEvent::RenameGroup { group, name } => {
                    if let Some(g) = groups.iter_mut().find(|g| g.id == group) {
                        g.name = name;
                    }
                    save_groups(&groups);
                    push_groups(&chrome, &groups);
                }
                UserEvent::SetGroupColor { group, color } => {
                    if let Some(g) = groups.iter_mut().find(|g| g.id == group) {
                        g.color = color;
                    }
                    save_groups(&groups);
                    push_groups(&chrome, &groups);
                }
                UserEvent::Ungroup(group) => {
                    for t in tabs.iter_mut().filter(|t| t.group == Some(group)) {
                        t.group = None;
                    }
                    groups.retain(|g| g.id != group);
                    normalize_groups(&mut tabs);
                    save_groups(&groups);
                    push_groups(&chrome, &groups);
                    push_tabs(&chrome, &tabs, active);
                    persist_session(&tabs, active);
                }
                UserEvent::CloseGroup(group) => {
                    let ids: Vec<u32> =
                        tabs.iter().filter(|t| t.group == Some(group)).map(|t| t.id).collect();
                    for id in &ids {
                        if let Some(pos) = tabs.iter().position(|t| t.id == *id) {
                            remember_closed(&mut closed_stack, &tabs[pos].url);
                            tabs.remove(pos);
                        }
                    }
                    groups.retain(|g| g.id != group);
                    if tabs.is_empty() {
                        *control_flow = ControlFlow::Exit;
                        return;
                    }
                    if !tabs.iter().any(|t| t.id == active) {
                        active = tabs[0].id;
                        if let Some(t) = tabs.iter_mut().find(|t| t.id == active) {
                            ensure_live(t, &window, spawn_proxy.clone(), &engine, &mut web_context);
                        }
                    }
                    normalize_groups(&mut tabs);
                    relayout(&window, &chrome, &ai_sidebar, &tabs);
                    activate(&window, &mut tabs, active);
                    save_groups(&groups);
                    push_groups(&chrome, &groups);
                    push_tabs(&chrome, &tabs, active);
                    push_url_star(&chrome, &tabs, active, &bookmarks);
                    push_can_go(&chrome, &tabs, active);
                    persist_session(&tabs, active);
                }

                UserEvent::PageUrlChanged(id, url) => {
                    let mut is_private = false;
                    // Where this navigation came from: a pending opener edge (link opened into a
                    // new tab) wins; otherwise the tab's own previous page.
                    let mut came_from: Option<String> = None;
                    if let Some(t) = tabs.iter_mut().find(|t| t.id == id) {
                        is_private = t.private;
                        came_from = t.trail_from.take().or_else(|| {
                            store::trailable(&t.url).then(|| t.url.clone())
                        });
                        t.url = url.clone();
                        t.title = tab_label(&url);
                        t.favicon.clear();
                        t.audio = false;
                        t.muted = false;
                        *t.page_url.borrow_mut() = url.clone();
                    }
                    // Private tabs leave no record: not in history, not on the trail.
                    if !is_private {
                        store::record_visit(&mut history, &url, &tab_label(&url));
                        store::save_history(&history);
                        push_history_source(&chrome, &history);
                        if settings.trail.enabled
                            && !trail_excluded(&url, &settings.trail.exclude)
                        {
                            let came_from = came_from
                                .filter(|f| !trail_excluded(f, &settings.trail.exclude));
                            store::record_trail(
                                &mut trail,
                                &url,
                                &tab_label(&url),
                                came_from,
                                settings.trail.retention_days,
                            );
                            store::save_trail(&trail);
                        }
                    }
                    if id == active {
                        push_url(&chrome, &url); // push_url maps home -> empty, search -> query
                        push_star(&chrome, &tabs, active, &bookmarks);
                        let _ = chrome
                            .evaluate_script("window.__chrome&&window.__chrome.setLoading(true)");
                    }
                    push_tabs(&chrome, &tabs, active);
                    persist_session(&tabs, active);
                }

                UserEvent::ChromeReady => {
                    push_profiles(&chrome, &profiles, &current_profile);
                    // Optional launch picker: only when the user turned it on, more than one
                    // profile exists, and this launch wasn't already told what to open.
                    if profiles.ask_at_startup
                        && !profiles.list.is_empty()
                        && !asked_for_profile
                        && cli_url.is_none()
                    {
                        let _ = chrome.evaluate_script(
                            "window.__chrome&&window.__chrome.showProfilePicker()",
                        );
                    }
                    push_workspaces(&chrome, &workspaces, active_ws);
                    push_groups(&chrome, &groups);
                    push_tabs(&chrome, &tabs, active);
                    push_url_star(&chrome, &tabs, active, &bookmarks);
                    push_bookmarks(&chrome, &bookmarks);
                    push_star(&chrome, &tabs, active, &bookmarks);
                    push_reading(&chrome, &reading);
                    push_bookmark_folders(&chrome, &settings.bookmark_folders);
                    push_history_source(&chrome, &history);
                    push_restore_prompt(&chrome);
                    push_archive(&chrome, &archive);
                    push_chrome_ai_cfg(&chrome, &settings);
                    push_tab_layout(&chrome, settings.vertical_tabs);
                    let _ = chrome.evaluate_script(&chrome_theme_js(&settings.theme));
                }

                UserEvent::ReclaimIdleTabs => {
                    let split = SPLIT_TAB.load(Ordering::Relaxed);
                    let mut changed = false;
                    for t in tabs.iter_mut() {
                        // Never reclaim: the active tab (or the visible split pane), pinned ("keep
                        // loaded") tabs, tabs making sound (background video/music), or tabs with an
                        // in-progress download. Their idle stamp stays fresh so that the moment they
                        // stop being exempt, the clock starts from now, not from their last click.
                        if t.id == active
                            || t.id == split
                            || t.pinned
                            || t.audio
                            || t.downloading > 0
                            || t.private
                        {
                            t.last_active = std::time::Instant::now();
                            t.last_used = store::unix_now();
                            continue;
                        }
                        if t.discarded {
                            continue;
                        }
                        let idle_for = t.last_active.elapsed();
                        if t.suspended {
                            // Long-abandoned -> discard: drop the WebView so its renderer process
                            // exits and the RAM is actually freed. Waking from this tier is a full
                            // page reload, so it's reserved for genuinely stale tabs.
                            if idle_for >= discard_after {
                                t.webview = None;
                                t.discarded = true;
                                t.suspended = false;
                                changed = true;
                            }
                        } else if idle_for >= suspend_after {
                            if let Some(wv) = &t.webview {
                                if suspend_webview(wv) {
                                    t.suspended = true;
                                    changed = true;
                                }
                            }
                        }
                    }
                    // Reflect new sleeping/discarded state in the strip + memory HUD.
                    if changed {
                        push_tabs(&chrome, &tabs, active);
                    }
                    // ARCHIVE tier: tabs untouched for archive_days quietly leave the strip into
                    // archive.json (History viewer > Archived). Pinned/private/audible/downloading
                    // tabs were exempted above (their last_used stays fresh); the active tab and
                    // split pane are skipped here too, and the strip is never emptied.
                    if settings.archive_enabled {
                        let cutoff = store::unix_now()
                            .saturating_sub(settings.archive_days.max(1) * 86_400);
                        let mut archived_any = false;
                        loop {
                            if tabs.len() <= 1 {
                                break;
                            }
                            let Some(pos) = tabs.iter().position(|t| {
                                t.id != active
                                    && t.id != split
                                    && !t.pinned
                                    && !t.private
                                    && !t.audio
                                    && t.downloading == 0
                                    && t.last_used > 0
                                    && t.last_used < cutoff
                            }) else {
                                break;
                            };
                            let t = tabs.remove(pos);
                            if store::trailable(&t.url) {
                                store::record_archive(&mut archive, &t.url, &t.title);
                            }
                            archived_any = true;
                        }
                        if archived_any {
                            store::save_archive(&archive);
                            if prune_empty_groups(&mut groups, &tabs) {
                                save_groups(&groups);
                                push_groups(&chrome, &groups);
                            }
                            relayout(&window, &chrome, &ai_sidebar, &tabs);
                            push_tabs(&chrome, &tabs, active);
                            push_archive(&chrome, &archive);
                            persist_session(&tabs, active);
                        }
                    }
                }

                UserEvent::BwFill => {
                    enter_bw_modal(&window, &chrome, &ai_sidebar, &tabs, active);
                }
                UserEvent::OpenExtension => {
                    if load_extensions {
                        open_extension_popup(&chrome, spawn_proxy.clone());
                    } else {
                        let _ = spawn_proxy.send_event(UserEvent::NewTabUrl(EXT_HELP_URL.to_string(), None));
                    }
                }
                UserEvent::BwSubmit(mut password) => {
                    // If a passkey assertion is waiting on the unlock, this master password is for
                    // that, not a username/password fill.
                    if let Some(pw) = &pending_webauthn {
                        let rp_id = pw.rp_id.clone();
                        let _ = chrome.evaluate_script(
                            "window.__chrome&&window.__chrome.bwMessage('Unlocking…')",
                        );
                        let bw_proxy = spawn_proxy.clone();
                        std::thread::spawn(move || {
                            let outcome = bitwarden::fetch_passkey(&mut password, &rp_id);
                            let _ = bw_proxy.send_event(UserEvent::WebauthnPasskey(outcome));
                        });
                        return;
                    }
                    let url = active_url(&tabs, active);
                    // R6: only inject a vault credential into an https:// origin (never http/file/about).
                    if !url.starts_with("https://") {
                        password.zeroize();
                        let _ = chrome.evaluate_script(
                            "window.__chrome&&window.__chrome.bwMessage('Autofill is only allowed on https:// pages.')",
                        );
                        return;
                    }
                    let _ = chrome
                        .evaluate_script("window.__chrome&&window.__chrome.bwMessage('Unlocking…')");
                    let bw_proxy = spawn_proxy.clone();
                    std::thread::spawn(move || {
                        let outcome = bitwarden::fetch(&mut password, &url);
                        let _ = bw_proxy.send_event(UserEvent::BwResult(outcome));
                    });
                }
                UserEvent::BwCancel => {
                    if let Some(pw) = pending_webauthn.take() {
                        exit_bw_modal(&window, &chrome, &ai_sidebar, &mut tabs, active);
                        // User dismissed the unlock -> tell the page the passkey was declined.
                        webauthn_reject(&tabs, pw.tab_id, &pw.req_id, "Passkey unlock cancelled");
                        return;
                    }
                    exit_bw_modal(&window, &chrome, &ai_sidebar, &mut tabs, active);
                }
                UserEvent::BwResult(outcome) => match outcome {
                    bitwarden::FillOutcome::Found { username, password } => {
                        inject_credentials(&tabs, active, &username, &password);
                        exit_bw_modal(&window, &chrome, &ai_sidebar, &mut tabs, active);
                    }
                    bitwarden::FillOutcome::NoMatch => {
                        let _ = chrome.evaluate_script(
                            "window.__chrome&&window.__chrome.bwMessage('No matching login for this site.')",
                        );
                    }
                    bitwarden::FillOutcome::NotLoggedIn => {
                        let _ = chrome.evaluate_script(
                            "window.__chrome&&window.__chrome.bwMessage('Not logged in. One-time setup: run bw login in a terminal.')",
                        );
                    }
                    bitwarden::FillOutcome::Error(e) => {
                        if let Ok(js) = serde_json::to_string(&format!("Error: {e}")) {
                            let _ = chrome.evaluate_script(&format!(
                                "window.__chrome&&window.__chrome.bwMessage({js})"
                            ));
                        }
                    }
                },

                UserEvent::WebauthnGet {
                    tab_id,
                    req_id,
                    rp_id,
                    challenge,
                } => {
                    // Real origin from the tab's live top-level URL - NEVER what the page claimed.
                    let url = tabs
                        .iter()
                        .find(|t| t.id == tab_id)
                        .map(|t| t.page_url.borrow().clone())
                        .unwrap_or_default();
                    let host = host_only(&url);
                    let rp = if rp_id.is_empty() { host.clone() } else { rp_id };
                    if !url.starts_with("https://")
                        || host.is_empty()
                        || !webauthn::rp_id_allowed(&rp, &host)
                    {
                        // Insecure or mismatched rpId -> never sign; let the page's native call run.
                        webauthn_fallback(&tabs, tab_id, &req_id);
                        return;
                    }
                    pending_webauthn = Some(PendingWebauthn {
                        tab_id,
                        req_id,
                        rp_id: rp,
                        challenge,
                        origin: origin_of(&url),
                    });
                    // Reuse the unlock modal; BwSubmit routes to the passkey path while one is pending.
                    enter_bw_modal(&window, &chrome, &ai_sidebar, &tabs, active);
                }

                UserEvent::WebauthnPasskey(outcome) => {
                    let Some(pw) = pending_webauthn.take() else {
                        return;
                    };
                    exit_bw_modal(&window, &chrome, &ai_sidebar, &mut tabs, active);
                    match outcome {
                        bitwarden::PasskeyOutcome::Found(pk) => {
                            match webauthn::assert(&pk, &pw.challenge, &pw.origin) {
                                Ok(a) => deliver_assertion(&tabs, pw.tab_id, &pw.req_id, &a),
                                Err(e) => {
                                    eprintln!("[webauthn] assert failed: {e}");
                                    webauthn_reject(&tabs, pw.tab_id, &pw.req_id, &e);
                                }
                            }
                        }
                        bitwarden::PasskeyOutcome::NoMatch => {
                            // No vault passkey for this site -> let the page's native call proceed.
                            webauthn_fallback(&tabs, pw.tab_id, &pw.req_id);
                        }
                        bitwarden::PasskeyOutcome::NotLoggedIn => {
                            webauthn_reject(
                                &tabs,
                                pw.tab_id,
                                &pw.req_id,
                                "Bitwarden CLI not logged in (run: bw login)",
                            );
                        }
                        bitwarden::PasskeyOutcome::Error(e) => {
                            webauthn_reject(&tabs, pw.tab_id, &pw.req_id, &e);
                        }
                    }
                }

                UserEvent::BookmarkAdd => {
                    let entry = tabs
                        .iter()
                        .find(|t| t.id == active)
                        .map(|t| (t.title.clone(), t.url.clone()));
                    if let Some((title, url)) = entry {
                        if !url.is_empty() && !bookmarks.iter().any(|b| b.url == url) {
                            bookmarks.push(store::Bookmark { title, url, folder: None });
                            store::save_bookmarks(&bookmarks);
                            push_bookmarks(&chrome, &bookmarks);
                    push_star(&chrome, &tabs, active, &bookmarks);
                        }
                    }
                }
                UserEvent::BookmarkToggle => {
                    if let Some((title, url)) = tabs
                        .iter()
                        .find(|t| t.id == active)
                        .map(|t| (t.title.clone(), t.url.clone()))
                    {
                        if !url.is_empty() && !url.contains("home.html") {
                            let existed = bookmarks.iter().any(|b| b.url == url);
                            if existed {
                                bookmarks.retain(|b| b.url != url);
                            } else {
                                bookmarks.push(store::Bookmark {
                                    title,
                                    url: url.clone(),
                                    folder: None,
                                });
                            }
                            store::save_bookmarks(&bookmarks);
                            push_bookmarks(&chrome, &bookmarks);
                            push_star(&chrome, &tabs, active, &bookmarks);
                            if !existed {
                                // Just added -> offer a folder choice next to the star.
                                let uj = serde_json::to_string(&url).unwrap_or_else(|_| "\"\"".into());
                                let _ = chrome.evaluate_script(&format!(
                                    "window.__chrome&&window.__chrome.showBookmarkAdded({uj})"
                                ));
                            }
                        }
                    }
                }
                UserEvent::BookmarkOpen(url) => {
                    if let Some(t) = tabs.iter_mut().find(|t| t.id == active) {
                        t.url = url.clone();
                        if let Some(wv) = &t.webview {
                            let _ = wv.load_url(&url);
                        }
                    }
                    persist_session(&tabs, active);
                }
                UserEvent::BookmarkRemove(url) => {
                    bookmarks.retain(|b| b.url != url);
                    store::save_bookmarks(&bookmarks);
                    push_bookmarks(&chrome, &bookmarks);
                    push_star(&chrome, &tabs, active, &bookmarks);
                }
                UserEvent::SaveBookmarks(json) => {
                    if let Ok(parsed) = serde_json::from_str::<Vec<store::Bookmark>>(&json) {
                        bookmarks = parsed;
                        store::save_bookmarks(&bookmarks);
                        push_bookmarks(&chrome, &bookmarks);
                    push_star(&chrome, &tabs, active, &bookmarks);
                    }
                }
                UserEvent::BookmarkNewFolder(name) => {
                    let name = name.trim().to_string();
                    if !name.is_empty() && !settings.bookmark_folders.iter().any(|f| f == &name) {
                        settings.bookmark_folders.push(name);
                        store::save_settings(&settings);
                        push_bookmark_folders(&chrome, &settings.bookmark_folders);
                    }
                }
                UserEvent::BookmarkSetFolder { url, folder } => {
                    if let Some(b) = bookmarks.iter_mut().find(|b| b.url == url) {
                        b.folder = folder.clone();
                    }
                    if let Some(name) = folder.as_ref().filter(|n| !n.is_empty()) {
                        if !settings.bookmark_folders.iter().any(|f| f == name) {
                            settings.bookmark_folders.push(name.clone());
                            store::save_settings(&settings);
                        }
                    }
                    store::save_bookmarks(&bookmarks);
                    push_bookmarks(&chrome, &bookmarks);
                    push_bookmark_folders(&chrome, &settings.bookmark_folders);
                    push_star(&chrome, &tabs, active, &bookmarks);
                }
                UserEvent::BookmarkReorder { url, target, before } => {
                    // Move the dragged bookmark to just before/after the target in the Vec (which is
                    // the bar's render order). Recompute the target index AFTER removing the dragged
                    // one so the insert position stays correct.
                    if url != target {
                        if let Some(from) = bookmarks.iter().position(|b| b.url == url) {
                            let item = bookmarks.remove(from);
                            let mut to = bookmarks
                                .iter()
                                .position(|b| b.url == target)
                                .unwrap_or(bookmarks.len());
                            if !before {
                                to += 1;
                            }
                            to = to.min(bookmarks.len());
                            bookmarks.insert(to, item);
                            store::save_bookmarks(&bookmarks);
                            push_bookmarks(&chrome, &bookmarks);
                            push_star(&chrome, &tabs, active, &bookmarks);
                        }
                    }
                }
                UserEvent::BookmarkDeleteFolder(name) => {
                    settings.bookmark_folders.retain(|f| f != &name);
                    store::save_settings(&settings);
                    for b in bookmarks.iter_mut() {
                        if b.folder.as_deref() == Some(name.as_str()) {
                            b.folder = None;
                        }
                    }
                    store::save_bookmarks(&bookmarks);
                    push_bookmarks(&chrome, &bookmarks);
                    push_bookmark_folders(&chrome, &settings.bookmark_folders);
                }
                UserEvent::ReadingAdd => {
                    let entry = tabs
                        .iter()
                        .find(|t| t.id == active)
                        .map(|t| (t.title.clone(), t.url.clone()));
                    if let Some((title, url)) = entry {
                        if !url.is_empty()
                            && !url.contains("home.html")
                            && !reading.iter().any(|r| r.url == url)
                        {
                            reading.insert(0, store::ReadingItem { title, url });
                            store::save_reading(&reading);
                            push_reading(&chrome, &reading);
                        }
                    }
                }
                UserEvent::ReadingOpen(url) => {
                    if let Some(t) = tabs.iter_mut().find(|t| t.id == active) {
                        t.url = url.clone();
                        if let Some(wv) = &t.webview {
                            let _ = wv.load_url(&url);
                        }
                    }
                    persist_session(&tabs, active);
                }
                UserEvent::ReadingRemove(url) => {
                    reading.retain(|r| r.url != url);
                    store::save_reading(&reading);
                    push_reading(&chrome, &reading);
                }

                UserEvent::CloseActiveTab => {
                    let id = active;
                    // Closing the focused pane exits split view.
                    if SPLIT_TAB.load(Ordering::Relaxed) != 0 {
                        SPLIT_TAB.store(0, Ordering::Relaxed);
                    }
                    if let Some(pos) = tabs.iter().position(|t| t.id == id) {
                        remember_closed(&mut closed_stack, &tabs[pos].url);
                        tabs.remove(pos);
                        if tabs.is_empty() {
                            let t = make_tab(
                                &window,
                                next_id,
                                &home_url,
                                spawn_proxy.clone(),
                                &engine,
                                &mut web_context,
                                false,
                            );
                            active = t.id;
                            tabs.push(t);
                            next_id += 1;
                        }
                        if tabs.iter().any(|t| t.id != active) {
                            let new_pos = pos.min(tabs.len() - 1);
                            active = tabs[new_pos].id;
                        }
                        if let Some(t) = tabs.iter_mut().find(|t| t.id == active) {
                            ensure_live(t, &window, spawn_proxy.clone(), &engine, &mut web_context);
                        }
                        if prune_empty_groups(&mut groups, &tabs) {
                            save_groups(&groups);
                            push_groups(&chrome, &groups);
                        }
                        relayout(&window, &chrome, &ai_sidebar, &tabs);
                        activate(&window, &mut tabs, active);
                        push_tabs(&chrome, &tabs, active);
                        push_url_star(&chrome, &tabs, active, &bookmarks);
                        push_can_go(&chrome, &tabs, active);
                        persist_session(&tabs, active);
                    }
                }
                UserEvent::FocusOmnibox => {
                    let _ = chrome.focus();
                    let _ = chrome.evaluate_script("window.__chrome&&window.__chrome.focusOmnibox()");
                }
                UserEvent::CycleTab(forward) => {
                    // Cycle within the visible workspace only.
                    let ids: Vec<u32> = tabs
                        .iter()
                        .filter(|t| t.workspace == active_ws)
                        .map(|t| t.id)
                        .collect();
                    let len = ids.len();
                    if len > 1 {
                        let cur = ids.iter().position(|id| *id == active).unwrap_or(0);
                        let next = if forward {
                            (cur + 1) % len
                        } else {
                            (cur + len - 1) % len
                        };
                        active = ids[next];
                        if let Some(t) = tabs.iter_mut().find(|t| t.id == active) {
                            ensure_live(t, &window, spawn_proxy.clone(), &engine, &mut web_context);
                        }
                        relayout(&window, &chrome, &ai_sidebar, &tabs);
                        activate(&window, &mut tabs, active);
                        push_tabs(&chrome, &tabs, active);
                        push_url_star(&chrome, &tabs, active, &bookmarks);
                        push_can_go(&chrome, &tabs, active);
                        persist_session(&tabs, active);
                    }
                }

                UserEvent::TogglePin(id) => {
                    if let Some(t) = tabs.iter_mut().find(|t| t.id == id) {
                        t.pinned = !t.pinned;
                    }
                    push_tabs(&chrome, &tabs, active);
                    persist_session(&tabs, active);
                }

                UserEvent::OmniboxTyping(text) => {
                    if let Some(c) = best_completion(&history, &bookmarks, &text) {
                        if let (Ok(tj), Ok(cj)) =
                            (serde_json::to_string(&text), serde_json::to_string(&c))
                        {
                            let _ = chrome.evaluate_script(&format!(
                                "window.__chrome&&window.__chrome.setOmniboxComplete({tj},{cj})"
                            ));
                        }
                    }
                }

                UserEvent::HomeReady(id) => {
                    inject_home_data(&tabs, id, &history, &links, &settings, &trail);
                    if let Some(wv) = tabs.iter().find(|t| t.id == id).and_then(|t| t.webview.as_ref())
                    {
                        let _ = wv.evaluate_script(&home_theme_js(&settings.theme));
                    }
                    // Typing on a fresh new-tab page should land in the top omnibox, not the page's
                    // central search box (which is kept for clicks but no longer autofocused).
                    if id == active {
                        let _ = chrome.focus();
                        let _ =
                            chrome.evaluate_script("window.__chrome&&window.__chrome.focusOmnibox()");
                        // WebView2 can pull focus back into the page when the navigation fully
                        // commits, which lands after this handler runs - losing the race silently.
                        // Re-assert past that window, twice (a fresh webview's first commit can be
                        // slow). focusOmnibox no-ops when the bar already has focus, so a late
                        // re-assert never clobbers in-progress typing.
                        let p = spawn_proxy.clone();
                        std::thread::spawn(move || {
                            for delay in [150u64, 450] {
                                std::thread::sleep(std::time::Duration::from_millis(delay));
                                if p.send_event(UserEvent::FocusOmnibox).is_err() {
                                    break;
                                }
                            }
                        });
                    }
                }

                UserEvent::PageLoaded(id) => {
                    // Restore this site's remembered zoom (if any).
                    let host = host_of(&tabs.iter().find(|t| t.id == id).map(|t| t.url.clone()).unwrap_or_default());
                    if let Some(&z) = zoom_levels.get(&host) {
                        apply_zoom(&tabs, id, z);
                    }
                    // Apply the selection-pill on/off setting to the freshly loaded page.
                    if let Some(wv) = tabs.iter().find(|t| t.id == id).and_then(|t| t.webview.as_ref())
                    {
                        let _ = wv
                            .evaluate_script(&format!("window.__apPill={}", settings.ai.selection_pill));
                    }
                    if id == active {
                        push_can_go(&chrome, &tabs, active);
                        push_zoom(&chrome, &tabs, active);
                        let _ = chrome
                            .evaluate_script("window.__chrome&&window.__chrome.setLoading(false)");
                        // Hand keyboard focus to the page so shortcuts like YouTube's space (play/
                        // pause) and arrow keys (seek) work immediately, without a click into the
                        // page. Omnibox-initiated and toolbar back/forward navigations otherwise
                        // leave focus on the chrome webview. The new-tab page is the exception - it
                        // keeps omnibox focus (set in HomeReady).
                        if let Some(t) = tabs.iter().find(|t| t.id == id) {
                            if !t.url.contains("home.html") {
                                if let Some(wv) = &t.webview {
                                    let _ = wv.focus();
                                }
                            }
                        }
                    }
                }

                UserEvent::OpenSettings => {
                    let links_json = serde_json::to_string(&links).unwrap_or_else(|_| "[]".into());
                    let prefs_json = serde_json::to_string(&settings).unwrap_or_else(|_| "{}".into());
                    // The shortcut editor only makes sense on the new-tab page (that's where the
                    // shortcuts render), so only offer it when the active tab is the home page.
                    let is_home = tabs
                        .iter()
                        .find(|t| t.id == active)
                        .map(|t| t.url.contains("home.html"))
                        .unwrap_or(false);
                    enter_settings_modal(
                        &window, &chrome, &ai_sidebar, &tabs, active, &links_json, &prefs_json,
                        is_home,
                    );
                    // Fetch the installed Ollama models (off-thread) to populate the model dropdown.
                    if settings.ai.enabled {
                        let host = settings.ai.host.clone();
                        let p = spawn_proxy.clone();
                        std::thread::spawn(move || {
                            let _ = p.send_event(UserEvent::AiModels(ai::list_models(&host)));
                        });
                    }
                }
                UserEvent::SaveLinks(json) => {
                    if let Ok(parsed) = serde_json::from_str::<Vec<store::QuickLink>>(&json) {
                        links = parsed;
                        store::save_links(&links);
                    }
                    exit_settings_modal(&window, &chrome, &ai_sidebar, &mut tabs, active);
                    inject_home_data(&tabs, active, &history, &links, &settings, &trail);
                }
                UserEvent::SaveSettings(json) => {
                    if let Ok(parsed) = serde_json::from_str::<store::Settings>(&json) {
                        // The settings form doesn't include bookmark_folders (folders are managed from
                        // the bar, not the form), so carry the existing list across or `settings = parsed`
                        // would wipe it - leaving folder chips on the bar but an empty add-bookmark picker.
                        let folders = std::mem::take(&mut settings.bookmark_folders);
                        settings = parsed;
                        settings.bookmark_folders = folders;
                        store::save_settings(&settings);
                        rebuild_shortcut_map(&settings.shortcuts);
                    }
                    // Tab orientation feeds the layout helpers; update it before the modal exits so the
                    // chrome shrinks back to the correct strip/column and content reflows in one pass.
                    if settings.vertical_tabs != VERTICAL.load(Ordering::Relaxed) {
                        VERTICAL.store(settings.vertical_tabs, Ordering::Relaxed);
                        push_tab_layout(&chrome, settings.vertical_tabs);
                    }
                    // Off the home page, Save only sends save_settings (no save_links), so close the
                    // modal here too. On the home page save_links also fires; exit is idempotent.
                    exit_settings_modal(&window, &chrome, &ai_sidebar, &mut tabs, active);
                    inject_home_data(&tabs, active, &history, &links, &settings, &trail);
                    // Re-apply AI feature flags everywhere the settings affect.
                    push_chrome_ai_cfg(&chrome, &settings);
                    push_sidebar_ai_cfg(&ai_sidebar, &settings);
                    push_pill_flag(&tabs, settings.ai.selection_pill);
                    // Re-apply the color theme to the chrome, sidebar, and any open new-tab pages.
                    let _ = chrome.evaluate_script(&chrome_theme_js(&settings.theme));
                    let _ = ai_sidebar.evaluate_script(&sidebar_theme_js(&settings.theme));
                    let home_js = home_theme_js(&settings.theme);
                    for t in &tabs {
                        if t.url.contains("home.html") {
                            if let Some(wv) = &t.webview {
                                let _ = wv.evaluate_script(&home_js);
                            }
                        }
                    }
                    // If the assistant was just disabled while its panel was open, close the panel.
                    if !settings.ai.enabled && AI_PANEL_W.load(Ordering::Relaxed) > 0 {
                        AI_PANEL_W.store(0, Ordering::Relaxed);
                        let _ = ai_sidebar.set_visible(false);
                        relayout(&window, &chrome, &ai_sidebar, &tabs);
                        activate(&window, &mut tabs, active);
                    }
                }
                UserEvent::CloseSettings => {
                    exit_settings_modal(&window, &chrome, &ai_sidebar, &mut tabs, active);
                }
                UserEvent::ClearCookies => {
                    clear_all_cookies(&chrome);
                }
                UserEvent::ExportData => {
                    let bundle = serde_json::json!({
                        "settings": &settings,
                        "bookmarks": &bookmarks,
                        "links": &links,
                    });
                    if let Ok(json) = serde_json::to_string_pretty(&bundle) {
                        let ts = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        let base = std::env::var("USERPROFILE").unwrap_or_default();
                        let path = format!("{base}\\Downloads\\aperture-backup-{ts}.json");
                        if std::fs::write(&path, json).is_ok() {
                            let _ = std::process::Command::new("explorer")
                                .arg(format!("/select,{path}"))
                                .spawn();
                        }
                    }
                }
                UserEvent::ImportData(data) => {
                    #[derive(serde::Deserialize)]
                    struct Bundle {
                        settings: Option<store::Settings>,
                        bookmarks: Option<Vec<store::Bookmark>>,
                        links: Option<Vec<store::QuickLink>>,
                    }
                    if let Ok(b) = serde_json::from_str::<Bundle>(&data) {
                        // The backup file is plaintext on disk and could be tampered with. Validate
                        // attacker-controllable fields before applying them to the persistent profile: a
                        // bad AI host would silently exfiltrate every prompt; javascript:/data: bookmarks
                        // would run script when opened. Keep the prior AI host if the imported one isn't
                        // http(s), and drop any bookmark/link whose URL isn't a safe web scheme.
                        if let Some(s) = b.settings {
                            let prev_host = settings.ai.host.clone();
                            settings = s;
                            if !url_is_safe(&settings.ai.host) {
                                settings.ai.host = prev_host;
                            }
                            store::save_settings(&settings);
                            rebuild_shortcut_map(&settings.shortcuts);
                        }
                        if let Some(bm) = b.bookmarks {
                            bookmarks = bm.into_iter().filter(|b| url_is_safe(&b.url)).collect();
                            store::save_bookmarks(&bookmarks);
                        }
                        if let Some(l) = b.links {
                            links = l.into_iter().filter(|l| url_is_safe(&l.url)).collect();
                            store::save_links(&links);
                        }
                        // Re-apply everything the imported data touches.
                        push_bookmarks(&chrome, &bookmarks);
                    push_star(&chrome, &tabs, active, &bookmarks);
                        push_chrome_ai_cfg(&chrome, &settings);
                        push_sidebar_ai_cfg(&ai_sidebar, &settings);
                        let _ = chrome.evaluate_script(&chrome_theme_js(&settings.theme));
                        let _ = ai_sidebar.evaluate_script(&sidebar_theme_js(&settings.theme));
                        let home_js = home_theme_js(&settings.theme);
                        for t in &tabs {
                            if t.url.contains("home.html") {
                                if let Some(wv) = &t.webview {
                                    let _ = wv.evaluate_script(&home_js);
                                }
                            }
                        }
                        inject_home_data(&tabs, active, &history, &links, &settings, &trail);
                        if settings.vertical_tabs != VERTICAL.load(Ordering::Relaxed) {
                            VERTICAL.store(settings.vertical_tabs, Ordering::Relaxed);
                            push_tab_layout(&chrome, settings.vertical_tabs);
                        }
                    }
                    exit_settings_modal(&window, &chrome, &ai_sidebar, &mut tabs, active);
                }

                UserEvent::BwSaveCandidate {
                    url,
                    username,
                    mut password,
                } => {
                    let host = tab_label(&url);
                    // Only offer to save logins seen on https:// origins.
                    if url.starts_with("https://")
                        && pending_save.is_none()
                        && !dismissed_save_hosts.contains(&host)
                    {
                        let uname = username.clone();
                        pending_save = Some(PendingSave { url, username, password });
                        enter_save_modal(&window, &chrome, &ai_sidebar, &tabs, active, &host, &uname);
                    } else {
                        password.zeroize();
                    }
                }
                UserEvent::BwSaveSubmit(mut master) => {
                    if let Some(mut pend) = pending_save.take() {
                        dismissed_save_hosts.insert(tab_label(&pend.url));
                        let _ = chrome.evaluate_script(
                            "window.__chrome&&window.__chrome.saveMessage('Saving…')",
                        );
                        let url = std::mem::take(&mut pend.url);
                        let username = std::mem::take(&mut pend.username);
                        let mut password = std::mem::take(&mut pend.password);
                        let bw_proxy = spawn_proxy.clone();
                        std::thread::spawn(move || {
                            let outcome = bitwarden::save(&mut master, &url, &username, &mut password);
                            let _ = bw_proxy.send_event(UserEvent::BwSaveResult(outcome));
                        });
                    } else {
                        master.zeroize();
                    }
                }
                UserEvent::BwSaveCancel => {
                    if let Some(pend) = pending_save.take() {
                        dismissed_save_hosts.insert(tab_label(&pend.url));
                    }
                    exit_save_modal(&window, &chrome, &ai_sidebar, &mut tabs, active);
                }
                UserEvent::BwSaveResult(outcome) => match outcome {
                    bitwarden::SaveOutcome::Saved | bitwarden::SaveOutcome::AlreadyExists => {
                        exit_save_modal(&window, &chrome, &ai_sidebar, &mut tabs, active);
                    }
                    bitwarden::SaveOutcome::NotLoggedIn => {
                        let _ = chrome.evaluate_script(
                            "window.__chrome&&window.__chrome.saveMessage('Not logged in. Run bw login in a terminal first.')",
                        );
                    }
                    bitwarden::SaveOutcome::Error(e) => {
                        if let Ok(js) = serde_json::to_string(&format!("Error: {e}")) {
                            let _ = chrome.evaluate_script(&format!(
                                "window.__chrome&&window.__chrome.saveMessage({js})"
                            ));
                        }
                    }
                },
            },

            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                save_closed_window_session(&tabs, active);
                store::clear_session();
                *control_flow = ControlFlow::Exit;
            }

            Event::WindowEvent {
                event: WindowEvent::Resized(_),
                ..
            } => {
                relayout(&window, &chrome, &ai_sidebar, &tabs);
                activate(&window, &mut tabs, active);
            }

            _ => {}
        }
    });
}

fn idle_secs() -> u64 {
    std::env::var("BROWSER_IDLE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| store::load_settings().idle_secs.max(10))
}

/// Resting bounds of the chrome webview. Horizontal: full-width top strip, `CHROME_H` tall.
/// Vertical: full-height left column, `SIDEBAR_W` wide. (Modals temporarily grow it to full window.)
fn chrome_rect(w: f64, h: f64) -> Rect {
    if vertical() {
        Rect {
            position: LogicalPosition::new(0.0, 0.0).into(),
            size: LogicalSize::new(SIDEBAR_W, h).into(),
        }
    } else {
        Rect {
            position: LogicalPosition::new(0.0, 0.0).into(),
            size: LogicalSize::new(w, CHROME_H).into(),
        }
    }
}

/// The content area available to tabs: (x, y, width, height), to the left of the AI sidebar and
/// clear of the chrome. Horizontal mode reserves the top strip; vertical mode reserves the left
/// column. Split view divides whatever this returns.
fn content_area(w: f64, h: f64) -> (f64, f64, f64, f64) {
    let panel = AI_PANEL_W.load(Ordering::Relaxed) as f64;
    if vertical() {
        let x = SIDEBAR_W;
        (x, 0.0, (w - SIDEBAR_W - panel).max(0.0), h.max(0.0))
    } else {
        (0.0, CHROME_H, (w - panel).max(0.0), (h - CHROME_H).max(0.0))
    }
}

fn content_rect(w: f64, h: f64) -> Rect {
    let (x, y, cw, ch) = content_area(w, h);
    Rect {
        position: LogicalPosition::new(x, y).into(),
        size: LogicalSize::new(cw, ch).into(),
    }
}

/// Bounds for the AI sidebar: the far-right column, `AI_PANEL_W` wide. It spans the full height in
/// vertical mode (the chrome column is on the left), or starts below the top strip in horizontal.
fn ai_rect(w: f64, h: f64) -> Rect {
    let panel = AI_PANEL_W.load(Ordering::Relaxed) as f64;
    let top = if vertical() { 0.0 } else { CHROME_H };
    Rect {
        position: LogicalPosition::new((w - panel).max(0.0), top).into(),
        size: LogicalSize::new(panel, (h - top).max(0.0)).into(),
    }
}

/// Build the content webview for a tab (used on creation and when recreating a discarded tab).
fn build_content_webview(
    window: &Window,
    id: u32,
    url: &str,
    proxy: EventLoopProxy<UserEvent>,
    engine: &Rc<adblock::Engine>,
    page_url: &Rc<RefCell<String>>,
    web_context: &mut WebContext,
    private: bool,
) -> WebView {
    let size = window.inner_size().to_logical::<f64>(window.scale_factor());
    let sc_proxy = proxy.clone();
    let load_proxy = proxy.clone();
    let ipc_proxy = proxy.clone();
    let nw_proxy = proxy.clone();
    let ev_proxy = proxy.clone();
    // WebView2 refuses to CREATE a webview already pointed at a chrome-extension:// URL (it throws
    // ERROR_INVALID_STATE). Create at about:blank and navigate after build instead.
    let initial_url = if url.starts_with("chrome-extension://") {
        "about:blank"
    } else {
        url
    };
    // All webviews sharing one user-data folder MUST agree on the AreBrowserExtensionsEnabled env
    // option, or creating the second+ webview throws ERROR_INVALID_STATE. The chrome enables it when
    // an extension is present, so content tabs must match. (We don't set extension_path here - the
    // chrome already loaded the extension into the shared profile.) Private tabs live on a separate,
    // extension-free context, so they always pass false (and never collide with the main profile).
    let ext_enabled = !private && store::has_extensions();
    let webview = WebViewBuilder::new_with_web_context(web_context)
        .with_url(initial_url)
        .with_browser_extensions_enabled(ext_enabled)
        .with_devtools(true)
        .with_additional_browser_args(PRIVACY_ARGS)
        // Dark default background so new tabs / loading pages never flash white against the dark UI.
        .with_background_color((13, 15, 19, 255))
        .with_visible(false)
        .with_bounds(content_rect(size.width, size.height))
        .with_initialization_script(CAPTURE_JS)
        .with_ipc_handler(move |req| {
            // Content webviews are untrusted AND file:// pages must not post IPC (wry tags the IPC
            // Request with the page URL, and http::Uri can't parse file:// -> panic/abort). So our
            // home/search pages navigate instead of posting. The only accepted message is a dormant
            // save-login candidate (only ever sent from https pages); it's non-privileged.
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(req.body()) {
                match v["cmd"].as_str() {
                    Some("save_candidate") => {
                        let password = v["password"].as_str().unwrap_or_default().to_string();
                        if !password.is_empty() {
                            let _ = ipc_proxy.send_event(UserEvent::BwSaveCandidate {
                                url: v["url"].as_str().unwrap_or_default().to_string(),
                                username: v["username"].as_str().unwrap_or_default().to_string(),
                                password,
                            });
                        }
                    }
                    // Non-privileged AI hint: the user clicked an action on the selection pill. Carries
                    // only the selected text + which action; cannot drive navigation or app state.
                    Some("ai_selection") => {
                        let text = v["text"].as_str().unwrap_or_default().to_string();
                        if !text.is_empty() {
                            let _ = ipc_proxy.send_event(UserEvent::AiSelection {
                                action: v["action"].as_str().unwrap_or("explain").to_string(),
                                text,
                            });
                        }
                    }
                    // Non-privileged passkey request: the host validates the rpId against this tab's
                    // real origin before ever touching the vault (see UserEvent::WebauthnGet).
                    Some("webauthn_get") => {
                        let _ = ipc_proxy.send_event(UserEvent::WebauthnGet {
                            tab_id: id,
                            req_id: v["reqId"].as_str().unwrap_or_default().to_string(),
                            rp_id: v["rpId"].as_str().unwrap_or_default().to_string(),
                            challenge: v["challenge"].as_str().unwrap_or_default().to_string(),
                        });
                    }
                    _ => {}
                }
            }
        })
        .with_new_window_req_handler(move |url, _features| {
            // Extension-initiated popouts (e.g. Bitwarden's FIDO2 / passkey picker, opened via
            // chrome.windows.create({type:'popup'})) are built to BE a real popup window - forcing
            // them into a tab leaves them blank. Let WebView2 open those natively. Regular web-page
            // popups (target=_blank / window.open) still open in our tab strip.
            if url.starts_with("chrome-extension://") {
                NewWindowResponse::Allow
            } else {
                let _ = nw_proxy.send_event(UserEvent::NewTabUrl(url, Some(id)));
                NewWindowResponse::Deny
            }
        })
        .with_navigation_handler(move |u| {
            let _ = proxy.send_event(UserEvent::PageUrlChanged(id, u));
            true
        })
        .with_on_page_load_handler(move |event, loaded| {
            if matches!(event, wry::PageLoadEvent::Finished) {
                let _ = load_proxy.send_event(UserEvent::PageLoaded(id));
                if loaded.to_lowercase().contains("home.html") {
                    let _ = load_proxy.send_event(UserEvent::HomeReady(id));
                }
            }
        })
        .build_as_child(window)
        .expect("failed to create tab webview");
    // Extension pages (the Bitwarden popup) are navigated to after creation, not at build time.
    if initial_url != url {
        let _ = webview.load_url(url);
    }
    // Chrome-style built-in password manager: prompt to save new logins + autofill them. Stored
    // locally and encrypted in the persistent profile (no cloud sync). This is what makes saving a
    // new login "just work" without any Bitwarden setup.
    unsafe {
        if let Ok(s) = webview.webview().Settings() {
            // Ctrl + mouse-wheel zoom (and Ctrl+/-) at the engine level. Default is on, but set it
            // explicitly so a future default change can't silently disable page zoom.
            let _ = s.SetIsZoomControlEnabled(true);
            if let Ok(s4) = s.cast::<ICoreWebView2Settings4>() {
                let _ = s4.SetIsPasswordAutosaveEnabled(true);
                let _ = s4.SetIsGeneralAutofillEnabled(true);
            }
        }
    }
    blocker::attach(&webview, engine.clone(), page_url.clone());
    attach_shortcuts(&webview, sc_proxy);
    attach_webview_events(&webview, id, ev_proxy);
    webview
}

/// Attach WebView2 events that feed the tab UI: page <title>, favicon, and audio-playing state.
fn attach_webview_events(webview: &WebView, id: u32, proxy: EventLoopProxy<UserEvent>) {
    let core = webview.webview();

    let p_title = proxy.clone();
    let title_handler = DocumentTitleChangedEventHandler::create(Box::new(move |sender, _| {
        if let Some(c) = sender {
            unsafe {
                let mut pw = windows_core::PWSTR::null();
                if c.DocumentTitle(&mut pw).is_ok() && !pw.is_null() {
                    if let Ok(t) = pw.to_string() {
                        let _ = p_title.send_event(UserEvent::PageTitleChanged(id, t));
                    }
                    CoTaskMemFree(Some(pw.0 as *const _));
                }
            }
        }
        Ok(())
    }));

    let p_fav = proxy.clone();
    let fav_handler = FaviconChangedEventHandler::create(Box::new(move |sender, _| {
        if let Some(c15) = sender.and_then(|c| c.cast::<ICoreWebView2_15>().ok()) {
            unsafe {
                let mut pw = windows_core::PWSTR::null();
                if c15.FaviconUri(&mut pw).is_ok() && !pw.is_null() {
                    if let Ok(uri) = pw.to_string() {
                        let _ = p_fav.send_event(UserEvent::PageFaviconChanged(id, uri));
                    }
                    CoTaskMemFree(Some(pw.0 as *const _));
                }
            }
        }
        Ok(())
    }));

    // Zoom: fires when the page zoom changes, including native Ctrl + mouse-wheel. Lets us persist
    // the per-site factor and refresh the omnibox zoom chip for scroll-zoom, not just Ctrl+/-.
    let p_zoom = proxy.clone();
    let zoom_handler = ZoomFactorChangedEventHandler::create(Box::new(move |_sender, _| {
        let _ = p_zoom.send_event(UserEvent::ZoomChanged(id));
        Ok(())
    }));

    let p_audio = proxy.clone();
    let audio_handler =
        IsDocumentPlayingAudioChangedEventHandler::create(Box::new(move |sender, _| {
            if let Some(c8) = sender.and_then(|c| c.cast::<ICoreWebView2_8>().ok()) {
                unsafe {
                    let mut b = windows_core::BOOL(0);
                    if c8.IsDocumentPlayingAudio(&mut b).is_ok() {
                        let _ = p_audio.send_event(UserEvent::PageAudioChanged(id, b.as_bool()));
                    }
                }
            }
            Ok(())
        }));

    // Site permission prompts (notifications/camera/mic/location/...): defer WebView2's default grey
    // bubble and route the request to our own styled prompt in the chrome. We mark the args Handled so
    // the native UI is suppressed, stash (args, deferral) thread-locally, and complete it on the user's
    // choice. Falls back to letting WebView2 handle it if the deferral or Handled hook is unavailable.
    let p_perm = proxy.clone();
    let perm_handler = PermissionRequestedEventHandler::create(Box::new(move |_sender, args| {
        if let Some(args) = args {
            unsafe {
                let mut kind = COREWEBVIEW2_PERMISSION_KIND::default();
                let _ = args.PermissionKind(&mut kind);
                let mut pw = windows_core::PWSTR::null();
                let origin = if args.Uri(&mut pw).is_ok() && !pw.is_null() {
                    let s = pw.to_string().unwrap_or_default();
                    CoTaskMemFree(Some(pw.0 as *const _));
                    s
                } else {
                    String::new()
                };
                let handled = args
                    .cast::<ICoreWebView2PermissionRequestedEventArgs2>()
                    .and_then(|a2| a2.SetHandled(true))
                    .is_ok();
                if handled {
                    if let Ok(def) = args.GetDeferral() {
                        let pid = PERM_SEQ.fetch_add(1, Ordering::Relaxed);
                        PENDING_PERMS.with(|m| m.borrow_mut().insert(pid, (args.clone(), def)));
                        let _ = p_perm.send_event(UserEvent::PermissionRequest {
                            id: pid,
                            origin,
                            kind: kind.0,
                        });
                    }
                }
            }
        }
        Ok(())
    }));

    // Downloads: keep a tab alive while it has an in-progress download. We don't change the download
    // behavior (WebView2's default UI handles it) - we only observe start/finish for the idle sweep.
    let p_dl = proxy.clone();
    let dl_handler = DownloadStartingEventHandler::create(Box::new(move |_sender, args| {
        let _ = p_dl.send_event(UserEvent::DownloadStarted(id));
        if let Some(args) = args {
            unsafe {
                if let Ok(op) = args.DownloadOperation() {
                    let dl_id = DL_COUNTER.fetch_add(1, Ordering::Relaxed);
                    // The intended target path is set by the time DownloadStarting fires; the file
                    // name is its last path component.
                    let path = {
                        let mut pw = windows_core::PWSTR::null();
                        if op.ResultFilePath(&mut pw).is_ok() && !pw.is_null() {
                            let s = pw.to_string().unwrap_or_default();
                            CoTaskMemFree(Some(pw.0 as *const _));
                            s
                        } else {
                            String::new()
                        }
                    };
                    let name = path
                        .rsplit(['\\', '/'])
                        .next()
                        .filter(|s| !s.is_empty())
                        .unwrap_or("download")
                        .to_string();
                    let _ = p_dl.send_event(UserEvent::DownloadAdded { dl_id, name, path });
                    let p_end = p_dl.clone();
                    let sc = StateChangedEventHandler::create(Box::new(move |op2, _| {
                        if let Some(op2) = op2 {
                            let mut st = COREWEBVIEW2_DOWNLOAD_STATE(0);
                            if op2.State(&mut st).is_ok()
                                && (st == COREWEBVIEW2_DOWNLOAD_STATE_COMPLETED
                                    || st == COREWEBVIEW2_DOWNLOAD_STATE_INTERRUPTED)
                            {
                                let _ = p_end.send_event(UserEvent::DownloadEnded(id));
                                let state = if st == COREWEBVIEW2_DOWNLOAD_STATE_COMPLETED {
                                    1
                                } else {
                                    2
                                };
                                let _ = p_end
                                    .send_event(UserEvent::DownloadStateChanged { dl_id, state });
                            }
                        }
                        Ok(())
                    }));
                    let mut tok = 0i64;
                    let _ = op.add_StateChanged(&sc, &mut tok);
                }
            }
        }
        Ok(())
    }));

    unsafe {
        let mut tz = 0i64;
        let _ = webview
            .controller()
            .add_ZoomFactorChanged(&zoom_handler, &mut tz);
        let mut t = 0i64;
        let _ = core.add_DocumentTitleChanged(&title_handler, &mut t);
        let mut tp = 0i64;
        let _ = core.add_PermissionRequested(&perm_handler, &mut tp);
        if let Ok(c4) = core.cast::<ICoreWebView2_4>() {
            let mut t4 = 0i64;
            let _ = c4.add_DownloadStarting(&dl_handler, &mut t4);
        }
        if let Ok(c15) = core.cast::<ICoreWebView2_15>() {
            let mut t2 = 0i64;
            let _ = c15.add_FaviconChanged(&fav_handler, &mut t2);
        }
        if let Ok(c8) = core.cast::<ICoreWebView2_8>() {
            let mut t3 = 0i64;
            let _ = c8.add_IsDocumentPlayingAudioChanged(&audio_handler, &mut t3);
        }
    }
}

fn make_tab(
    window: &Window,
    id: u32,
    url: &str,
    proxy: EventLoopProxy<UserEvent>,
    engine: &Rc<adblock::Engine>,
    web_context: &mut WebContext,
    private: bool,
) -> Tab {
    let page_url = Rc::new(RefCell::new(url.to_string()));
    let webview = Some(build_content_webview(
        window,
        id,
        url,
        proxy,
        engine,
        &page_url,
        web_context,
        private,
    ));
    Tab {
        id,
        webview,
        url: url.to_string(),
        title: tab_label(url),
        suspended: false,
        discarded: false,
        last_active: std::time::Instant::now(),
        last_used: store::unix_now(),
        pinned: false,
        favicon: String::new(),
        audio: false,
        muted: false,
        downloading: 0,
        group: None,
        workspace: 0,
        private,
        page_url,
        trail_from: None,
    }
}

/// A restored background tab that has strip/session metadata but no WebView2 renderer yet. It loads
/// on first activation through `ensure_live`, which makes large saved sessions feel instant at launch.
fn make_lazy_tab(id: u32, url: &str) -> Tab {
    Tab {
        id,
        webview: None,
        url: url.to_string(),
        title: tab_label(url),
        suspended: false,
        discarded: true,
        last_active: std::time::Instant::now(),
        last_used: store::unix_now(),
        pinned: false,
        favicon: String::new(),
        audio: false,
        muted: false,
        downloading: 0,
        group: None,
        workspace: 0,
        private: false,
        page_url: Rc::new(RefCell::new(url.to_string())),
        trail_from: None,
    }
}

/// Recreate a discarded tab's webview (reloads its URL) so it can be shown again.
fn ensure_live(
    tab: &mut Tab,
    window: &Window,
    proxy: EventLoopProxy<UserEvent>,
    engine: &Rc<adblock::Engine>,
    web_context: &mut WebContext,
) {
    if tab.webview.is_none() {
        let url = tab.url.clone();
        let page_url = tab.page_url.clone();
        tab.webview = Some(build_content_webview(
            window,
            tab.id,
            &url,
            proxy,
            engine,
            &page_url,
            web_context,
            tab.private,
        ));
        tab.discarded = false;
        tab.suspended = false;
    }
}

/// Build the chat messages for an AI request. `page_text` is the JSON `{title,url,text}` from
/// AI_EXTRACT_JS (empty for "general" scope or when extraction failed); when present it's handed to
/// the model as grounding context with an instruction not to invent answers.
/// Splice the remembered conversation turns into a fresh request so follow-ups have context. `messages`
/// is the just-built turn (system prompt first, then the current user message(s)); history goes between
/// the system message and the new turn, matching the order the model expects. No-op when history is
/// empty or there's no leading system message.
fn with_ai_history(messages: Vec<ai::Msg>, history: &[ai::Msg]) -> Vec<ai::Msg> {
    if history.is_empty() || messages.first().map(|m| m.role) != Some("system") {
        return messages;
    }
    let mut out = Vec::with_capacity(messages.len() + history.len());
    let mut it = messages.into_iter();
    out.push(it.next().expect("checked non-empty above")); // system
    out.extend(history.iter().cloned());
    out.extend(it);
    out
}

/// Record a finished conversational turn so the next question can see it. Stores the lightweight pair
/// (user question + assistant answer) only, caps the answer length, and trims to the most recent
/// AI_HISTORY_TURNS turns. Skips empty answers (a turn that errored or produced nothing).
fn commit_ai_turn(history: &mut Vec<ai::Msg>, user: &str, answer: &str) {
    let answer = answer.trim();
    if answer.is_empty() {
        return;
    }
    let user = if user.trim().is_empty() { "(no question)" } else { user.trim() };
    let answer: String = answer.chars().take(AI_HISTORY_ANSWER_CHARS).collect();
    history.push(ai::Msg::user(user.to_string()));
    history.push(ai::Msg::assistant(answer));
    let max_msgs = AI_HISTORY_TURNS * 2;
    if history.len() > max_msgs {
        let drop = history.len() - max_msgs;
        history.drain(0..drop);
    }
}

fn build_ai_messages(prompt: &str, page_text: &str) -> Vec<ai::Msg> {
    #[derive(serde::Deserialize, Default)]
    struct PageContext {
        #[serde(default)]
        title: String,
        #[serde(default)]
        url: String,
        #[serde(default)]
        text: String,
    }
    let ctx: PageContext = serde_json::from_str(page_text).unwrap_or_default();
    if ctx.text.trim().is_empty() {
        vec![
            ai::Msg::system(
                "You are Aperture's built-in assistant. Be concise and direct. Plain text, no \
                 markdown headers.",
            ),
            ai::Msg::user(prompt.to_string()),
        ]
    } else {
        // Char-boundary-safe cap (the JSON string is valid UTF-8; truncate() could split a char).
        let text: String = ctx.text.chars().take(AI_CTX_CHARS).collect();
        vec![
            ai::Msg::system(
                "You are Aperture's built-in assistant. Answer the user's question \
                 using the web page provided below. Be concise and direct. If the answer isn't in the \
                 page, say so plainly instead of guessing. Plain text, no markdown headers.",
            ),
            ai::Msg::user(format!(
                "Current page: {} ({})\n\n--- PAGE CONTENT ---\n{}\n--- END PAGE ---\n\nQuestion: {}",
                ctx.title, ctx.url, text, prompt
            )),
        ]
    }
}

/// Spawn a worker thread that streams a chat completion to the sidebar (each token -> AiToken, end ->
/// AiDone). Returns the cancel flag so the run loop can stop it (AiStop or a superseding request).
fn spawn_ai_stream(
    cfg: AiCfg,
    messages: Vec<ai::Msg>,
    req: u64,
    proxy: EventLoopProxy<UserEvent>,
) -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    let cancel = flag.clone();
    std::thread::spawn(move || {
        let done_proxy = proxy.clone();
        let r = ai::chat_stream(
            &cfg.host,
            &cfg.model,
            &cfg.keep_alive,
            &messages,
            |delta| {
                let _ = proxy.send_event(UserEvent::AiToken {
                    req,
                    delta: delta.to_string(),
                });
            },
            || cancel.load(Ordering::Relaxed),
        );
        let _ = done_proxy.send_event(UserEvent::AiDone { req, error: r.err() });
    });
    flag
}

/// Agentic web search: search DuckDuckGo, read the top results, then answer the question grounded in
/// them with inline [n] citations. Runs entirely on a worker thread (blocking fetches + streaming);
/// posts progress notes, the source list, answer tokens, and a final AiDone. Returns the cancel flag.
fn spawn_web_research(
    cfg: AiCfg,
    web_results: usize,
    query: String,
    history: Vec<ai::Msg>,
    req: u64,
    proxy: EventLoopProxy<UserEvent>,
) -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    let cancel = flag.clone();
    std::thread::spawn(move || {
        let note = |t: &str| {
            let _ = proxy.send_event(UserEvent::AiWebStatus {
                req,
                text: t.to_string(),
            });
        };
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        note("Searching the web\u{2026}");
        let hits = search::search(&query);
        if hits.is_empty() {
            let _ = proxy.send_event(UserEvent::AiDone {
                req,
                error: Some("no web results (couldn't reach DuckDuckGo, or nothing found)".into()),
            });
            return;
        }
        let top: Vec<search::Hit> = hits.into_iter().take(web_results.clamp(1, 5)).collect();
        // Read each result page (fall back to the search snippet if a fetch comes back empty).
        let mut context = String::new();
        for (i, h) in top.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            note(&format!("Reading source {}\u{2026}", i + 1));
            let body = search::fetch_readable(&h.url);
            let capped: String = body.chars().take(4000).collect();
            // Always include the search snippet (it often carries the gist even when the page fetch
            // comes back thin or blocked), then the extracted page text when we got any. Feeding both
            // gives the model far more to reason over than either alone.
            context.push_str(&format!("[{}] {} ({})\n", i + 1, h.title, h.url));
            if !h.snippet.trim().is_empty() {
                context.push_str(&format!("Summary: {}\n", h.snippet.trim()));
            }
            if !capped.trim().is_empty() {
                context.push_str(&format!("Page text: {capped}\n"));
            }
            context.push('\n');
        }
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        note("Answering\u{2026}");
        let messages = with_ai_history(
            vec![
                ai::Msg::system(
                    "You are Aperture's assistant. Answer the user's question using the numbered web \
                     sources below. Synthesize across the sources and give a direct, decisive answer, \
                     even when no single source states it outright: infer from titles, summaries, \
                     partial text, and the overall consensus, and make a reasoned best judgment. When \
                     the user asks for a ranking or a recommendation, commit to specific picks and \
                     briefly justify each one from the sources. Only say the sources lack the \
                     information if they contain nothing relevant at all. Cite sources inline as [1], \
                     [2], etc. Be concise. Plain text, no markdown headers.",
                ),
                ai::Msg::user(format!("Sources:\n\n{context}\n---\nQuestion: {query}")),
            ],
            &history,
        );
        let done_proxy = proxy.clone();
        let r = ai::chat_stream(
            &cfg.host,
            &cfg.model,
            &cfg.keep_alive,
            &messages,
            |delta| {
                let _ = proxy.send_event(UserEvent::AiToken {
                    req,
                    delta: delta.to_string(),
                });
            },
            || cancel.load(Ordering::Relaxed),
        );
        // Citation chips after the answer.
        let sources: Vec<(String, String)> =
            top.iter().map(|h| (h.title.clone(), h.url.clone())).collect();
        let _ = proxy.send_event(UserEvent::AiSources { req, sources });
        let _ = done_proxy.send_event(UserEvent::AiDone { req, error: r.err() });
    });
    flag
}

/// Screenshot the active tab's rendered view (PNG) and, on completion, hand the base64 image to the
/// vision model via AiVisionContext. Async: WebView2 invokes the completion handler on the UI thread.
fn capture_active_preview(
    tabs: &[Tab],
    active: u32,
    req: u64,
    prompt: String,
    proxy: EventLoopProxy<UserEvent>,
) {
    let Some(wv) = tabs
        .iter()
        .find(|t| t.id == active)
        .and_then(|t| t.webview.as_ref())
    else {
        let _ = proxy.send_event(UserEvent::AiDone {
            req,
            error: Some("no active page to screenshot".into()),
        });
        return;
    };
    let core = wv.webview();
    unsafe {
        let stream = match CreateStreamOnHGlobal(HGLOBAL(std::ptr::null_mut()), true) {
            Ok(s) => s,
            Err(e) => {
                let _ = proxy.send_event(UserEvent::AiDone {
                    req,
                    error: Some(format!("screenshot init failed: {e}")),
                });
                return;
            }
        };
        let cb_proxy = proxy.clone();
        let cb_stream = stream.clone();
        let handler = CapturePreviewCompletedHandler::create(Box::new(move |hr| {
            if hr.is_ok() {
                match read_stream_bytes(&cb_stream) {
                    Some(bytes) if !bytes.is_empty() => {
                        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                        let _ = cb_proxy.send_event(UserEvent::AiVisionContext {
                            req,
                            prompt: prompt.clone(),
                            image: b64,
                        });
                    }
                    _ => {
                        let _ = cb_proxy.send_event(UserEvent::AiDone {
                            req,
                            error: Some("screenshot was empty".into()),
                        });
                    }
                }
            } else {
                let _ = cb_proxy.send_event(UserEvent::AiDone {
                    req,
                    error: Some("screenshot capture failed".into()),
                });
            }
            Ok(())
        }));
        if let Err(e) =
            core.CapturePreview(COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG, &stream, &handler)
        {
            let _ = proxy.send_event(UserEvent::AiDone {
                req,
                error: Some(format!("screenshot failed: {e}")),
            });
        }
    }
}

/// Capture the active page (visible viewport) to a PNG in the user's Downloads folder, then reveal it
/// in Explorer. Reuses WebView2 CapturePreview (same as the vision path), writing the bytes to a file.
fn save_active_screenshot(tabs: &[Tab], active: u32) {
    let Some(wv) = tabs
        .iter()
        .find(|t| t.id == active)
        .and_then(|t| t.webview.as_ref())
    else {
        return;
    };
    let core = wv.webview();
    unsafe {
        let Ok(stream) = CreateStreamOnHGlobal(HGLOBAL(std::ptr::null_mut()), true) else {
            return;
        };
        let cb_stream = stream.clone();
        let handler = CapturePreviewCompletedHandler::create(Box::new(move |hr| {
            if hr.is_ok() {
                if let Some(bytes) = read_stream_bytes(&cb_stream) {
                    if !bytes.is_empty() {
                        let ts = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        let base = std::env::var("USERPROFILE").unwrap_or_default();
                        let path = format!("{base}\\Downloads\\aperture-{ts}.png");
                        if std::fs::write(&path, &bytes).is_ok() {
                            let _ = std::process::Command::new("explorer")
                                .arg(format!("/select,{path}"))
                                .spawn();
                        }
                    }
                }
            }
            Ok(())
        }));
        let _ = core.CapturePreview(COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG, &stream, &handler);
    }
}

/// Read all bytes out of an in-memory IStream (the captured PNG).
unsafe fn read_stream_bytes(stream: &IStream) -> Option<Vec<u8>> {
    let hglobal = GetHGlobalFromStream(stream).ok()?;
    let size = GlobalSize(hglobal);
    if size == 0 {
        return Some(Vec::new());
    }
    let ptr = GlobalLock(hglobal) as *const u8;
    if ptr.is_null() {
        return None;
    }
    let bytes = std::slice::from_raw_parts(ptr, size).to_vec();
    let _ = GlobalUnlock(hglobal);
    Some(bytes)
}

/// Open the AI sidebar if it's closed (idempotent), narrowing the active tab to make room. Used by
/// host-initiated requests (omnibox "?" question, selection-pill actions) so the answer has somewhere
/// to land even when the panel wasn't already open.
/// Drive one AI-panel slide: a worker thread posts eased frames (~150ms total) back to the run
/// loop, which moves the sidebar's x each frame. `gen` must still match when a frame lands, so a
/// newer toggle silently cancels an in-flight slide.
fn spawn_ai_slide(proxy: EventLoopProxy<UserEvent>, gen: u64, opening: bool) {
    std::thread::spawn(move || {
        // ~60fps over ~200ms: enough frames that the page's edge reads as motion, not steps.
        const FRAMES: u32 = 24;
        for f in 1..=FRAMES {
            std::thread::sleep(std::time::Duration::from_millis(8));
            let t = f as f64 / FRAMES as f64;
            if proxy
                .send_event(UserEvent::AiSlide { gen, t, opening })
                .is_err()
            {
                break;
            }
        }
    });
}

/// The sidebar's rect during a slide: `off` = 0 fully docked, 1 fully off the right edge.
fn ai_slide_rect(window: &Window, off: f64) -> Rect {
    let size = window.inner_size().to_logical::<f64>(window.scale_factor());
    let top = if vertical() { 0.0 } else { CHROME_H };
    Rect {
        position: LogicalPosition::new((size.width - AI_W) + AI_W * off, top).into(),
        size: LogicalSize::new(AI_W, (size.height - top).max(0.0)).into(),
    }
}

/// Resize just the visible content webviews (active + split pane) to the current content_area,
/// touching neither focus nor visibility. Used per-frame by the AI panel slide so the page's edge
/// follows the panel instead of snapping to its final width.
fn resize_content(window: &Window, tabs: &[Tab], active: u32) {
    let size = window.inner_size().to_logical::<f64>(window.scale_factor());
    let (x, y, cw, ch) = content_area(size.width, size.height);
    let split = SPLIT_TAB.load(Ordering::Relaxed);
    let split_on = split != 0 && split != active && tabs.iter().any(|t| t.id == split);
    let gap = 1.0;
    let half = ((cw - gap) / 2.0).max(0.0);
    for t in tabs.iter() {
        let is_active = t.id == active;
        let is_split = split_on && t.id == split;
        if !(is_active || is_split) {
            continue;
        }
        if let Some(wv) = &t.webview {
            let rect = if !split_on {
                Rect {
                    position: LogicalPosition::new(x, y).into(),
                    size: LogicalSize::new(cw, ch).into(),
                }
            } else if is_active {
                Rect {
                    position: LogicalPosition::new(x, y).into(),
                    size: LogicalSize::new(half, ch).into(),
                }
            } else {
                Rect {
                    position: LogicalPosition::new(x + half + gap, y).into(),
                    size: LogicalSize::new(half, ch).into(),
                }
            };
            let _ = wv.set_bounds(rect);
        }
    }
}

fn open_ai_panel(
    window: &Window,
    chrome: &WebView,
    ai: &WebView,
    tabs: &mut [Tab],
    active: u32,
    anim_gen: &mut u64,
    proxy: &EventLoopProxy<UserEvent>,
) {
    // Any open cancels an in-flight slide (e.g. an ask landing while the panel is animating shut,
    // which would otherwise hide the sidebar mid-answer at the stale slide's final frame).
    *anim_gen += 1;
    if AI_PANEL_W.load(Ordering::Relaxed) == 0 {
        // Start fully off the right edge; the slide moves the panel in while the content's edge
        // follows it frame by frame (AiSlide).
        let _ = ai.set_bounds(ai_slide_rect(window, 1.0));
        let _ = ai.set_visible(true);
        spawn_ai_slide(proxy.clone(), *anim_gen, true);
    } else {
        // Already open (or was mid-close, leaving a partial width): settle to fully docked.
        AI_PANEL_W.store(AI_W as u32, Ordering::Relaxed);
        relayout(window, chrome, ai, tabs);
        activate(window, tabs, active);
        let _ = ai.set_visible(true);
    }
}

/// Push AI feature flags to the chrome: whether the assistant is enabled (lens button visibility) and
/// whether a leading "?" in the omnibox routes to the AI.
fn push_chrome_ai_cfg(chrome: &WebView, s: &store::Settings) {
    let cfg = serde_json::json!({ "enabled": s.ai.enabled, "omnibox": s.ai.omnibox_ask });
    let _ = chrome.evaluate_script(&format!("window.__chrome&&window.__chrome.setAiConfig({cfg})"));
}

/// Tell the chrome UI whether to lay its tab strip out as a vertical left column or a horizontal
/// top strip. The chrome reflows via a `body.vertical` class.
fn push_tab_layout(chrome: &WebView, vertical: bool) {
    let _ = chrome.evaluate_script(&format!(
        "window.__chrome&&window.__chrome.setTabLayout({vertical})"
    ));
}

/// Push AI feature flags to the sidebar: which scopes/buttons to offer (Web, vision).
fn push_sidebar_ai_cfg(ai: &WebView, s: &store::Settings) {
    let cfg = serde_json::json!({ "web": s.ai.web_search, "vision": s.ai.vision });
    let _ = ai.evaluate_script(&format!("window.__ai&&window.__ai.config({cfg})"));
}

/// Enable/disable the on-page selection pill across all live tabs (CAPTURE_JS reads `window.__apPill`).
fn push_pill_flag(tabs: &[Tab], on: bool) {
    for t in tabs {
        if let Some(wv) = &t.webview {
            let _ = wv.evaluate_script(&format!("window.__apPill={on}"));
        }
    }
}

/// Lighten an #rrggbb hex by adding `amt` to each channel (for a slightly raised panel shade).
fn lighten(hex: &str, amt: u8) -> String {
    let h = hex.trim_start_matches('#');
    if h.len() != 6 {
        return hex.to_string();
    }
    let ch = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).unwrap_or(0).saturating_add(amt);
    format!("#{:02x}{:02x}{:02x}", ch(0), ch(2), ch(4))
}

/// Build a JS snippet that sets the given CSS custom properties on :root.
fn set_vars_js(pairs: &[(&str, &str)]) -> String {
    let mut s = String::from("(function(){try{var d=document.documentElement.style;");
    for (k, v) in pairs {
        s.push_str(&format!("d.setProperty('{k}','{v}');"));
    }
    s.push_str("}catch(e){}})()");
    s
}

/// Theme -> CSS vars for the chrome UI (its var names differ from the other surfaces).
fn chrome_theme_js(t: &store::Theme) -> String {
    let alt = lighten(&t.panel, 12);
    set_vars_js(&[
        ("--bg-deep", t.background.as_str()),
        ("--bg", t.panel.as_str()),
        ("--bg-elev", alt.as_str()),
        ("--stroke", t.border.as_str()),
        ("--fg", t.text.as_str()),
        ("--fg-dim", t.muted.as_str()),
        ("--accent", t.accent.as_str()),
    ])
}

/// Theme -> CSS vars for the new-tab (home) page.
fn home_theme_js(t: &store::Theme) -> String {
    set_vars_js(&[
        ("--bg", t.background.as_str()),
        ("--panel", t.panel.as_str()),
        ("--line", t.border.as_str()),
        ("--fg", t.text.as_str()),
        ("--dim", t.muted.as_str()),
        ("--accent", t.accent.as_str()),
    ])
}

/// Theme -> CSS vars for the AI sidebar.
fn sidebar_theme_js(t: &store::Theme) -> String {
    let alt = lighten(&t.panel, 12);
    set_vars_js(&[
        ("--bg", t.background.as_str()),
        ("--elev", t.panel.as_str()),
        ("--elev2", alt.as_str()),
        ("--stroke", t.border.as_str()),
        ("--fg", t.text.as_str()),
        ("--dim", t.muted.as_str()),
        ("--accent", t.accent.as_str()),
    ])
}

fn relayout(window: &Window, chrome: &WebView, ai: &WebView, tabs: &[Tab]) {
    let size = window.inner_size().to_logical::<f64>(window.scale_factor());
    let _ = chrome.set_bounds(chrome_rect(size.width, size.height));
    let _ = ai.set_bounds(ai_rect(size.width, size.height));
    for t in tabs {
        if let Some(wv) = &t.webview {
            let _ = wv.set_bounds(content_rect(size.width, size.height));
        }
    }
}

/// Show only the active tab; hide the rest; focus the active one. Background tabs drop to Low
/// memory level. Showing a tab also auto-resumes it from sleep, so clear its `suspended` flag.
fn activate(window: &Window, tabs: &mut [Tab], active: u32) {
    let size = window.inner_size().to_logical::<f64>(window.scale_factor());
    let (x, y, cw, ch) = content_area(size.width, size.height);
    // Split view: active = left pane, SPLIT_TAB = right pane. Only when the split tab exists and isn't
    // the active one.
    let split = SPLIT_TAB.load(Ordering::Relaxed);
    let split_on = split != 0 && split != active && tabs.iter().any(|t| t.id == split);
    let gap = 1.0;
    let half = ((cw - gap) / 2.0).max(0.0);
    let pane_rect = |left: bool| {
        if !split_on {
            Rect {
                position: LogicalPosition::new(x, y).into(),
                size: LogicalSize::new(cw, ch).into(),
            }
        } else if left {
            Rect {
                position: LogicalPosition::new(x, y).into(),
                size: LogicalSize::new(half, ch).into(),
            }
        } else {
            Rect {
                position: LogicalPosition::new(x + half + gap, y).into(),
                size: LogicalSize::new(half, ch).into(),
            }
        }
    };
    for t in tabs.iter_mut() {
        let is_active = t.id == active;
        let is_split = split_on && t.id == split;
        let on = is_active || is_split;
        if on {
            t.suspended = false;
            t.last_active = std::time::Instant::now();
            t.last_used = store::unix_now();
        }
        if let Some(wv) = &t.webview {
            let _ = wv.set_visible(on);
            let _ = wv.set_memory_usage_level(if on {
                MemoryUsageLevel::Normal
            } else {
                MemoryUsageLevel::Low
            });
            if on {
                let _ = wv.set_bounds(pane_rect(is_active));
            }
            if is_active {
                let _ = wv.focus();
            }
        }
    }
    update_window_title(window, tabs, active);
}

fn act_on_active(tabs: &[Tab], active: u32, js: &str) {
    if let Some(t) = tabs.iter().find(|t| t.id == active) {
        if let Some(wv) = &t.webview {
            let _ = wv.evaluate_script(js);
        }
    }
}

fn print_active(tabs: &[Tab], active: u32) {
    let Some(wv) = tabs
        .iter()
        .find(|t| t.id == active)
        .and_then(|t| t.webview.as_ref())
    else {
        return;
    };
    if let Ok(wv16) = wv.webview().cast::<ICoreWebView2_16>() {
        unsafe {
            let _ = wv16.ShowPrintUI(COREWEBVIEW2_PRINT_DIALOG_KIND_BROWSER);
        }
    } else {
        let _ = wv.evaluate_script("window.print()");
    }
}

/// Step the active tab's zoom via the WebView2 controller (dir>0 in, dir<0 out, 0 = reset to 100%).
/// Returns the resulting zoom factor so the caller can remember it per-site.
fn zoom_active(tabs: &[Tab], active: u32, dir: i32) -> Option<f64> {
    let t = tabs.iter().find(|t| t.id == active)?;
    let wv = t.webview.as_ref()?;
    let c = wv.controller();
    unsafe {
        let nz = if dir == 0 {
            1.0
        } else {
            let mut z = 1.0f64;
            let _ = c.ZoomFactor(&mut z);
            (if dir > 0 { z * 1.1 } else { z / 1.1 }).clamp(0.3, 5.0)
        };
        let _ = c.SetZoomFactor(nz);
        Some(nz)
    }
}

/// Read a tab's current zoom factor from its controller (None if it has no live webview).
fn current_zoom(tabs: &[Tab], id: u32) -> Option<f64> {
    let wv = tabs
        .iter()
        .find(|t| t.id == id)
        .and_then(|t| t.webview.as_ref())?;
    let mut z = 1.0f64;
    unsafe {
        let _ = wv.controller().ZoomFactor(&mut z);
    }
    Some(z)
}

/// Persist a tab's current zoom under its host (drop the entry at 100%). No-op for hostless URLs
/// (file://, about:, the home page) or when nothing actually changed (avoids churn on every load).
fn remember_zoom(tabs: &[Tab], id: u32, zoom_levels: &mut HashMap<String, f64>) {
    let Some(z) = current_zoom(tabs, id) else {
        return;
    };
    let Some(t) = tabs.iter().find(|t| t.id == id) else {
        return;
    };
    let host = host_of(&t.url);
    if host.is_empty() {
        return;
    }
    let changed = if (z - 1.0).abs() < 0.001 {
        zoom_levels.remove(&host).is_some()
    } else {
        zoom_levels.insert(host, z) != Some(z)
    };
    if changed {
        store::save_zoom(zoom_levels);
    }
}

/// Apply a stored zoom factor to a specific tab (no-op if it has no live webview).
fn apply_zoom(tabs: &[Tab], id: u32, factor: f64) {
    if let Some(wv) = tabs
        .iter()
        .find(|t| t.id == id)
        .and_then(|t| t.webview.as_ref())
    {
        unsafe {
            let _ = wv.controller().SetZoomFactor(factor);
        }
    }
}

/// Whether a URL's host matches a host-suffix pattern: the exact host or any subdomain of it
/// ("bank.com" covers "www.bank.com", but not "notbank.com").
fn host_matches(url: &str, pattern: &str) -> bool {
    let host = host_of(url);
    if host.is_empty() {
        return false;
    }
    let p = pattern.trim().trim_start_matches("www.").to_lowercase();
    !p.is_empty() && (host == p || host.ends_with(&format!(".{p}")))
}

/// Whether a URL's host matches the user's trail-exclusion list.
fn trail_excluded(url: &str, exclude: &[String]) -> bool {
    exclude.iter().any(|e| host_matches(url, e))
}

/// The host (eTLD+1-ish, just the host component) of a URL, for per-site zoom keys. Empty for
/// non-http(s) URLs (file://, about:, the home page).
fn host_of(url: &str) -> String {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"));
    match rest {
        Some(r) => r
            .split(['/', '?', '#'])
            .next()
            .unwrap_or("")
            .trim_start_matches("www.")
            .to_string(),
        None => String::new(),
    }
}

/// The host of a URL WITHOUT stripping www or the port (for WebAuthn rpId checks + origin building).
fn host_only(url: &str) -> String {
    url.strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .and_then(|r| r.split(['/', '?', '#', ':']).next())
        .unwrap_or("")
        .to_string()
}

/// The web origin (scheme://host[:port]) of an https URL, e.g. "https://example.com". The exact
/// string a WebAuthn relying party expects in clientDataJSON.origin.
fn origin_of(url: &str) -> String {
    match url.strip_prefix("https://") {
        Some(rest) => {
            let hostport = rest.split(['/', '?', '#']).next().unwrap_or("");
            format!("https://{hostport}")
        }
        None => String::new(),
    }
}

/// Deliver a signed passkey assertion back to the page that called navigator.credentials.get().
fn deliver_assertion(tabs: &[Tab], tab_id: u32, req_id: &str, a: &webauthn::Assertion) {
    let Some(wv) = tabs
        .iter()
        .find(|t| t.id == tab_id)
        .and_then(|t| t.webview.as_ref())
    else {
        return;
    };
    let q = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
    let js = format!(
        "window.__apWaResolve&&window.__apWaResolve({},{{credentialId:{},authenticatorData:{},signature:{},clientDataJSON:{},userHandle:{}}})",
        q(req_id),
        q(&a.credential_id_b64u),
        q(&a.authenticator_data_b64u),
        q(&a.signature_b64u),
        q(&a.client_data_json_b64u),
        q(&a.user_handle_b64u),
    );
    let _ = wv.evaluate_script(&js);
}

/// Tell the page its passkey request failed (rejects the navigator.credentials.get() promise).
fn webauthn_reject(tabs: &[Tab], tab_id: u32, req_id: &str, msg: &str) {
    if let Some(wv) = tabs
        .iter()
        .find(|t| t.id == tab_id)
        .and_then(|t| t.webview.as_ref())
    {
        let q = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
        let _ = wv.evaluate_script(&format!(
            "window.__apWaReject&&window.__apWaReject({},{})",
            q(req_id),
            q(msg)
        ));
    }
}

/// Tell the page to run its ORIGINAL navigator.credentials.get() (we have no vault passkey for it).
fn webauthn_fallback(tabs: &[Tab], tab_id: u32, req_id: &str) {
    if let Some(wv) = tabs
        .iter()
        .find(|t| t.id == tab_id)
        .and_then(|t| t.webview.as_ref())
    {
        let q = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
        let _ = wv.evaluate_script(&format!(
            "window.__apWaFallback&&window.__apWaFallback({})",
            q(req_id)
        ));
    }
}

/// Set the OS window title to "<active page title> - Aperture" (Chrome-style).
fn update_window_title(window: &Window, tabs: &[Tab], active: u32) {
    let title = tabs
        .iter()
        .find(|t| t.id == active)
        .map(|t| t.title.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("New Tab");
    let suffix = PROFILE_SUFFIX.get().map(String::as_str).unwrap_or("");
    window.set_title(&format!("{title} - {APP_TITLE}{suffix}"));
}

/// Stop the active tab's current load (Esc).
fn stop_active(tabs: &[Tab], active: u32) {
    if let Some(t) = tabs.iter().find(|t| t.id == active) {
        if let Some(wv) = &t.webview {
            unsafe {
                let _ = wv.webview().Stop();
            }
        }
    }
}

/// Remember a closed tab's URL so Ctrl+Shift+T can reopen it (skips blank/home; caps the stack).
fn remember_closed(stack: &mut Vec<String>, url: &str) {
    if url.is_empty() || url.starts_with("about:") || url.contains("home.html") {
        return;
    }
    stack.push(url.to_string());
    if stack.len() > 25 {
        stack.remove(0);
    }
}

/// Native WebView2 back/forward on the active tab (reliable, unlike JS history.back()).
fn nav_active(tabs: &[Tab], active: u32, forward: bool) {
    if let Some(t) = tabs.iter().find(|t| t.id == active) {
        if let Some(wv) = &t.webview {
            let core = wv.webview();
            unsafe {
                let _ = if forward {
                    core.GoForward()
                } else {
                    core.GoBack()
                };
            }
        }
    }
}

/// Expand the chrome webview to fill the window and hide the active page, so the Bitwarden
/// unlock modal (drawn inside the chrome webview) appears on top of everything.
fn enter_bw_modal(window: &Window, chrome: &WebView, ai: &WebView, tabs: &[Tab], active: u32) {
    let size = window.inner_size().to_logical::<f64>(window.scale_factor());
    if let Some(t) = tabs.iter().find(|t| t.id == active) {
        if let Some(wv) = &t.webview {
            let _ = wv.set_visible(false);
        }
    }
    let _ = chrome.set_bounds(Rect {
        position: LogicalPosition::new(0.0, 0.0).into(),
        size: LogicalSize::new(size.width, size.height).into(),
    });
    // Hide the AI sidebar while a full-window modal is up (it sits above the chrome in z-order).
    let _ = ai.set_visible(false);
    let _ = chrome.evaluate_script("window.__chrome&&window.__chrome.showBw()");
}

/// Hide the modal, shrink the chrome webview back to the top strip, and re-show the active tab.
fn exit_bw_modal(window: &Window, chrome: &WebView, ai: &WebView, tabs: &mut [Tab], active: u32) {
    let _ = chrome.evaluate_script("window.__chrome&&window.__chrome.hideBw()");
    let size = window.inner_size().to_logical::<f64>(window.scale_factor());
    let _ = chrome.set_bounds(chrome_rect(size.width, size.height));
    activate(window, tabs, active);
    // Restore the AI sidebar if it was open before the modal.
    let _ = ai.set_visible(AI_PANEL_W.load(Ordering::Relaxed) > 0);
}

/// Push live pinned/recent data and the user's quick links into a home page that just loaded (or
/// whose links were just edited in settings). A no-op on non-home pages (window.__home is absent).
fn inject_home_data(
    tabs: &[Tab],
    id: u32,
    history: &[store::HistoryEntry],
    links: &[store::QuickLink],
    settings: &store::Settings,
    trail: &[store::TrailVisit],
) {
    let pinned: Vec<serde_json::Value> = tabs
        .iter()
        .filter(|t| t.pinned)
        .map(|t| serde_json::json!({ "title": t.title, "url": t.url }))
        .collect();
    let recent: Vec<serde_json::Value> = history
        .iter()
        .take(8)
        .map(|h| serde_json::json!({ "title": h.title, "url": h.url }))
        .collect();
    let links_j: Vec<serde_json::Value> = links
        .iter()
        .map(|l| serde_json::json!({ "icon": l.icon, "label": l.label, "url": l.url }))
        .collect();
    // The trail graph gets raw visits (newest-last) and filters to the picked day-range client
    // side, so switching ranges on the page needs no round trip. Payload capped for big trails.
    const TRAIL_PUSH_CAP: usize = 1500;
    let visits: Vec<serde_json::Value> = trail
        .iter()
        .skip(trail.len().saturating_sub(TRAIL_PUSH_CAP))
        .map(|v| serde_json::json!({ "u": v.url, "t": v.title, "ts": v.ts, "f": v.from }))
        .collect();
    let data = serde_json::json!({
        "pinned": pinned,
        "recent": recent,
        "links": links_j,
        "name": settings.name,
        "search": settings.search_url,
        "trail": {
            "on": settings.trail.enabled,
            "days": settings.trail.graph_days,
            "visits": visits,
        },
    });
    if let (Some(t), Ok(js)) = (
        tabs.iter().find(|t| t.id == id),
        serde_json::to_string(&data),
    ) {
        if let Some(wv) = &t.webview {
            let _ = wv.evaluate_script(&format!("window.__home&&window.__home.setData({js})"));
        }
    }
}

/// Expand the chrome webview to fill the window and hide the active page, then show the settings
/// panel (drawn inside the chrome webview), seeded with the current quick links.
fn enter_settings_modal(
    window: &Window,
    chrome: &WebView,
    ai: &WebView,
    tabs: &[Tab],
    active: u32,
    links_json: &str,
    prefs_json: &str,
    is_home: bool,
) {
    let size = window.inner_size().to_logical::<f64>(window.scale_factor());
    if let Some(t) = tabs.iter().find(|t| t.id == active) {
        if let Some(wv) = &t.webview {
            let _ = wv.set_visible(false);
        }
    }
    let _ = chrome.set_bounds(Rect {
        position: LogicalPosition::new(0.0, 0.0).into(),
        size: LogicalSize::new(size.width, size.height).into(),
    });
    // Hide the AI sidebar while a full-window modal is up (it sits above the chrome in z-order).
    let _ = ai.set_visible(false);
    let _ = chrome.evaluate_script(&format!(
        "window.__chrome&&window.__chrome.showSettings({links_json},{prefs_json},{is_home})"
    ));
}

/// Hide the settings panel, shrink the chrome webview back to the top strip, re-show the active tab.
fn exit_settings_modal(window: &Window, chrome: &WebView, ai: &WebView, tabs: &mut [Tab], active: u32) {
    let _ = chrome.evaluate_script("window.__chrome&&window.__chrome.hideSettings()");
    let size = window.inner_size().to_logical::<f64>(window.scale_factor());
    let _ = chrome.set_bounds(chrome_rect(size.width, size.height));
    activate(window, tabs, active);
    // Restore the AI sidebar if it was open before the modal.
    let _ = ai.set_visible(AI_PANEL_W.load(Ordering::Relaxed) > 0);
}

/// Grow the chrome to fill the window and show the "save login to Bitwarden?" prompt for `site`.
fn enter_save_modal(
    window: &Window,
    chrome: &WebView,
    ai: &WebView,
    tabs: &[Tab],
    active: u32,
    site: &str,
    username: &str,
) {
    let size = window.inner_size().to_logical::<f64>(window.scale_factor());
    if let Some(t) = tabs.iter().find(|t| t.id == active) {
        if let Some(wv) = &t.webview {
            let _ = wv.set_visible(false);
        }
    }
    let _ = chrome.set_bounds(Rect {
        position: LogicalPosition::new(0.0, 0.0).into(),
        size: LogicalSize::new(size.width, size.height).into(),
    });
    // Hide the AI sidebar while a full-window modal is up (it sits above the chrome in z-order).
    let _ = ai.set_visible(false);
    let info = serde_json::json!({ "site": site, "username": username });
    let js = serde_json::to_string(&info).unwrap_or_else(|_| "{}".into());
    let _ = chrome.evaluate_script(&format!("window.__chrome&&window.__chrome.showSave({js})"));
}

/// Hide the save prompt, shrink the chrome back to the top strip, re-show the active tab.
fn exit_save_modal(window: &Window, chrome: &WebView, ai: &WebView, tabs: &mut [Tab], active: u32) {
    let _ = chrome.evaluate_script("window.__chrome&&window.__chrome.hideSave()");
    let size = window.inner_size().to_logical::<f64>(window.scale_factor());
    let _ = chrome.set_bounds(chrome_rect(size.width, size.height));
    activate(window, tabs, active);
    // Restore the AI sidebar if it was open before the modal.
    let _ = ai.set_visible(AI_PANEL_W.load(Ordering::Relaxed) > 0);
}

/// Grow the chrome to fill the window and show the command palette (Ctrl+K).
fn enter_palette_modal(window: &Window, chrome: &WebView, ai: &WebView, tabs: &[Tab], active: u32) {
    let size = window.inner_size().to_logical::<f64>(window.scale_factor());
    if let Some(t) = tabs.iter().find(|t| t.id == active) {
        if let Some(wv) = &t.webview {
            let _ = wv.set_visible(false);
        }
    }
    let _ = chrome.set_bounds(Rect {
        position: LogicalPosition::new(0.0, 0.0).into(),
        size: LogicalSize::new(size.width, size.height).into(),
    });
    // Hide the AI sidebar while a full-window modal is up (it sits above the chrome in z-order).
    let _ = ai.set_visible(false);
    let _ = chrome.evaluate_script("window.__chrome&&window.__chrome.showPalette()");
}

fn exit_palette_modal(window: &Window, chrome: &WebView, ai: &WebView, tabs: &mut [Tab], active: u32) {
    let _ = chrome.evaluate_script("window.__chrome&&window.__chrome.hidePalette()");
    let size = window.inner_size().to_logical::<f64>(window.scale_factor());
    let _ = chrome.set_bounds(chrome_rect(size.width, size.height));
    activate(window, tabs, active);
    // Restore the AI sidebar if it was open before the modal.
    let _ = ai.set_visible(AI_PANEL_W.load(Ordering::Relaxed) > 0);
}

/// Grow the chrome to fill the window and show the history viewer (Ctrl+H), seeded with the history.
fn enter_history_modal(
    window: &Window,
    chrome: &WebView,
    ai: &WebView,
    tabs: &[Tab],
    active: u32,
    history: &[store::HistoryEntry],
) {
    let size = window.inner_size().to_logical::<f64>(window.scale_factor());
    // History fades in OVER the live page (raised transparent chrome), and fades back out to it on
    // close - no hide/blank of the content. Fallback: if the chrome can't be raised, hide the page
    // the old way so the viewer isn't invisible underneath it.
    if !raise_chrome() {
        if let Some(t) = tabs.iter().find(|t| t.id == active) {
            if let Some(wv) = &t.webview {
                let _ = wv.set_visible(false);
            }
        }
        let _ = ai.set_visible(false);
    }
    let _ = chrome.set_bounds(Rect {
        position: LogicalPosition::new(0.0, 0.0).into(),
        size: LogicalSize::new(size.width, size.height).into(),
    });
    push_history(chrome, history);
    let _ = chrome.evaluate_script("window.__chrome&&window.__chrome.openHistory()");
}

fn exit_history_modal(window: &Window, chrome: &WebView, ai: &WebView, tabs: &mut [Tab], active: u32) {
    let _ = chrome.evaluate_script("window.__chrome&&window.__chrome.closeHistory()");
    let size = window.inner_size().to_logical::<f64>(window.scale_factor());
    let _ = chrome.set_bounds(chrome_rect(size.width, size.height));
    activate(window, tabs, active);
    // Restore the AI sidebar if it was open before the modal.
    let _ = ai.set_visible(AI_PANEL_W.load(Ordering::Relaxed) > 0);
}

/// Inject credentials into the active page's login form (first password field + a likely username
/// field). Values are JSON-escaped before being placed into the script.
fn inject_credentials(tabs: &[Tab], active: u32, username: &str, password: &str) {
    let Some(t) = tabs.iter().find(|t| t.id == active) else {
        return;
    };
    let Some(wv) = &t.webview else {
        return;
    };
    let (Ok(u), Ok(p)) = (
        serde_json::to_string(username),
        serde_json::to_string(password),
    ) else {
        return;
    };
    let template = r#"(function(){
  var set=function(el,val){ if(el){ el.value=val; el.dispatchEvent(new Event('input',{bubbles:true})); el.dispatchEvent(new Event('change',{bubbles:true})); } };
  set(document.querySelector('input[type=password]'), __BW_PW__);
  set(document.querySelector('input[type=email],input[name=username],input[id=username],input[name=email],input[autocomplete=username],input[type=text]'), __BW_USER__);
})()"#;
    let js = template.replace("__BW_PW__", &p).replace("__BW_USER__", &u);
    let _ = wv.evaluate_script(&js);
}

fn active_url(tabs: &[Tab], active: u32) -> String {
    tabs.iter()
        .find(|t| t.id == active)
        .map(|t| t.url.clone())
        .unwrap_or_default()
}

/// Shown (as a new tab) when the Bitwarden button is pressed but no extension is installed yet.
const EXT_HELP_URL: &str = "data:text/html,<body style='background:%230d0f13;color:%23d2d7df;font:15px/1.6 system-ui,sans-serif;max-width:640px;margin:60px auto;padding:0 24px'><h2 style='color:%23fff'>No password-manager extension installed</h2><p>Put the unpacked Bitwarden extension (a folder containing manifest.json) into:</p><pre style='background:%2314171c;padding:12px 14px;border-radius:8px;color:%235b9cff'>%APPDATA%\\RustBrowser\\extensions\\</pre><p>Then restart Aperture and press the Bitwarden button again.</p></body>";

/// Read a COM-allocated PWSTR into an owned String and free it (no-op for null).
unsafe fn take_pwstr(p: windows_core::PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    let s = p.to_string().unwrap_or_default();
    CoTaskMemFree(Some(p.0 as *const core::ffi::c_void));
    s
}

/// Reload a page bypassing the HTTP cache (Chrome's hard refresh), via the DevTools protocol:
/// Page.reload with ignoreCache. Plain location.reload() would happily serve from cache.
fn hard_reload(wv: &WebView) {
    use windows::core::PCWSTR;
    let method: Vec<u16> = "Page.reload"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let params: Vec<u16> = r#"{"ignoreCache":true}"#
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let handler =
        webview2_com::CallDevToolsProtocolMethodCompletedHandler::create(Box::new(|_, _| Ok(())));
    unsafe {
        let _ = wv.webview().CallDevToolsProtocolMethod(
            PCWSTR(method.as_ptr()),
            PCWSTR(params.as_ptr()),
            &handler,
        );
    }
}

/// Delete every cookie in the shared profile (signs out of all sites). The "Clear cookies now" button.
fn clear_all_cookies(wv: &WebView) {
    if let Ok(c2) = wv.webview().cast::<ICoreWebView2_2>() {
        unsafe {
            if let Ok(cm) = c2.CookieManager() {
                let _ = cm.DeleteAllCookies();
            }
        }
    }
}

/// Delete cookies for every domain NOT in `allow` (the keep-signed-in list). Async via the cookie
/// manager. Used at startup when "clear other sites on launch" is enabled. A domain is kept if it
/// equals an allow entry or is a subdomain of it (so "google.com" keeps "mail.google.com").
fn clear_non_allowlisted_cookies(wv: &WebView, allow: Vec<String>) {
    let Ok(c2) = wv.webview().cast::<ICoreWebView2_2>() else {
        return;
    };
    let allow: Vec<String> = allow
        .into_iter()
        .map(|a| a.trim().trim_start_matches('.').to_lowercase())
        .filter(|a| !a.is_empty())
        .collect();
    unsafe {
        let Ok(cm) = c2.CookieManager() else {
            return;
        };
        let cm_del = cm.clone();
        let handler = GetCookiesCompletedHandler::create(Box::new(move |_hr, list| {
            if let Some(list) = list {
                let mut count = 0u32;
                let _ = list.Count(&mut count);
                for i in 0..count {
                    let Ok(cookie) = list.GetValueAtIndex(i) else {
                        continue;
                    };
                    let mut dp = windows_core::PWSTR::null();
                    let _ = cookie.Domain(&mut dp);
                    let domain = take_pwstr(dp).trim_start_matches('.').to_lowercase();
                    let keep = allow
                        .iter()
                        .any(|a| domain == *a || domain.ends_with(&format!(".{a}")));
                    if !keep {
                        let _ = cm_del.DeleteCookie(&cookie);
                    }
                }
            }
            Ok(())
        }));
        let _ = cm.GetCookies(windows_core::PCWSTR::null(), &handler);
    }
}

/// Enumerate the loaded browser extensions, pick Bitwarden (or the first one), and open its popup
/// page in a new tab so the user can unlock the vault. WebView2 returns the list asynchronously, so
/// the chosen popup URL is posted back to the run loop as a NewTabUrl event.
fn open_extension_popup(chrome: &WebView, proxy: EventLoopProxy<UserEvent>) {
    unsafe {
        let Ok(c13) = chrome.webview().cast::<ICoreWebView2_13>() else {
            return;
        };
        let Ok(profile) = c13.Profile() else {
            return;
        };
        let Ok(profile) = profile.cast::<ICoreWebView2Profile7>() else {
            return;
        };
        let handler =
            ProfileGetBrowserExtensionsCompletedHandler::create(Box::new(move |_hr, list| {
                let Some(list) = list else {
                    return Ok(());
                };
                let mut count = 0u32;
                let _ = list.Count(&mut count);
                eprintln!("[ext] GetBrowserExtensions returned {count} extension(s)");
                let mut chosen: Option<String> = None;
                for i in 0..count {
                    let Ok(ext) = list.GetValueAtIndex(i) else {
                        continue;
                    };
                    let mut idp = windows_core::PWSTR::null();
                    let _ = ext.Id(&mut idp);
                    let id = take_pwstr(idp);
                    let mut namep = windows_core::PWSTR::null();
                    let _ = ext.Name(&mut namep);
                    let name = take_pwstr(namep);
                    let mut enabled = windows_core::BOOL(0);
                    let _ = ext.IsEnabled(&mut enabled);
                    eprintln!(
                        "[ext]   [{i}] id={id} name={name:?} enabled={}",
                        enabled.as_bool()
                    );
                    if name.to_lowercase().contains("bitwarden") {
                        chosen = Some(id);
                        break;
                    }
                    if chosen.is_none() {
                        chosen = Some(id);
                    }
                }
                match &chosen {
                    Some(id) => {
                        // Bitwarden's MV3 browser-action popup page.
                        let url = format!("chrome-extension://{id}/popup/index.html");
                        eprintln!("[ext] opening popup: {url}");
                        let _ = proxy.send_event(UserEvent::NewTabUrl(url, None));
                    }
                    None => eprintln!("[ext] no extension found to open"),
                }
                Ok(())
            }));
        let _ = profile.GetBrowserExtensions(&handler);
    }
}

/// Inline-autocomplete: the most-recent history (then bookmark) URL whose display form (scheme and
/// leading www. stripped) starts with what the user typed. Returns that display form to complete to.
fn best_completion(
    history: &[store::HistoryEntry],
    bookmarks: &[store::Bookmark],
    typed: &str,
) -> Option<String> {
    let q = typed.trim().to_lowercase();
    if q.len() < 2 || q.contains(' ') || q.contains("://") {
        return None;
    }
    let strip = |u: &str| -> String {
        let s = u
            .strip_prefix("https://")
            .or_else(|| u.strip_prefix("http://"))
            .unwrap_or(u);
        s.strip_prefix("www.")
            .unwrap_or(s)
            .trim_end_matches('/')
            .to_string()
    };
    history
        .iter()
        .map(|h| h.url.as_str())
        .chain(bookmarks.iter().map(|b| b.url.as_str()))
        .map(strip)
        .find(|disp| disp.len() > q.len() && disp.to_lowercase().starts_with(&q))
}

/// Best-effort sleep a backgrounded tab via WebView2 TrySuspend (pauses timers/scripts, makes the
/// renderer memory reclaimable; auto-resumes when shown). Precondition (satisfied: only called on
/// non-active tabs, which `activate` hid): the controller IsVisible is false. Returns true if issued.
fn suspend_webview(webview: &WebView) -> bool {
    let core = webview.webview(); // ICoreWebView2 (version-matched to wry)
    match core.cast::<ICoreWebView2_3>() {
        Ok(wv3) => {
            let handler =
                TrySuspendCompletedHandler::create(Box::new(|_result, _is_successful| Ok(())));
            unsafe { wv3.TrySuspend(&handler).is_ok() }
        }
        Err(_) => false,
    }
}

/// Wake a suspended webview without showing it. Used by hover prewake: resuming while the pointer is
/// still over the tab hides the wake-up latency, so the page is live by the time the user clicks.
fn resume_webview(webview: &WebView) -> bool {
    let core = webview.webview();
    match core.cast::<ICoreWebView2_3>() {
        Ok(wv3) => unsafe { wv3.Resume().is_ok() },
        Err(_) => false,
    }
}

/// Rebindable shortcut actions: (action id, label shown in Settings, default binding). The chrome's
/// Keyboard settings section mirrors this table (KB_ACTIONS in chrome.html) - keep them in sync.
/// Bindings are "mod+mod+key" strings; keys are physical-key names matching `vk_from_name`.
const SHORTCUT_ACTIONS: &[(&str, &str, &str)] = &[
    ("new_tab", "New tab", "ctrl+t"),
    ("reopen_tab", "Reopen closed tab", "ctrl+shift+t"),
    ("new_private_tab", "New private tab", "ctrl+shift+n"),
    ("close_tab", "Close tab", "ctrl+w"),
    ("next_tab", "Next tab", "ctrl+tab"),
    ("prev_tab", "Previous tab", "ctrl+shift+tab"),
    ("back", "Back", "alt+left"),
    ("forward", "Forward", "alt+right"),
    ("reload", "Reload page", "ctrl+r"),
    ("hard_reload", "Hard reload (bypass cache)", "ctrl+shift+r"),
    ("focus_omnibox", "Focus address bar", "ctrl+l"),
    ("bookmark", "Bookmark this page", "ctrl+d"),
    ("find", "Find in page", "ctrl+f"),
    ("history", "History", "ctrl+h"),
    ("palette", "Command palette", "ctrl+k"),
    ("ai_panel", "AI sidebar", "alt+g"),
    ("split", "Split view", "ctrl+\\"),
    ("print", "Print", "ctrl+p"),
    ("zoom_in", "Zoom in", "ctrl+="),
    ("zoom_out", "Zoom out", "ctrl+-"),
    ("zoom_reset", "Reset zoom", "ctrl+0"),
];

/// Windows virtual-key code for a physical-key name (the same names the settings capture UI emits
/// from KeyboardEvent.code, so bindings are keyboard-layout independent).
fn vk_from_name(name: &str) -> Option<u32> {
    let n = name.trim().to_lowercase();
    let b = n.as_bytes();
    match n.as_str() {
        _ if b.len() == 1 && b[0].is_ascii_lowercase() => Some(b[0].to_ascii_uppercase() as u32),
        _ if b.len() == 1 && b[0].is_ascii_digit() => Some(b[0] as u32),
        "tab" => Some(0x09),
        "enter" => Some(0x0D),
        "space" => Some(0x20),
        "backspace" => Some(0x08),
        "delete" => Some(0x2E),
        "insert" => Some(0x2D),
        "home" => Some(0x24),
        "end" => Some(0x23),
        "pageup" => Some(0x21),
        "pagedown" => Some(0x22),
        "left" => Some(0x25),
        "up" => Some(0x26),
        "right" => Some(0x27),
        "down" => Some(0x28),
        "=" => Some(0xBB),
        "-" => Some(0xBD),
        "," => Some(0xBC),
        "." => Some(0xBE),
        "/" => Some(0xBF),
        ";" => Some(0xBA),
        "'" => Some(0xDE),
        "[" => Some(0xDB),
        "]" => Some(0xDD),
        "\\" => Some(0xDC),
        "`" => Some(0xC0),
        _ if n.len() >= 2 && b[0] == b'f' => {
            n[1..].parse::<u32>().ok().filter(|f| (1..=12).contains(f)).map(|f| 0x6F + f)
        }
        _ => None,
    }
}

/// Parse a "ctrl+shift+t" binding into the packed (modifiers, vk) key used by the shortcut map.
/// Modifier bits: 1 = Ctrl, 2 = Shift, 4 = Alt. None if the key name is unknown or no key present.
fn parse_binding(spec: &str) -> Option<(u8, u32)> {
    let mut mods = 0u8;
    let mut vk = None;
    for part in spec.split('+') {
        match part.trim().to_lowercase().as_str() {
            "ctrl" | "control" => mods |= 1,
            "shift" => mods |= 2,
            "alt" => mods |= 4,
            k => vk = vk_from_name(k),
        }
    }
    vk.map(|v| (mods, v))
}

/// The live (modifiers, vk) -> action-id map consulted by every accelerator handler. Rebuilt from
/// defaults + user overrides at startup, on settings save, and on import.
static SHORTCUT_MAP: std::sync::Mutex<Option<HashMap<(u8, u32), String>>> =
    std::sync::Mutex::new(None);

fn rebuild_shortcut_map(overrides: &std::collections::HashMap<String, String>) {
    let mut m: HashMap<(u8, u32), String> = HashMap::new();
    // Fixed aliases first (lowest priority - a user binding to the same combo wins below):
    // Ctrl+M for split (some layouts lack \), numpad +/-/0 for zoom, Ctrl+1..9 for tab jumps.
    m.insert((1, 0x4D), "split".into());
    // Chrome parity: Ctrl+F5 also hard-reloads (F5 / Shift+F5 are fixed in attach_shortcuts,
    // since the map only sees Ctrl/Alt combos).
    m.insert((1, 0x74), "hard_reload".into());
    m.insert((1, 0x6B), "zoom_in".into());
    m.insert((3, 0xBB), "zoom_in".into()); // Ctrl+Shift+= - how "Ctrl and plus" is actually typed
    m.insert((1, 0x6D), "zoom_out".into());
    m.insert((1, 0x60), "zoom_reset".into());
    for i in 0..8u32 {
        m.insert((1, 0x31 + i), format!("tab_{}", i + 1));
    }
    m.insert((1, 0x39), "tab_last".into());
    for (id, _, default) in SHORTCUT_ACTIONS {
        let spec = overrides.get(*id).map(String::as_str).unwrap_or(default);
        // An unparseable override falls back to the default rather than silently losing the action.
        if let Some(key) = parse_binding(spec).or_else(|| parse_binding(default)) {
            m.insert(key, (*id).to_string());
        }
    }
    *SHORTCUT_MAP.lock().unwrap() = Some(m);
}

fn lookup_shortcut(ctrl: bool, shift: bool, alt: bool, vk: u32) -> Option<String> {
    let mods = (ctrl as u8) | ((shift as u8) << 1) | ((alt as u8) << 2);
    SHORTCUT_MAP.lock().unwrap().as_ref()?.get(&(mods, vk)).cloned()
}

/// The event a shortcut action fires. Split from the map so bindings stay plain data.
fn shortcut_event(action: &str) -> Option<UserEvent> {
    Some(match action {
        "new_tab" => UserEvent::NewTab,
        "reopen_tab" => UserEvent::ReopenClosed,
        "new_private_tab" => UserEvent::NewPrivateTab,
        "close_tab" => UserEvent::CloseActiveTab,
        "next_tab" => UserEvent::CycleTab(true),
        "prev_tab" => UserEvent::CycleTab(false),
        "back" => UserEvent::Back,
        "forward" => UserEvent::Forward,
        "reload" => UserEvent::Reload,
        "hard_reload" => UserEvent::HardReload,
        "focus_omnibox" => UserEvent::FocusOmnibox,
        "bookmark" => UserEvent::BookmarkAdd,
        "find" => UserEvent::ToggleFind,
        "history" => UserEvent::OpenHistory,
        "palette" => UserEvent::OpenPalette,
        "ai_panel" => UserEvent::ToggleAi,
        "split" => UserEvent::ToggleSplit,
        "print" => UserEvent::Print,
        "zoom_in" => UserEvent::Zoom(1),
        "zoom_out" => UserEvent::Zoom(-1),
        "zoom_reset" => UserEvent::Zoom(0),
        "tab_last" => UserEvent::SwitchToIndex(usize::MAX),
        _ => {
            let idx = action.strip_prefix("tab_")?.parse::<usize>().ok()?;
            UserEvent::SwitchToIndex(idx.checked_sub(1)?)
        }
    })
}

/// Register keyboard shortcuts on a webview's controller via WebView2's AcceleratorKeyPressed event
/// (fires on the host side, before the page). Attached to every webview (chrome + each tab) so
/// shortcuts work regardless of which has focus. Bindings come from the rebindable SHORTCUT_MAP.
fn attach_shortcuts(webview: &WebView, proxy: EventLoopProxy<UserEvent>) {
    let controller = webview.controller();
    let handler = AcceleratorKeyPressedEventHandler::create(Box::new(move |_controller, args| {
        let Some(args) = args else {
            return Ok(());
        };
        unsafe {
            let mut kind = COREWEBVIEW2_KEY_EVENT_KIND(0);
            let _ = args.KeyEventKind(&mut kind);
            if kind != COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN
                && kind != COREWEBVIEW2_KEY_EVENT_KIND_SYSTEM_KEY_DOWN
            {
                return Ok(());
            }
            // Modifier state (high bit of GetKeyState => key held).
            let ctrl = GetKeyState(0x11) < 0;
            let alt = GetKeyState(0x12) < 0;
            let shift = GetKeyState(0x10) < 0;
            let mut vk: u32 = 0;
            let _ = args.VirtualKey(&mut vk);
            // (event to fire, whether to swallow the key from the page)
            let (event, swallow) = if ctrl || alt {
                if KB_CAPTURING.load(Ordering::Relaxed) {
                    // Settings is recording a new combo: suppress the bound action and the browser's
                    // default accelerator, but let the keydown reach the page's capture listener.
                    let _ = args.SetHandled(true);
                    return Ok(());
                }
                // All Ctrl/Alt combos go through the rebindable map (defaults + user overrides).
                (
                    lookup_shortcut(ctrl, shift, alt, vk).and_then(|a| shortcut_event(&a)),
                    true,
                )
            } else {
                // No Ctrl/Alt. Esc stops loading (page still gets the key). F5 reloads and
                // Shift+F5 hard-reloads (both swallowed so WebView2's own accelerator doesn't
                // fire a second reload).
                match vk {
                    0x1B => (Some(UserEvent::Stop), false),
                    0x74 if shift => (Some(UserEvent::HardReload), true),
                    0x74 => (Some(UserEvent::Reload), true),
                    _ => (None, false),
                }
            };
            if let Some(event) = event {
                let _ = proxy.send_event(event);
                if swallow {
                    let _ = args.SetHandled(true);
                }
            }
        }
        Ok(())
    }));
    unsafe {
        let mut token = 0i64;
        let _ = controller.add_AcceleratorKeyPressed(&handler, &mut token);
    }
}

fn push_tabs(chrome: &WebView, tabs: &[Tab], active: u32) {
    let arr: Vec<serde_json::Value> = tabs
        .iter()
        .map(|t| serde_json::json!({ "id": t.id, "title": t.title, "url": t.url, "active": t.id == active, "pinned": t.pinned, "favicon": t.favicon, "audio": t.audio, "muted": t.muted, "loaded": t.webview.is_some(), "suspended": t.suspended, "discarded": t.discarded, "napping": t.suspended || t.discarded, "group": t.group, "workspace": t.workspace, "private": t.private }))
        .collect();
    if let Ok(js) = serde_json::to_string(&serde_json::Value::Array(arr)) {
        let _ = chrome.evaluate_script(&format!("window.__chrome&&window.__chrome.setTabs({js})"));
    }
}

/// Push the profile registry to the chrome (Settings > Profiles + the startup picker).
fn push_profiles(chrome: &WebView, profiles: &store::ProfilesFile, current: &str) {
    let mut names = vec!["Default".to_string()];
    names.extend(profiles.list.iter().cloned());
    let ask = profiles.ask_at_startup;
    if let (Ok(names_js), Ok(cur_js)) = (
        serde_json::to_string(&names),
        serde_json::to_string(current),
    ) {
        let _ = chrome.evaluate_script(&format!(
            "window.__chrome&&window.__chrome.setProfiles({names_js},{cur_js},{ask})"
        ));
    }
}

/// Persist the workspace list + the currently shown workspace to workspaces.json.
fn persist_workspaces(workspaces: &[Workspace], active: u32) {
    store::save_workspaces(&store::WorkspaceFile {
        list: workspaces
            .iter()
            .map(|w| store::SessionWorkspace {
                id: w.id,
                name: w.name.clone(),
                color: w.color.clone(),
            })
            .collect(),
        active,
    });
}

/// Push the workspace list + active workspace to the chrome (the switcher next to the strip).
fn push_workspaces(chrome: &WebView, workspaces: &[Workspace], active: u32) {
    let arr: Vec<serde_json::Value> = workspaces
        .iter()
        .map(|w| serde_json::json!({ "id": w.id, "name": w.name, "color": w.color }))
        .collect();
    if let Ok(js) = serde_json::to_string(&serde_json::Value::Array(arr)) {
        let _ = chrome.evaluate_script(&format!(
            "window.__chrome&&window.__chrome.setWorkspaces({js},{active})"
        ));
    }
}

/// Push the tab groups to the chrome UI (colors/names/collapsed state for the strip chips).
fn push_groups(chrome: &WebView, groups: &[TabGroup]) {
    let arr: Vec<serde_json::Value> = groups
        .iter()
        .map(|g| serde_json::json!({ "id": g.id, "name": g.name, "color": g.color, "collapsed": g.collapsed }))
        .collect();
    if let Ok(js) = serde_json::to_string(&serde_json::Value::Array(arr)) {
        let _ =
            chrome.evaluate_script(&format!("window.__chrome&&window.__chrome.setGroups({js})"));
    }
}

/// Drop any group that no longer has member tabs. Returns true if anything was removed.
fn prune_empty_groups(groups: &mut Vec<TabGroup>, tabs: &[Tab]) -> bool {
    let before = groups.len();
    let live: HashSet<u32> = tabs.iter().filter_map(|t| t.group).collect();
    groups.retain(|g| live.contains(&g.id));
    groups.len() != before
}

/// Persist the tab groups to groups.json.
fn save_groups(groups: &[TabGroup]) {
    let saved: Vec<store::SessionGroup> = groups
        .iter()
        .map(|g| store::SessionGroup {
            id: g.id,
            name: g.name.clone(),
            color: g.color.clone(),
            collapsed: g.collapsed,
        })
        .collect();
    store::save_groups(&saved);
}

/// Reorder `tabs` so every group's members are contiguous, anchored at the group's first appearance
/// (Chrome keeps grouped tabs together). Stable for ungrouped tabs and within-group order.
fn normalize_groups(tabs: &mut Vec<Tab>) {
    let mut emitted: HashSet<u32> = HashSet::new();
    let mut order: Vec<u32> = Vec::with_capacity(tabs.len());
    for t in tabs.iter() {
        match t.group {
            None => order.push(t.id),
            Some(g) => {
                if emitted.insert(g) {
                    for m in tabs.iter() {
                        if m.group == Some(g) {
                            order.push(m.id);
                        }
                    }
                }
            }
        }
    }
    let mut reordered: Vec<Tab> = Vec::with_capacity(tabs.len());
    for id in &order {
        if let Some(pos) = tabs.iter().position(|t| t.id == *id) {
            reordered.push(tabs.remove(pos));
        }
    }
    reordered.append(tabs);
    *tabs = reordered;
}

fn push_url(chrome: &WebView, url: &str) {
    if let Ok(js) = serde_json::to_string(&omnibox_display(url)) {
        let _ = chrome.evaluate_script(&format!("window.__chrome&&window.__chrome.setUrl({js})"));
    }
}

/// Query the active tab's history and enable/disable the back/forward buttons accordingly. Without
/// this the buttons stay `disabled` from page load and clicks do nothing.
fn push_can_go(chrome: &WebView, tabs: &[Tab], active: u32) {
    let (back, fwd) = tabs
        .iter()
        .find(|t| t.id == active)
        .and_then(|t| t.webview.as_ref())
        .map(|wv| {
            let core = wv.webview();
            let mut b = windows_core::BOOL(0);
            let mut f = windows_core::BOOL(0);
            unsafe {
                let _ = core.CanGoBack(&mut b);
                let _ = core.CanGoForward(&mut f);
            }
            (b.as_bool(), f.as_bool())
        })
        .unwrap_or((false, false));
    let _ = chrome.evaluate_script(&format!(
        "window.__chrome&&window.__chrome.setCanGo({back},{fwd})"
    ));
}

/// Push the bookmarks list to the chrome UI's bookmarks bar.
/// Tell the chrome whether the active tab's page is bookmarked (gold star).
fn push_star(chrome: &WebView, tabs: &[Tab], active: u32, bookmarks: &[store::Bookmark]) {
    let on = tabs
        .iter()
        .find(|t| t.id == active)
        .map(|t| bookmarks.iter().any(|b| b.url == t.url))
        .unwrap_or(false);
    let _ = chrome.evaluate_script(&format!("window.__chrome&&window.__chrome.setStarred({on})"));
}

/// Push the active tab's omnibox URL and its bookmarked (star) state together.
fn push_url_star(chrome: &WebView, tabs: &[Tab], active: u32, bookmarks: &[store::Bookmark]) {
    push_url(chrome, &active_url(tabs, active));
    push_star(chrome, tabs, active, bookmarks);
}

fn push_reading(chrome: &WebView, reading: &[store::ReadingItem]) {
    let arr: Vec<serde_json::Value> = reading
        .iter()
        .map(|r| serde_json::json!({ "title": r.title, "url": r.url }))
        .collect();
    if let Ok(js) = serde_json::to_string(&arr) {
        let _ = chrome.evaluate_script(&format!(
            "window.__chrome&&window.__chrome.setReading({js})"
        ));
    }
}

fn push_bookmark_folders(chrome: &WebView, folders: &[String]) {
    if let Ok(js) = serde_json::to_string(folders) {
        let _ = chrome.evaluate_script(&format!(
            "window.__chrome&&window.__chrome.setBookmarkFolders({js})"
        ));
    }
}

fn push_bookmarks(chrome: &WebView, bookmarks: &[store::Bookmark]) {
    let arr: Vec<serde_json::Value> = bookmarks
        .iter()
        .map(|b| serde_json::json!({ "title": b.title, "url": b.url, "folder": b.folder }))
        .collect();
    if let Ok(js) = serde_json::to_string(&serde_json::Value::Array(arr)) {
        let _ = chrome.evaluate_script(&format!(
            "window.__chrome&&window.__chrome.setBookmarks({js})"
        ));
    }
}

/// Push the browsing history (most-recent first) to the chrome UI's history viewer.
fn push_history(chrome: &WebView, history: &[store::HistoryEntry]) {
    let arr: Vec<serde_json::Value> = history
        .iter()
        .map(|h| serde_json::json!({ "url": h.url, "title": h.title }))
        .collect();
    if let Ok(js) = serde_json::to_string(&serde_json::Value::Array(arr)) {
        let _ = chrome.evaluate_script(&format!(
            "window.__chrome&&window.__chrome.showHistory({js})"
        ));
    }
}

/// Push the auto-archived tabs to the chrome (shown under the History viewer's Archived filter).
fn push_archive(chrome: &WebView, archive: &[store::ArchivedTab]) {
    let arr: Vec<serde_json::Value> = archive
        .iter()
        .map(|a| serde_json::json!({ "url": a.url, "title": a.title, "ts": a.ts }))
        .collect();
    if let Ok(js) = serde_json::to_string(&serde_json::Value::Array(arr)) {
        let _ = chrome.evaluate_script(&format!(
            "window.__chrome&&window.__chrome.setArchive({js})"
        ));
    }
}

/// Push recent history to the omnibox suggestion model without opening the History overlay.
fn push_history_source(chrome: &WebView, history: &[store::HistoryEntry]) {
    let arr: Vec<serde_json::Value> = history
        .iter()
        .take(200)
        .map(|h| serde_json::json!({ "url": h.url, "title": h.title }))
        .collect();
    if let Ok(js) = serde_json::to_string(&serde_json::Value::Array(arr)) {
        let _ = chrome.evaluate_script(&format!(
            "window.__chrome&&window.__chrome.setHistorySource({js})"
        ));
    }
}

/// Push the active tab's zoom percent to the chrome UI (the omnibox shows a chip when it's not 100%).
fn push_zoom(chrome: &WebView, tabs: &[Tab], active: u32) {
    let pct = tabs
        .iter()
        .find(|t| t.id == active)
        .and_then(|t| t.webview.as_ref())
        .map(|wv| {
            let mut z = 1.0f64;
            unsafe {
                let _ = wv.controller().ZoomFactor(&mut z);
            }
            (z * 100.0).round() as i32
        })
        .unwrap_or(100);
    let _ = chrome.evaluate_script(&format!("window.__chrome&&window.__chrome.setZoom({pct})"));
}

/// Push the downloads list to the chrome UI's downloads popover.
fn push_downloads(chrome: &WebView, downloads: &[DownloadItem]) {
    let arr: Vec<serde_json::Value> = downloads
        .iter()
        .map(|d| serde_json::json!({ "name": d.name, "path": d.path, "state": d.state }))
        .collect();
    if let Ok(js) = serde_json::to_string(&serde_json::Value::Array(arr)) {
        let _ = chrome.evaluate_script(&format!(
            "window.__chrome&&window.__chrome.setDownloads({js})"
        ));
    }
}

/// Persist the open tabs + active index so the session is restored on next launch.
/// Tabs that should never be restored on relaunch: extension pages (the Bitwarden popup - can't be
/// recreated at build time, would crash the restore) and ephemeral data: pages (e.g. the in-app
/// extension help page). Both are transient by nature.
fn is_ephemeral_tab(url: &str) -> bool {
    url.starts_with("chrome-extension://") || url.starts_with("data:")
}

fn session_from_tabs(tabs: &[Tab], active: u32) -> store::Session {
    // Private/temporary tabs are intentionally never persisted (incognito).
    let saved: Vec<store::SessionTab> = tabs
        .iter()
        .filter(|t| !is_ephemeral_tab(&t.url) && !t.private)
        .map(|t| store::SessionTab {
            url: t.url.clone(),
            pinned: t.pinned,
            group: t.group,
            workspace: t.workspace,
            last_used: t.last_used,
        })
        .collect();
    let active_idx = tabs
        .iter()
        .filter(|t| !is_ephemeral_tab(&t.url) && !t.private)
        .position(|t| t.id == active)
        .unwrap_or(0);
    store::Session {
        tabs: saved,
        active: active_idx,
    }
}

fn persist_session(tabs: &[Tab], active: u32) {
    store::save_session(&session_from_tabs(tabs, active));
}

fn save_closed_window_session(tabs: &[Tab], active: u32) {
    let session = session_from_tabs(tabs, active);
    if !session.tabs.is_empty() {
        store::save_closed_session(&session);
    }
}

fn push_restore_prompt(chrome: &WebView) {
    let count = store::load_closed_session().tabs.len();
    let show = count > 0;
    let _ = chrome.evaluate_script(&format!(
        "window.__chrome&&window.__chrome.showRestore({show},{count})"
    ));
}

/// Write the bundled new-tab page into the data dir and return its file:// URL (used as the start
/// page and for new tabs). Rewritten each launch so updates to the page ship with the binary.
fn setup_home_page() -> String {
    let path = store::data_dir().join("home.html");
    let _ = std::fs::write(&path, include_str!("ui/home.html"));
    format!("file:///{}", path.to_string_lossy().replace('\\', "/"))
}

/// What the omnibox should display for a page URL: empty on the new-tab page (a clean search bar, not
/// a file:// path), the query on a DuckDuckGo search (instead of the long URL), otherwise the URL.
fn omnibox_display(url: &str) -> String {
    if url.contains("home.html") {
        return String::new();
    }
    if url.contains("duckduckgo.com") {
        if let Some(rest) = url.split("?q=").nth(1) {
            let q = rest.split('&').next().unwrap_or(rest);
            return q.replace('+', " ").replace("%20", " ");
        }
    }
    url.to_string()
}

fn tab_label(url: &str) -> String {
    let s = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let host = s.split('/').next().unwrap_or(s);
    let host = host.strip_prefix("www.").unwrap_or(host);
    if host.is_empty() {
        "New Tab".to_string()
    } else {
        host.to_string()
    }
}

/// Resolve an omnibox entry to a URL: a leading keyword that matches a configured search engine
/// ("w cat" -> Wikipedia) routes there; otherwise fall back to normal URL/domain/default-search logic.
fn resolve_query(input: &str, settings: &store::Settings) -> String {
    let s = input.trim();
    if let Some((first, rest)) = s.split_once(' ') {
        let rest = rest.trim();
        if !rest.is_empty() {
            if let Some(eng) = settings
                .search_engines
                .iter()
                .find(|e| !e.keyword.is_empty() && e.keyword.eq_ignore_ascii_case(first))
            {
                return eng.url.replace("{q}", &urlencode(rest));
            }
        }
    }
    normalize_url(s, &settings.search_url)
}

/// Whether a URL is safe to navigate to or persist from an untrusted source (AI output, an imported
/// backup file). Only web and about: schemes pass; this blocks javascript:/data:/file:/blob:/vbscript:
/// which could run script in our context or read local files. Used on AI-driven navigation and import,
/// NOT on user-typed input (normalize_url already coerces that to https).
fn url_is_safe(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("about:")
}

fn normalize_url(input: &str, search_url: &str) -> String {
    let s = input.trim();
    if s.starts_with("http://") || s.starts_with("https://") || s.starts_with("about:") {
        s.to_string()
    } else if s.contains('.') && !s.contains(' ') {
        format!("https://{s}")
    } else {
        search_url.replace("{q}", &urlencode(s))
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_url_keeps_urls_and_searches_queries() {
        let search = "https://duckduckgo.com/?q={q}";
        assert_eq!(
            normalize_url("https://example.com", search),
            "https://example.com"
        );
        assert_eq!(normalize_url("example.com", search), "https://example.com");
        assert_eq!(
            normalize_url("hello world", search),
            "https://duckduckgo.com/?q=hello+world"
        );
    }

    #[test]
    fn completion_prefers_recent_history_then_bookmarks() {
        let history = vec![
            store::HistoryEntry {
                url: "https://mail.google.com/".into(),
                title: "Gmail".into(),
            },
            store::HistoryEntry {
                url: "https://maps.google.com/".into(),
                title: "Maps".into(),
            },
        ];
        let bookmarks = vec![store::Bookmark {
            title: "GitHub".into(),
            url: "https://github.com".into(),
        }];
        assert_eq!(
            best_completion(&history, &bookmarks, "ma"),
            Some("mail.google.com".into())
        );
        assert_eq!(
            best_completion(&history, &bookmarks, "git"),
            Some("github.com".into())
        );
        assert_eq!(best_completion(&history, &bookmarks, "hello world"), None);
    }
}

/// Our held single-instance mutex handle (0 = none). Released explicitly on a profile switch so
/// the successor process can start; otherwise the OS frees it at exit.
static INSTANCE_MUTEX: AtomicIsize = AtomicIsize::new(0);

/// True if another copy of the app already holds this PROFILE's named mutex (each profile gets
/// its own, so different profiles can run side by side). The created handle is stashed in
/// INSTANCE_MUTEX for release on profile switch. Fails open (returns false) if the mutex can't
/// be created, so the app still runs.
fn another_instance_running(profile: &str) -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;
    let name: Vec<u16> = format!(
        "Local\\Aperture_SingleInstance_{}",
        store::slug(profile)
    )
    .encode_utf16()
    .chain(std::iter::once(0))
    .collect();
    unsafe {
        match CreateMutexW(None, true, PCWSTR(name.as_ptr())) {
            Ok(handle) => {
                let exists = GetLastError() == ERROR_ALREADY_EXISTS;
                if !exists {
                    INSTANCE_MUTEX.store(handle.0 as isize, Ordering::SeqCst);
                }
                exists
            }
            Err(_) => false,
        }
    }
}

/// Release the single-instance mutex so a successor process (profile switch) can take over.
fn release_instance_mutex() {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    let h = INSTANCE_MUTEX.swap(0, Ordering::SeqCst);
    if h != 0 {
        unsafe {
            let _ = CloseHandle(HANDLE(h as *mut std::ffi::c_void));
        }
    }
}

/// Best-effort: raise the already-running window so a second launch feels like a focus, not a no-op.
fn focus_existing_window() {
    use windows::core::PCWSTR;
    use windows::Win32::UI::WindowsAndMessaging::{
        FindWindowW, IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE,
    };
    let title: Vec<u16> = APP_TITLE.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        if let Ok(hwnd) = FindWindowW(PCWSTR::null(), PCWSTR(title.as_ptr())) {
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
            let _ = SetForegroundWindow(hwnd);
        }
    }
}

/// Pipe a later launch uses to hand its command-line URL to the running instance.
const OPEN_URL_PIPE: &str = r"\\.\pipe\aperture_open_url";

/// " [<profile>]" window-title suffix for non-default profiles (empty for Default).
static PROFILE_SUFFIX: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Split the command line into (--profile value, first non-flag argument).
fn parse_args() -> (Option<String>, Option<String>) {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut profile = None;
    let mut url = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--profile" && i + 1 < args.len() {
            profile = Some(args[i + 1].clone());
            i += 2;
        } else {
            if url.is_none() && !args[i].starts_with("--") {
                url = Some(args[i].clone());
            }
            i += 1;
        }
    }
    (profile, url)
}

/// A command-line argument as an openable URL: http(s) passes through, an existing local
/// .html/.htm file becomes a file:// URL, anything else is ignored.
fn cli_url_from(arg: Option<String>) -> Option<String> {
    let a = arg?;
    if a.starts_with("http://") || a.starts_with("https://") {
        return Some(a);
    }
    let p = std::path::Path::new(&a);
    let ext_ok = p
        .extension()
        .map(|e| e.eq_ignore_ascii_case("html") || e.eq_ignore_ascii_case("htm"))
        .unwrap_or(false);
    if ext_ok && p.is_file() {
        return Some(format!("file:///{}", p.to_string_lossy().replace('\\', "/")));
    }
    None
}

/// Second-launch side of the open-url pipe: write the URL and exit. The server end is opened by
/// the running instance in spawn_open_url_pipe. Best effort; a miss just means a plain focus.
fn send_url_to_running_instance(url: &str) {
    use std::io::Write;
    // The named-pipe CLIENT side works through std: opening \\.\pipe\<name> for write connects.
    for _ in 0..10 {
        match std::fs::OpenOptions::new().write(true).open(OPEN_URL_PIPE) {
            Ok(mut f) => {
                let _ = f.write_all(url.as_bytes());
                return;
            }
            // Pipe busy (another sender connected): give the server a moment to cycle.
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
}

/// Host side of the open-url pipe: a worker thread accepts one short connection at a time and
/// posts each received http(s) URL into the run loop as OpenExternal.
fn spawn_open_url_pipe(proxy: EventLoopProxy<UserEvent>) {
    use windows::core::w;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Storage::FileSystem::{ReadFile, PIPE_ACCESS_INBOUND};
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
        PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    std::thread::spawn(move || unsafe {
        loop {
            let handle = CreateNamedPipeW(
                w!(r"\\.\pipe\aperture_open_url"),
                PIPE_ACCESS_INBOUND,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                0,
                4096,
                0,
                None,
            );
            if handle.is_invalid() {
                return;
            }
            if ConnectNamedPipe(handle, None).is_ok() {
                let mut buf = [0u8; 4096];
                let mut read: u32 = 0;
                if ReadFile(handle, Some(&mut buf), Some(&mut read), None).is_ok() && read > 0 {
                    if let Ok(s) = std::str::from_utf8(&buf[..read as usize]) {
                        let s = s.trim();
                        if s.starts_with("http://")
                            || s.starts_with("https://")
                            || s.starts_with("file:///")
                        {
                            let _ = proxy.send_event(UserEvent::OpenExternal(s.to_string()));
                        }
                    }
                }
            }
            let _ = DisconnectNamedPipe(handle);
            let _ = CloseHandle(handle);
        }
    });
}

/// Register Aperture as a browser candidate under HKCU (no admin needed) so it shows up in
/// Windows Settings > Default apps. Windows does not let an app make ITSELF default; the caller
/// opens the Settings page for the user to confirm. Returns false if any registry write failed.
fn register_as_default_browser() -> bool {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let exe = exe.to_string_lossy().to_string();
    let open_cmd = format!("\"{exe}\" \"%1\"");
    let icon = format!("\"{exe}\",0");
    let base = r"HKCU\Software\Clients\StartMenuInternet\Aperture";
    let sets: Vec<(String, Option<&str>, String)> = vec![
        (r"HKCU\Software\Classes\Aperture.HTML".into(), None, "Aperture HTML Document".into()),
        (r"HKCU\Software\Classes\Aperture.HTML\DefaultIcon".into(), None, icon.clone()),
        (r"HKCU\Software\Classes\Aperture.HTML\shell\open\command".into(), None, open_cmd),
        (base.into(), None, "Aperture".into()),
        (format!(r"{base}\DefaultIcon"), None, icon),
        (format!(r"{base}\shell\open\command"), None, format!("\"{exe}\"")),
        (format!(r"{base}\Capabilities"), Some("ApplicationName"), "Aperture".into()),
        (
            format!(r"{base}\Capabilities"),
            Some("ApplicationDescription"),
            "A lightweight, private daily-driver browser.".into(),
        ),
        (format!(r"{base}\Capabilities\URLAssociations"), Some("http"), "Aperture.HTML".into()),
        (format!(r"{base}\Capabilities\URLAssociations"), Some("https"), "Aperture.HTML".into()),
        (format!(r"{base}\Capabilities\FileAssociations"), Some(".html"), "Aperture.HTML".into()),
        (format!(r"{base}\Capabilities\FileAssociations"), Some(".htm"), "Aperture.HTML".into()),
        (
            r"HKCU\Software\RegisteredApplications".into(),
            Some("Aperture"),
            r"Software\Clients\StartMenuInternet\Aperture\Capabilities".into(),
        ),
    ];
    let mut ok = true;
    for (key, value_name, data) in sets {
        let mut c = std::process::Command::new("reg");
        c.arg("add").arg(&key);
        match value_name {
            Some(v) => {
                c.arg("/v").arg(v);
            }
            None => {
                c.arg("/ve");
            }
        }
        c.arg("/t").arg("REG_SZ").arg("/d").arg(&data).arg("/f");
        c.creation_flags(CREATE_NO_WINDOW);
        ok &= c.status().map(|s| s.success()).unwrap_or(false);
    }
    ok
}

/// WebView2's loader normally finds the Evergreen runtime via the EdgeUpdate registry. On this
/// machine the runtime FILES exist but that registration is missing, so default resolution fails
/// with 0x80070002. Detect the runtime folder ourselves and point WebView2 at it via the documented
/// WEBVIEW2_BROWSER_EXECUTABLE_FOLDER override (re-detected each launch). No-op if already set.
fn ensure_webview2_runtime() {
    if std::env::var_os("WEBVIEW2_BROWSER_EXECUTABLE_FOLDER").is_some() {
        return;
    }
    // Resolve Program Files via env vars (don't assume C: or the default localized path).
    let pf = std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".to_string());
    let pfx86 = std::env::var("ProgramFiles(x86)")
        .unwrap_or_else(|_| r"C:\Program Files (x86)".to_string());
    let bases = [
        format!(r"{pfx86}\Microsoft\EdgeWebView\Application"),
        format!(r"{pf}\Microsoft\EdgeWebView\Application"),
    ];
    let mut best: Option<(Vec<u64>, std::path::PathBuf)> = None;
    for base in &bases {
        let Ok(entries) = std::fs::read_dir(base) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if !p.join("msedgewebview2.exe").exists() {
                continue;
            }
            let parts: Vec<u64> = e
                .file_name()
                .to_string_lossy()
                .split('.')
                .filter_map(|s| s.parse().ok())
                .collect();
            if parts.is_empty() {
                continue;
            }
            if best.as_ref().is_none_or(|(bv, _)| parts > *bv) {
                best = Some((parts, p));
            }
        }
    }
    if let Some((_, path)) = best {
        std::env::set_var("WEBVIEW2_BROWSER_EXECUTABLE_FOLDER", path);
    }
}

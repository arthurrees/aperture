//! Ad/tracker blocking via Brave's `adblock` engine, wired into each tab's WebView2 through the
//! WebResourceRequested COM event (wry exposes no safe hook for subresource requests). For every
//! subresource the handler asks the shared engine whether to block; a match yields a synthetic 403
//! so the request never hits the network.

use std::cell::RefCell;
use std::rc::Rc;

use adblock::lists::ParseOptions;
use adblock::request::Request;
use adblock::Engine;
use webview2_com::{
    Microsoft::Web::WebView2::Win32::{
        ICoreWebView2_22, COREWEBVIEW2_WEB_RESOURCE_CONTEXT, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FETCH, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FONT,
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_IMAGE, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_MEDIA,
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_PING, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_SCRIPT,
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_STYLESHEET, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_WEBSOCKET,
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_XML_HTTP_REQUEST,
        COREWEBVIEW2_WEB_RESOURCE_REQUEST_SOURCE_KINDS_ALL,
    },
    WebResourceRequestedEventHandler,
};
use windows::Win32::System::Com::{CoTaskMemFree, IStream};
use windows_core::{Interface, PCWSTR, PWSTR};
use wry::{WebView, WebViewExtWindows};

/// Build the shared blocking engine from the bundled filter list.
pub fn build_engine() -> Engine {
    let list = include_str!("ui/filters.txt");
    Engine::from_rules(list.lines(), ParseOptions::default())
}

/// Map a WebView2 resource context to the request-type string adblock expects.
fn context_to_type(ctx: COREWEBVIEW2_WEB_RESOURCE_CONTEXT) -> &'static str {
    if ctx == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_SCRIPT {
        "script"
    } else if ctx == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_IMAGE {
        "image"
    } else if ctx == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_STYLESHEET {
        "stylesheet"
    } else if ctx == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FONT {
        "font"
    } else if ctx == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_MEDIA {
        "media"
    } else if ctx == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_XML_HTTP_REQUEST
        || ctx == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FETCH
    {
        "xmlhttprequest"
    } else if ctx == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_WEBSOCKET {
        "websocket"
    } else if ctx == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_PING {
        "ping"
    } else {
        "other"
    }
}

/// Register a WebResourceRequested handler on this tab's webview that blocks ad/tracker requests.
/// `page_url` is the tab's current top-level URL (used as the request source for correct
/// first-/third-party matching); it is updated by the run loop on navigation.
pub fn attach(webview: &WebView, engine: Rc<Engine>, page_url: Rc<RefCell<String>>) {
    let env = webview.environment();
    let core = webview.webview();
    let debug = std::env::var_os("BROWSER_DEBUG").is_some();

    let handler = WebResourceRequestedEventHandler::create(Box::new(move |_sender, args| {
        let Some(args) = args else {
            return Ok(());
        };

        // Read the request URL (Uri fills a heap PWSTR; take_pwstr converts + frees it).
        let url = unsafe {
            match args.Request() {
                Ok(req) => {
                    let mut p = PWSTR::null();
                    if req.Uri(&mut p).is_ok() {
                        let s = p.to_string().unwrap_or_default();
                        CoTaskMemFree(Some(p.0 as *const core::ffi::c_void));
                        s
                    } else {
                        return Ok(());
                    }
                }
                Err(_) => return Ok(()),
            }
        };
        if url.is_empty() {
            return Ok(());
        }

        let mut ctx = COREWEBVIEW2_WEB_RESOURCE_CONTEXT::default();
        unsafe {
            let _ = args.ResourceContext(&mut ctx);
        }

        let source = page_url.borrow().clone();
        if let Ok(req) = Request::new(&url, &source, context_to_type(ctx)) {
            let result = engine.check_network_request(&req);
            if result.matched && result.exception.is_none() {
                // Block: hand back a synthetic empty 403 so the request never goes out.
                unsafe {
                    let reason: Vec<u16> = "Blocked\u{0}".encode_utf16().collect();
                    let headers: Vec<u16> = vec![0u16];
                    if let Ok(resp) = env.CreateWebResourceResponse(
                        None::<&IStream>,
                        403,
                        PCWSTR(reason.as_ptr()),
                        PCWSTR(headers.as_ptr()),
                    ) {
                        let _ = args.SetResponse(&resp);
                    }
                }
                if debug {
                    eprintln!("[adblock] blocked {url}");
                }
            }
        }
        Ok(())
    }));

    unsafe {
        let mut token = 0i64;
        let _ = core.add_WebResourceRequested(&handler, &mut token);

        // Filter for ALL resource types. Prefer the _22 variant so service-worker / cross-origin
        // iframe requests are also seen; fall back to the base filter on older runtimes.
        let star: Vec<u16> = vec![b'*' as u16, 0u16];
        let pattern = PCWSTR(star.as_ptr());
        if let Ok(core22) = core.cast::<ICoreWebView2_22>() {
            let _ = core22.AddWebResourceRequestedFilterWithRequestSourceKinds(
                pattern,
                COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
                COREWEBVIEW2_WEB_RESOURCE_REQUEST_SOURCE_KINDS_ALL,
            );
        } else {
            let _ =
                core.AddWebResourceRequestedFilter(pattern, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL);
        }
    }
}

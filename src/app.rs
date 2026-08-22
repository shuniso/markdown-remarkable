//! The native window: a `tao` event loop hosting a `wry` WebView that serves
//! [`crate::routes::handle`] through a custom protocol instead of an HTTP
//! server (`server.rs`/`--browser`).
//!
//! Not covered by automated tests — GUI startup/event-loop code isn't
//! practical to exercise the way `routes`, `server`, and `watch` are; see
//! the design doc's "含まない" section. The routing logic it calls into
//! (`routes::handle`) is fully unit-tested on its own. Set `MDVIEW_DEBUG=1`
//! to log every request the WebView makes through the custom protocol,
//! which is how the live-reload path gets verified by hand.

use crate::routes::{self, file_title};
use crate::watch;
use anyhow::{Context, Result};
use notify::RecommendedWatcher;
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::window::{Window, WindowBuilder};
use wry::http::header::{HeaderName, HeaderValue, CONTENT_TYPE};
use wry::http::{Request, Response, StatusCode};
use wry::{DragDropEvent, NewWindowResponse, WebView, WebViewBuilder};

#[cfg(target_os = "linux")]
use tao::platform::unix::WindowExtUnix;
#[cfg(target_os = "linux")]
use wry::WebViewBuilderExtUnix;

const WINDOW_WIDTH: f64 = 960.0;
const WINDOW_HEIGHT: f64 = 800.0;

/// Custom-protocol scheme name registered with the WebView.
const PROTOCOL: &str = "mdview";

/// The URL the WebView is pointed at on startup, in the form wry expects
/// for a registered custom protocol on each platform: macOS/Linux keep the
/// scheme as-is, while Windows (and Android, not targeted here) rewrite it
/// to `http(s)://<scheme>.localhost/`.
#[cfg(not(target_os = "windows"))]
const INITIAL_URL: &str = "mdview://localhost/";
#[cfg(target_os = "windows")]
const INITIAL_URL: &str = "http://mdview.localhost/";

/// Prefix every in-app navigation shares; anything else is "leaving the
/// document" and is handed to the OS instead (see `navigation_policy`).
#[cfg(not(target_os = "windows"))]
const INTERNAL_URL_PREFIX: &str = "mdview://";
#[cfg(target_os = "windows")]
const INTERNAL_URL_PREFIX: &str = "http://mdview.localhost";

/// Dispatched from the drag&drop handler (via `EventLoopProxy`, since it
/// doesn't run inside the `tao` event loop closure) to ask the event loop to
/// switch to a newly dropped file.
enum UserEvent {
    OpenFile(PathBuf),
}

/// Opens the native window. `initial` is the file to show right away, if
/// one was given on the command line; `None` opens a file-picker dialog
/// first (and, if that's cancelled, shows the empty-state page instead of
/// exiting).
///
/// This function's happy path never actually returns `Ok(())`: `tao`'s
/// `EventLoop::run` has return type `!` (it calls `std::process::exit`
/// internally once the loop stops), so the only way out of this function is
/// an `Err` from setup that happens *before* the event loop starts running.
pub fn run(initial: Option<PathBuf>) -> Result<()> {
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // Keep the menu alive for the whole run: on macOS it's what makes
    // Cmd+Q / Cmd+C / Cmd+W work at all (wry doesn't add accelerators).
    let _menu = install_menu()?;

    // The picker is shown only once the event loop (and so the NSApp /
    // GTK context) exists, so it comes up in front as a real app dialog.
    let initial_file = initial.or_else(pick_file_dialog);

    let file_state: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(initial_file.clone()));
    let version: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    let debug = std::env::var_os("MDVIEW_DEBUG").is_some();

    let window = WindowBuilder::new()
        .with_title(window_title(initial_file.as_deref()))
        .with_inner_size(LogicalSize::new(WINDOW_WIDTH, WINDOW_HEIGHT))
        .build(&event_loop)
        .context("failed to create window")?;

    let protocol_file_state = Arc::clone(&file_state);
    let protocol_version = Arc::clone(&version);
    let builder = WebViewBuilder::new()
        .with_custom_protocol(PROTOCOL.into(), move |_webview_id, request| {
            protocol_response(&protocol_file_state, &protocol_version, request, debug)
        })
        .with_navigation_handler(navigation_policy)
        .with_new_window_req_handler(|url, _features| {
            // Never spawn a second window; treat it like a plain link.
            navigation_policy(url);
            NewWindowResponse::Deny
        })
        .with_drag_drop_handler(move |event| handle_drag_drop(event, &proxy))
        .with_url(INITIAL_URL);

    #[cfg(not(target_os = "linux"))]
    let webview = builder.build(&window).context(
        "failed to create the WebView (try `mdview --browser` to use the browser version instead)",
    )?;
    #[cfg(target_os = "linux")]
    let webview = {
        let vbox = window
            .default_vbox()
            .context("failed to access the window's GTK container")?;
        builder.build_gtk(vbox).context(
            "failed to create the WebView — is webkit2gtk installed? \
             try `mdview --browser` to use the browser version instead",
        )?
    };

    let mut watcher: Option<RecommendedWatcher> = initial_file
        .as_deref()
        .and_then(|path| start_watch(path, &version));

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => *control_flow = ControlFlow::Exit,
            Event::UserEvent(UserEvent::OpenFile(path)) => {
                open_file(&file_state, &version, &mut watcher, &window, &webview, path);
            }
            _ => {}
        }
    });
}

/// Handles `UserEvent::OpenFile`: switches the served file, re-points the
/// watcher at it, bumps `version`, updates the window title, and reloads
/// the WebView. The reload (rather than relying on the live-reload poll
/// alone) is what makes a drop recover a view that's stuck on an error
/// page whose script has died.
fn open_file(
    file_state: &Arc<Mutex<Option<PathBuf>>>,
    version: &Arc<AtomicU64>,
    watcher: &mut Option<RecommendedWatcher>,
    window: &Window,
    webview: &WebView,
    path: PathBuf,
) {
    *file_state.lock().expect("file state mutex poisoned") = Some(path.clone());
    // Drop the old watcher *before* creating the new one so the two never
    // overlap (they'd double-count a save when both files share a directory).
    *watcher = None;
    *watcher = start_watch(&path, version);
    version.fetch_add(1, Ordering::SeqCst);
    window.set_title(&window_title(Some(&path)));
    if let Err(err) = webview.load_url(INITIAL_URL) {
        eprintln!("warning: failed to reload the view: {err}");
    }
}

/// Starts watching `path`, logging (and continuing without live-reload)
/// rather than failing the whole app if the watcher can't be created.
fn start_watch(path: &Path, version: &Arc<AtomicU64>) -> Option<RecommendedWatcher> {
    match watch::watch(path, Arc::clone(version)) {
        Ok(watcher) => Some(watcher),
        Err(err) => {
            eprintln!(
                "warning: live-reload disabled for {}: {err}",
                path.display()
            );
            None
        }
    }
}

/// The custom-protocol handler: looks up the currently-open file (if any)
/// and routes the request through `routes::handle`, same as the browser
/// server does for an HTTP request.
fn protocol_response(
    file_state: &Arc<Mutex<Option<PathBuf>>>,
    version: &Arc<AtomicU64>,
    request: Request<Vec<u8>>,
    debug: bool,
) -> Response<Cow<'static, [u8]>> {
    let path = request.uri().path().to_owned();
    let file = file_state.lock().expect("file state mutex poisoned");
    let reply = routes::handle(&path, file.as_deref(), version);
    drop(file);
    if debug {
        eprintln!("[mdview] GET {path} -> {}", reply.status);
    }

    let mut response = match Response::builder()
        .status(reply.status)
        .header(CONTENT_TYPE, reply.content_type)
        .body(Cow::Owned(reply.body))
    {
        Ok(response) => response,
        Err(err) => {
            eprintln!("warning: failed to build response for {path}: {err}");
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Cow::Borrowed(b"500 Internal Server Error" as &[u8]))
                .expect("static fallback response is valid");
        }
    };
    // Extra headers are added one at a time so a single unrepresentable
    // value drops just that header instead of the whole response.
    for (name, value) in &reply.headers {
        match (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            (Ok(name), Ok(value)) => {
                response.headers_mut().insert(name, value);
            }
            _ => eprintln!("warning: dropping response header {name}: not representable"),
        }
    }
    response
}

/// Decides whether the WebView may navigate to `url`. Only the app's own
/// protocol stays inside the window; web links open in the default browser
/// (the window has no back button or URL bar to get home from), and
/// anything else is simply ignored.
fn navigation_policy(url: String) -> bool {
    if url.starts_with(INTERNAL_URL_PREFIX) {
        return true;
    }
    if url.starts_with("http://") || url.starts_with("https://") || url.starts_with("mailto:") {
        if let Err(err) = open::that(&url) {
            eprintln!("warning: could not open {url}: {err}");
        }
    }
    false
}

/// Only files dropped with a `.md`/`.markdown` extension (case-insensitive)
/// are accepted; anything else is left to whatever the OS/WebView would
/// otherwise do with a drop (returning `false` here means "don't block the
/// default behavior").
fn handle_drag_drop(event: DragDropEvent, proxy: &EventLoopProxy<UserEvent>) -> bool {
    let DragDropEvent::Drop { paths, .. } = event else {
        return false;
    };
    match paths.into_iter().find(|path| is_markdown_file(path)) {
        Some(path) => {
            // If the event loop has already shut down, there's nothing
            // useful to do with the error — the process is exiting anyway.
            let _ = proxy.send_event(UserEvent::OpenFile(path));
            true
        }
        None => false,
    }
}

fn is_markdown_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown"))
}

fn pick_file_dialog() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .add_filter("Markdown", &["md", "markdown"])
        .pick_file()
}

/// The native window's title bar text: `<file name> — mdview`, or just
/// `mdview` before any file has been opened.
fn window_title(file: Option<&Path>) -> String {
    match file {
        Some(path) => format!("{} — mdview", file_title(path)),
        None => "mdview".to_string(),
    }
}

/// macOS: a minimal application menu. Without one, none of the standard
/// shortcuts (Cmd+Q, Cmd+W, Cmd+C, Cmd+A) work in the window.
#[cfg(target_os = "macos")]
fn install_menu() -> Result<muda::Menu> {
    use muda::{Menu, PredefinedMenuItem, Submenu};

    let menu = Menu::new();
    let app = Submenu::with_items("mdview", true, &[&PredefinedMenuItem::quit(None)])?;
    let edit = Submenu::with_items(
        "Edit",
        true,
        &[
            &PredefinedMenuItem::copy(None),
            &PredefinedMenuItem::select_all(None),
        ],
    )?;
    let window = Submenu::with_items("Window", true, &[&PredefinedMenuItem::close_window(None)])?;
    menu.append(&app)?;
    menu.append(&edit)?;
    menu.append(&window)?;
    menu.init_for_nsapp();
    Ok(menu)
}

/// Other platforms get no menu bar (window chrome provides close/quit).
#[cfg(not(target_os = "macos"))]
fn install_menu() -> Result<()> {
    Ok(())
}

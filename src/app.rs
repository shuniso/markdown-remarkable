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

use crate::routes;
use crate::util::file_title;
use crate::watch;
use crate::window_state::{self, MonitorRect, WindowState};
use anyhow::{Context, Result};
use notify::RecommendedWatcher;
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tao::dpi::{LogicalPosition, LogicalSize};
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy};
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

/// How long to wait after startup for the OS to hand us a file (a Finder
/// double-click arrives as `Event::Opened` slightly after launch) before
/// concluding nothing is coming and showing the file picker instead.
const OPEN_EVENT_GRACE: Duration = Duration::from_millis(400);

/// How long to wait after the last `Moved`/`Resized` event before writing
/// the window's geometry to disk — a drag or a resize-by-dragging-the-edge
/// delivers a burst of these, and only the settled result is worth a write.
const WINDOW_STATE_SAVE_DEBOUNCE: Duration = Duration::from_millis(500);

/// Which direction a `View ▸ Zoom In/Out/Actual Size` menu item (or its
/// accelerator) moves the zoom level. Forwarded to the WebView as
/// `window.__mdviewViewer.zoom("in" | "out" | "reset")` — see
/// `assets/viewer.js` and [`UserEvent::Zoom`].
///
/// Every variant is only ever *constructed* inside the macOS-only
/// `install_menu` (other platforms have no menu bar, hence no zoom menu
/// item to construct one from); `as_js_arg`'s match on `self` doesn't count
/// as a construction site, so each variant still needs its own dead_code
/// allowance on non-macOS builds — same as [`UserEvent::PickFile`].
enum ZoomDir {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    In,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Out,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Reset,
}

impl ZoomDir {
    fn as_js_arg(&self) -> &'static str {
        match self {
            ZoomDir::In => "in",
            ZoomDir::Out => "out",
            ZoomDir::Reset => "reset",
        }
    }
}

/// Events posted to the loop from outside it: the drag&drop handler, the
/// menu handler, and (on macOS) the OS asking us to open a document.
enum UserEvent {
    OpenFile(PathBuf),
    /// Only the macOS menu (File ▸ Open…) posts this today; other
    /// platforms have no menu bar, so there it's handled but never sent.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    PickFile,
    /// Only the macOS menu's View submenu posts this — see
    /// [`install_menu`]. Other platforms have no menu bar at all; their
    /// zoom shortcuts are handled entirely in `assets/viewer.js`, which
    /// never needs to round-trip through the event loop.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Zoom(ZoomDir),
    /// Only the macOS menu's View ▸ Reload posts this — see
    /// [`install_menu`]. `assets/review.js` handles ⌘R/Ctrl+R directly with
    /// `location.reload()` everywhere else (including in the native window
    /// on non-macOS, which has no menu to intercept the key first).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Reload,
}

/// Opens the native window. `initial` is the file to show right away, if
/// one was given on the command line. With `None` the window opens on the
/// empty-state page and, unless the OS delivers a file to open within
/// [`OPEN_EVENT_GRACE`] (Finder double-click), shows a file-picker dialog;
/// cancelling that just leaves the empty page (drop a file, or ⌘O).
///
/// This function's happy path never actually returns `Ok(())`: `tao`'s
/// `EventLoop::run` has return type `!` (it calls `std::process::exit`
/// internally once the loop stops), so the only way out of this function is
/// an `Err` from setup that happens *before* the event loop starts running.
pub fn run(initial: Option<PathBuf>) -> Result<()> {
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // Keep the menu alive for the whole run: on macOS it's what makes
    // Cmd+Q / Cmd+O / Cmd+C / Cmd+W work at all (wry doesn't add accelerators).
    let _menu = install_menu(&proxy)?;

    let file_state: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(initial.clone()));
    let version: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    let debug = std::env::var_os("MDVIEW_DEBUG").is_some();

    // Where (if anywhere) the window's last size/position lives, and
    // what's there, if anything readable — `None` either way just means
    // "use the built-in defaults" (see resolve_initial_geometry).
    let window_state_path = window_state::config_path();
    let saved_window_state = window_state_path.as_deref().and_then(window_state::load);
    let monitors = monitor_rects(&event_loop);
    let (initial_size, initial_position) = resolve_initial_geometry(saved_window_state, &monitors);

    // Position is deliberately *not* set via `WindowBuilder::with_position`
    // here: on macOS, tao's builder-time position sets the window's
    // content-rect origin, not its frame (title-bar-inclusive) origin —
    // but `save_window_state` reads `Window::outer_position` (the frame
    // origin) to save, and `Window::set_outer_position` below is its exact
    // symmetric counterpart. Mixing the two would shift the saved/restored
    // position by the title bar's height on every single launch. Setting
    // it after `build()` via `set_outer_position` keeps save and restore
    // on the same coordinate basis.
    let window = WindowBuilder::new()
        .with_title(window_title(initial.as_deref()))
        .with_min_inner_size(LogicalSize::new(
            window_state::MIN_WIDTH,
            window_state::MIN_HEIGHT,
        ))
        .with_inner_size(initial_size)
        .build(&event_loop)
        .context("failed to create window")?;
    if let Some(position) = initial_position {
        window.set_outer_position(position);
    }

    let protocol_file_state = Arc::clone(&file_state);
    let protocol_version = Arc::clone(&version);
    let drop_proxy = proxy.clone();
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
        .with_drag_drop_handler(move |event| handle_drag_drop(event, &drop_proxy))
        .with_ipc_handler(move |request: Request<String>| {
            // `assets/viewer.js`'s startup localStorage probe posts its
            // result here via `window.ipc.postMessage(...)` — the only
            // way to observe from outside the WebView whether persistence
            // actually works in this embedder (some sandboxes/WebView
            // configurations silently disable it). Anything else posted
            // to `window.ipc` in the future also just lands here, logged
            // the same way.
            if debug {
                eprintln!("[mdview:js] {}", request.body());
            }
        })
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

    let mut watcher: Option<RecommendedWatcher> = initial
        .as_deref()
        .and_then(|path| start_watch(path, &version));
    // While `Some`, we still owe the user a file picker once this deadline
    // passes (no file yet, and the OS hasn't handed us one). Cleared by the
    // first file that arrives by any route.
    let mut picker_deadline = initial.is_none().then(|| Instant::now() + OPEN_EVENT_GRACE);
    // While `Some`, the window has moved/resized since the last save and is
    // due to be written to disk once this deadline passes (see
    // WINDOW_STATE_SAVE_DEBOUNCE) — cleared once that save happens. Also
    // saved unconditionally, deadline or not, on `CloseRequested` and
    // `LoopDestroyed`.
    let mut save_deadline: Option<Instant> = None;
    // `set_outer_position` above (and the window's own creation) can
    // themselves generate `Moved`/`Resized` events during startup; a
    // `Moved`/`Resized` seen before this deadline never schedules a save,
    // so the very first thing written to disk after launch is never just
    // "whatever the OS/tao happened to settle on before our own restore
    // finished applying" racing against the value that was actually
    // restored.
    let startup_grace = Instant::now() + Duration::from_secs(1);

    event_loop.run(move |event, _, control_flow| {
        let mut exit = false;

        match event {
            Event::NewEvents(StartCause::ResumeTimeReached { .. })
                if picker_deadline.is_some_and(|deadline| Instant::now() >= deadline) =>
            {
                picker_deadline = None;
                if let Some(path) = pick_file_dialog() {
                    open_file(&file_state, &version, &mut watcher, &window, &webview, path);
                }
            }
            Event::NewEvents(StartCause::ResumeTimeReached { .. })
                if save_deadline.is_some_and(|deadline| Instant::now() >= deadline) =>
            {
                save_deadline = None;
                save_window_state(&window, window_state_path.as_deref());
            }
            Event::Opened { urls } => {
                // Finder double-click / "Open With" / `open -a`: the OS hands
                // us file URLs. Take the first Markdown one.
                let dropped = urls
                    .iter()
                    .filter_map(|url| url.to_file_path().ok())
                    .find(|path| is_markdown_file(path));
                if let Some(path) = dropped {
                    picker_deadline = None;
                    open_file(&file_state, &version, &mut watcher, &window, &webview, path);
                }
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                // Unconditional, bypassing the debounce: this is the last
                // chance to persist whatever the final geometry turned out
                // to be, and the process is about to exit either way.
                save_window_state(&window, window_state_path.as_deref());
                exit = true;
            }
            Event::WindowEvent {
                event: WindowEvent::Moved(_) | WindowEvent::Resized(_),
                ..
            } if Instant::now() >= startup_grace => {
                save_deadline = Some(Instant::now() + WINDOW_STATE_SAVE_DEBOUNCE);
            }
            Event::LoopDestroyed => {
                // Covers the paths that skip `CloseRequested` entirely —
                // notably ⌘Q / "Quit" from the Dock on macOS, which
                // terminates the application directly rather than closing
                // the window first. This is the last point before the
                // process actually exits.
                save_window_state(&window, window_state_path.as_deref());
            }
            Event::UserEvent(UserEvent::OpenFile(path)) => {
                picker_deadline = None;
                open_file(&file_state, &version, &mut watcher, &window, &webview, path);
            }
            Event::UserEvent(UserEvent::PickFile) => {
                picker_deadline = None;
                if let Some(path) = pick_file_dialog() {
                    open_file(&file_state, &version, &mut watcher, &window, &webview, path);
                }
            }
            Event::UserEvent(UserEvent::Zoom(dir)) => {
                let script = format!(
                    "window.__mdviewViewer && window.__mdviewViewer.zoom('{}')",
                    dir.as_js_arg()
                );
                if let Err(err) = webview.evaluate_script(&script) {
                    eprintln!("warning: failed to change zoom: {err}");
                }
            }
            Event::UserEvent(UserEvent::Reload) => {
                if let Err(err) = webview.load_url(INITIAL_URL) {
                    eprintln!("warning: failed to reload the view: {err}");
                }
            }
            _ => {}
        }

        // Decided once per event, at the end, so the pending timers survive
        // the other events tao delivers in the same iteration.
        *control_flow = if exit {
            ControlFlow::Exit
        } else {
            match (picker_deadline, save_deadline) {
                (Some(a), Some(b)) => ControlFlow::WaitUntil(a.min(b)),
                (Some(deadline), None) | (None, Some(deadline)) => ControlFlow::WaitUntil(deadline),
                (None, None) => ControlFlow::Wait,
            }
        };
    });
}

/// Every attached monitor's bounds, in logical pixels — used only to check
/// whether a saved window position (see [`resolve_initial_geometry`]) is
/// still somewhere visible; the monitor it was on might have been
/// unplugged, or the saved file might just be stale/hand-edited.
fn monitor_rects(event_loop: &EventLoop<UserEvent>) -> Vec<MonitorRect> {
    event_loop
        .available_monitors()
        .map(|monitor| {
            let scale = monitor.scale_factor();
            let position = monitor.position().to_logical::<f64>(scale);
            let size = monitor.size().to_logical::<f64>(scale);
            MonitorRect {
                x: position.x,
                y: position.y,
                width: size.width,
                height: size.height,
            }
        })
        .collect()
}

/// Turns a possibly-absent saved [`WindowState`] into the size (always
/// present — falls back to the built-in default) and position (only if
/// it's still within one of `monitors`; otherwise `None`, and the OS
/// chooses where the window lands, same as before window persistence
/// existed) to hand `WindowBuilder`.
fn resolve_initial_geometry(
    saved: Option<WindowState>,
    monitors: &[MonitorRect],
) -> (LogicalSize<f64>, Option<LogicalPosition<f64>>) {
    let Some(state) = saved else {
        return (LogicalSize::new(WINDOW_WIDTH, WINDOW_HEIGHT), None);
    };
    let (width, height) = window_state::clamp_size(state.width, state.height);
    let position = window_state::position_is_visible(state.x, state.y, monitors)
        .then(|| LogicalPosition::new(state.x, state.y));
    (LogicalSize::new(width, height), position)
}

/// Saves `window`'s current outer position and inner size to `path`
/// (logical pixels), if there's anywhere to save to at all — `path` is
/// `None` when [`window_state::config_path`] couldn't determine a config
/// directory (e.g. no `HOME` in the environment), which is a warning, not a
/// hard failure, same as every other window-state I/O problem. A failure
/// to *query* the position (some platforms/compositors don't support it)
/// falls back to `(0, 0)` rather than dropping the save entirely — still a
/// valid, visible-somewhere position to restore later.
fn save_window_state(window: &Window, path: Option<&Path>) {
    let Some(path) = path else {
        return;
    };
    let scale = window.scale_factor();
    let position = window
        .outer_position()
        .map(|position| position.to_logical::<f64>(scale))
        .unwrap_or_default();
    let size = window.inner_size().to_logical::<f64>(scale);
    let state = WindowState {
        x: position.x,
        y: position.y,
        width: size.width,
        height: size.height,
    };
    if let Err(err) = window_state::save(path, &state) {
        eprintln!("warning: failed to save window state: {err}");
    }
}

/// Switches the served file, re-points the watcher at it, bumps `version`,
/// updates the window title, and reloads the WebView. The reload (rather
/// than relying on the live-reload poll alone) is what makes opening a file
/// recover a view that's stuck on an error page whose script has died.
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
    let method = request.method().as_str().to_owned();
    let headers: Vec<(String, String)> = request
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.to_string(),
                value.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    let body = request.body().clone();
    let route_request = routes::RouteRequest {
        method: &method,
        path: &path,
        headers: &headers,
        body: &body,
    };

    let file = file_state.lock().expect("file state mutex poisoned");
    let reply = routes::handle(&route_request, file.as_deref(), version);
    drop(file);
    if debug {
        // The body byte count is only interesting for state-changing
        // requests (PUT/POST) — that's how the body reaches this handler
        // gets confirmed to actually be non-empty when testing on macOS,
        // where `MDVIEW_DEBUG=1` is the only visibility into the WebView's
        // custom-protocol handler.
        let body_suffix = match method.as_str() {
            "PUT" | "POST" => format!(" (body {} bytes)", body.len()),
            _ => String::new(),
        };
        eprintln!("[mdview] {method} {path} -> {}{body_suffix}", reply.status);
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
/// shortcuts (Cmd+Q, Cmd+W, Cmd+C, Cmd+A) work in the window. File ▸ Open…
/// (Cmd+O) posts [`UserEvent::PickFile`] back to the event loop; the View
/// submenu's Zoom In/Out/Actual Size/Reload post [`UserEvent::Zoom`]/
/// [`UserEvent::Reload`] — `run` turns those into an `evaluate_script` call
/// into `assets/viewer.js` (zoom) or a full `load_url` (reload). Because
/// these accelerators are handled by the menu, the corresponding keydown
/// never reaches the WebView at all on macOS, so `assets/viewer.js`'s own
/// keyboard handler (which exists for Windows/Linux and `--browser` mode)
/// never double-fires here — see its module docs.
#[cfg(target_os = "macos")]
fn install_menu(proxy: &EventLoopProxy<UserEvent>) -> Result<muda::Menu> {
    use muda::accelerator::{Accelerator, Code, Modifiers};
    use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};

    const OPEN_ITEM_ID: &str = "open";
    const ZOOM_IN_ITEM_ID: &str = "zoom-in";
    const ZOOM_OUT_ITEM_ID: &str = "zoom-out";
    const ZOOM_RESET_ITEM_ID: &str = "zoom-reset";
    const RELOAD_ITEM_ID: &str = "reload";

    let menu = Menu::new();
    let app = Submenu::with_items("mdview", true, &[&PredefinedMenuItem::quit(None)])?;
    let open = MenuItem::with_id(
        OPEN_ITEM_ID,
        "Open…",
        true,
        Some(Accelerator::new(Some(Modifiers::SUPER), Code::KeyO)),
    );
    let file = Submenu::with_items("File", true, &[&open])?;
    let edit = Submenu::with_items(
        "Edit",
        true,
        &[
            &PredefinedMenuItem::copy(None),
            &PredefinedMenuItem::select_all(None),
        ],
    )?;
    let zoom_in = MenuItem::with_id(
        ZOOM_IN_ITEM_ID,
        "Zoom In",
        true,
        Some(Accelerator::new(Some(Modifiers::SUPER), Code::Equal)),
    );
    let zoom_out = MenuItem::with_id(
        ZOOM_OUT_ITEM_ID,
        "Zoom Out",
        true,
        Some(Accelerator::new(Some(Modifiers::SUPER), Code::Minus)),
    );
    let zoom_reset = MenuItem::with_id(
        ZOOM_RESET_ITEM_ID,
        "Actual Size",
        true,
        Some(Accelerator::new(Some(Modifiers::SUPER), Code::Digit0)),
    );
    let reload = MenuItem::with_id(
        RELOAD_ITEM_ID,
        "Reload",
        true,
        Some(Accelerator::new(Some(Modifiers::SUPER), Code::KeyR)),
    );
    let view = Submenu::with_items("View", true, &[&zoom_in, &zoom_out, &zoom_reset, &reload])?;
    let window = Submenu::with_items("Window", true, &[&PredefinedMenuItem::close_window(None)])?;
    menu.append(&app)?;
    menu.append(&file)?;
    menu.append(&edit)?;
    menu.append(&view)?;
    menu.append(&window)?;
    menu.init_for_nsapp();

    // The handler must be Send + Sync; the proxy itself is only Send.
    let proxy = Mutex::new(proxy.clone());
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let user_event = if event.id() == OPEN_ITEM_ID {
            Some(UserEvent::PickFile)
        } else if event.id() == ZOOM_IN_ITEM_ID {
            Some(UserEvent::Zoom(ZoomDir::In))
        } else if event.id() == ZOOM_OUT_ITEM_ID {
            Some(UserEvent::Zoom(ZoomDir::Out))
        } else if event.id() == ZOOM_RESET_ITEM_ID {
            Some(UserEvent::Zoom(ZoomDir::Reset))
        } else if event.id() == RELOAD_ITEM_ID {
            Some(UserEvent::Reload)
        } else {
            None
        };
        if let Some(user_event) = user_event {
            if let Ok(proxy) = proxy.lock() {
                let _ = proxy.send_event(user_event);
            }
        }
    }));
    Ok(menu)
}

/// Placeholder for platforms without a menu bar, so `run` can hold "the
/// menu" uniformly (a unit value there would trip clippy's `let_unit_value`).
#[cfg(not(target_os = "macos"))]
struct NoMenu;

/// Other platforms get no menu bar (window chrome provides close/quit).
#[cfg(not(target_os = "macos"))]
fn install_menu(_proxy: &EventLoopProxy<UserEvent>) -> Result<NoMenu> {
    Ok(NoMenu)
}

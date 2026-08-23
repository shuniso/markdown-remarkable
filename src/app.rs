//! The native windows: a `tao` event loop hosting one `wry` WebView per open
//! file, each serving [`crate::routes::handle`] through a single shared
//! custom protocol instead of an HTTP server (`server.rs`/`--browser`).
//!
//! Not covered by automated tests — GUI startup/event-loop code isn't
//! practical to exercise the way `routes`, `server`, and `watch` are; see
//! the design doc's "含まない" section. The routing logic it calls into
//! (`routes::handle`) is fully unit-tested on its own. Set `MDVIEW_DEBUG=1`
//! to log every request every WebView makes through the custom protocol,
//! which is how the live-reload path gets verified by hand.

use crate::routes;
use crate::util::file_title;
use crate::watch;
use crate::window_state::{self, MonitorRect, WindowState};
use anyhow::{Context, Result};
use notify::RecommendedWatcher;
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tao::dpi::{LogicalPosition, LogicalSize};
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{
    ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget,
};
use tao::window::{Window, WindowBuilder, WindowId};
use wry::http::header::{HeaderName, HeaderValue, CONTENT_TYPE};
use wry::http::{Request, Response, StatusCode};
use wry::{DragDropEvent, NewWindowResponse, WebView, WebViewBuilder, WebViewId};

#[cfg(target_os = "linux")]
use tao::platform::unix::WindowExtUnix;
#[cfg(target_os = "linux")]
use wry::WebViewBuilderExtUnix;

const WINDOW_WIDTH: f64 = 960.0;
const WINDOW_HEIGHT: f64 = 800.0;

/// Custom-protocol scheme name registered with every WebView.
const PROTOCOL: &str = "mdview";

/// The URL every WebView is pointed at on startup, in the form wry expects
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
/// concluding nothing is coming and showing the file picker instead. Only
/// ever relevant when `run` was given no files at all — see its doc
/// comment.
const OPEN_EVENT_GRACE: Duration = Duration::from_millis(400);

/// How long to wait after the last `Moved`/`Resized` event on a window
/// before writing its geometry to disk — a drag or a resize-by-dragging-
/// the-edge delivers a burst of these, and only the settled result is worth
/// a write.
const WINDOW_STATE_SAVE_DEBOUNCE: Duration = Duration::from_millis(500);

/// How long after a *particular* window is created to ignore `Moved`/
/// `Resized` events on it for the purposes of scheduling a save —
/// `set_outer_position` (called right after a window is built) and window
/// creation itself both generate synthetic events of these kinds, so an
/// event seen on a window before this deadline (measured from that
/// window's own creation, not the app's) never schedules a save. That
/// keeps the first thing ever written to disk for a window from being
/// "whatever the OS/tao happened to settle on before its own initial
/// position finished applying," racing against the value that was actually
/// meant to be restored or cascaded onto it.
const STARTUP_MOVE_GRACE: Duration = Duration::from_secs(1);

/// Which direction a `View ▸ Zoom In/Out/Actual Size` menu item (or its
/// accelerator) moves the zoom level. Forwarded to the focused window's
/// WebView as `window.__mdviewViewer.zoom("in" | "out" | "reset")` — see
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

/// Events posted to the loop from outside it: a window's drag&drop handler,
/// the menu handler, and (on macOS) the OS asking us to open a document
/// (handled directly in the `Event::Opened` arm, not via this enum).
enum UserEvent {
    /// A file dropped onto a specific window. Carries that window's id (set
    /// by the drag&drop handler, which is created per-window and so always
    /// knows exactly which one it belongs to) so `run` can decide whether
    /// to reuse that window (if it's empty) or open a new one — see
    /// [`open_in_window_or_new`].
    OpenFile(PathBuf, WindowId),
    /// Only the macOS menu (File ▸ Open…) posts this today; other
    /// platforms have no menu bar, so there it's handled but never sent.
    /// Carries no window id — which window it applies to (the frontmost
    /// one) is resolved inside `run` via [`focused_window_id`], since the
    /// menu itself has no way to know that.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    PickFile,
    /// Only the macOS menu's View submenu posts this — see
    /// [`install_menu`]. Other platforms have no menu bar at all; their
    /// zoom shortcuts are handled entirely in `assets/viewer.js`, which
    /// never needs to round-trip through the event loop. Applies to the
    /// frontmost window, same as `PickFile`.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Zoom(ZoomDir),
    /// Only the macOS menu's View ▸ Reload posts this — see
    /// [`install_menu`]. `assets/review.js` handles ⌘R/Ctrl+R directly with
    /// `location.reload()` everywhere else (including in every native
    /// window on non-macOS, which has no menu to intercept the key first).
    /// Applies to the frontmost window, same as `PickFile`.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Reload,
}

/// One open window: its `tao` window/`wry` WebView pair, the file state and
/// live-reload version the shared custom-protocol handler serves through
/// [`SharedRegistry`], and the watcher currently pointed at that file (if
/// any). Dropping a `WindowCtx` — as `run` does on `CloseRequested` —
/// closes its window and stops its watcher; `run` is responsible for also
/// removing its entry from the registry first (a `WindowCtx` doesn't know
/// about the registry itself).
///
/// Fields are declared `webview` before `window` deliberately: a struct's
/// fields (with no custom `Drop` impl, as here) drop in declaration order,
/// and the WebView holds a handle into the window it's mounted on, so it
/// should be torn down first rather than possibly outliving the window
/// underneath it, even if only for the duration of drop glue.
struct WindowCtx {
    webview: WebView,
    window: Window,
    /// This window's key into the shared registry — kept here purely so
    /// `CloseRequested` can look up and remove exactly this entry.
    webview_id: String,
    file: Arc<Mutex<Option<PathBuf>>>,
    /// `file`'s value canonicalized (or the path as given, if
    /// canonicalizing fails — e.g. it doesn't exist) — kept in sync by
    /// [`create_window`]/[`open_file`] whenever `file` changes. Cached here
    /// rather than recomputed so [`find_window_with_file`] doesn't have to
    /// touch the filesystem for every other open window on every dedup
    /// check.
    canonical_file: Option<PathBuf>,
    version: Arc<AtomicU64>,
    watcher: Option<RecommendedWatcher>,
    /// When this window was created — used only to satisfy
    /// [`STARTUP_MOVE_GRACE`] and to break ties in [`focused_window_id`]
    /// ("the most recently created window") when no window reports focus.
    created_at: Instant,
}

/// WebView id → that window's file/version state. Populated by
/// [`create_window`] once its WebView exists, cleared by `run` on
/// `CloseRequested`. Every window's custom-protocol handler closure closes
/// over a clone of the same `Arc`, so — although wry requires each
/// `WebViewBuilder` to be given its own handler *value* — all of them defer
/// to this one shared map and the same [`protocol_response`] logic, keyed
/// by each request's `webview_id`. A `webview_id` with no entry (a request
/// arriving for a window that's already been torn down) gets a 404.
type SharedRegistry = Arc<Mutex<HashMap<String, (Arc<Mutex<Option<PathBuf>>>, Arc<AtomicU64>)>>>;

/// Everything creating a new window needs beyond the file to open and the
/// live `windows` map — bundled so the several places a new window can be
/// created (startup, `Event::Opened`, drag&drop, ⌘O, the startup picker)
/// don't each thread the same handful of parameters through by hand.
/// Rebuilt fresh on every event-loop tick from `run`'s own locals (its
/// `event_loop` field in particular is only valid for the current tick).
struct WindowFactory<'a> {
    event_loop: &'a EventLoopWindowTarget<UserEvent>,
    proxy: &'a EventLoopProxy<UserEvent>,
    registry: &'a SharedRegistry,
    next_webview_id: &'a AtomicU64,
    debug: bool,
    saved_window_state: Option<WindowState>,
    monitors: &'a [MonitorRect],
}

/// Opens the native app: one window per entry in `initial`, or — if it's
/// empty — a single empty window that shows a file-picker dialog after
/// [`OPEN_EVENT_GRACE`] unless the OS hands it a file first (a Finder
/// double-click's `Event::Opened` arrives slightly after launch);
/// cancelling that dialog just leaves the empty "drop a file here" page
/// (drop a file, or ⌘O).
///
/// After startup, every file dropped, picked, or delivered via
/// `Event::Opened` either lands in an existing empty/already-open window or
/// opens a brand-new, cascaded one — see [`open_in_window_or_new`] and the
/// `Event::Opened` arm below for the exact rules. Closing a window (⌘W, its
/// close button, or every remaining window closing during ⌘Q) removes it
/// from `windows`; once none are left, the app exits.
///
/// This function's happy path never actually returns `Ok(())`: `tao`'s
/// `EventLoop::run` has return type `!` (it calls `std::process::exit`
/// internally once the loop stops), so the only way out of this function is
/// an `Err` from setup that happens *before* the event loop starts running.
pub fn run(initial: Vec<PathBuf>) -> Result<()> {
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // Keep the menu alive for the whole run: on macOS it's what makes
    // Cmd+Q / Cmd+O / Cmd+C / Cmd+W work at all (wry doesn't add
    // accelerators). Cmd+W in particular needs no window-specific code
    // here — `PredefinedMenuItem::close_window` sends the standard
    // `performClose:` action, which macOS routes to whichever window is
    // currently key, so it already closes just the one window without
    // `run` having to track "the" window itself.
    let _menu = install_menu(&proxy)?;

    let debug = std::env::var_os("MDVIEW_DEBUG").is_some();

    // Where (if anywhere) a window's last size/position lives, and what's
    // there, if anything readable — `None` either way just means "use the
    // built-in defaults" (see resolve_window_geometry). Loaded once, up
    // front: every new window's cascade base position for the whole run is
    // measured from this same snapshot, rather than re-reading a file
    // that's only ever written by *this* process's own debounced/close-time
    // saves.
    let window_state_path = window_state::config_path();
    let saved_window_state = window_state_path.as_deref().and_then(window_state::load);
    let monitors = monitor_rects(&event_loop);

    let registry: SharedRegistry = Arc::new(Mutex::new(HashMap::new()));
    let next_webview_id = AtomicU64::new(0);
    let mut windows: HashMap<WindowId, WindowCtx> = HashMap::new();

    // No files at all still gets exactly one (empty) window, same as
    // before this module went multi-window — just expressed here as the
    // one-element case of the general "one window per file" loop below.
    let initial_is_empty = initial.is_empty();
    let files: Vec<Option<PathBuf>> = if initial_is_empty {
        vec![None]
    } else {
        initial.into_iter().map(Some).collect()
    };
    let startup_factory = WindowFactory {
        event_loop: &event_loop,
        proxy: &proxy,
        registry: &registry,
        next_webview_id: &next_webview_id,
        debug,
        saved_window_state,
        monitors: &monitors,
    };
    for file in files {
        // A duplicate FILE on the command line (`mdview a.md a.md`) gets
        // focused instead of a second window on the same file — the same
        // dedup rule every other open path applies via
        // `open_in_window_or_new`/`find_window_with_file`, just inlined
        // here since there's no single `target` window to hand that helper
        // during startup.
        if let Some(path) = &file {
            if let Some(existing) = find_window_with_file(&windows, path) {
                if let Some(ctx) = windows.get(&existing) {
                    ctx.window.set_focus();
                }
                continue;
            }
        }
        // The `next_webview_id` counter, not `windows.len()` — see
        // `new_window_for_file`'s matching comment.
        let existing_count = next_webview_id.load(Ordering::SeqCst) as usize;
        let (size, position) =
            resolve_window_geometry(saved_window_state, &monitors, existing_count);
        let ctx = create_window(&startup_factory, size, position, file)?;
        windows.insert(ctx.window.id(), ctx);
    }

    // While `Some`, we still owe the user a file picker once this deadline
    // passes (no window has a file yet, and the OS hasn't handed us one).
    // Only ever scheduled when startup created exactly the one empty
    // window above — cleared by the first file that reaches any window by
    // any route.
    let mut picker_deadline = initial_is_empty.then(|| Instant::now() + OPEN_EVENT_GRACE);
    // While `Some`, `save_target` names the window that's moved/resized
    // since the last save and is due to have its geometry written to disk
    // once this deadline passes (see WINDOW_STATE_SAVE_DEBOUNCE) — both
    // cleared once that save happens (or the target window closes first).
    // Every window is also saved unconditionally, deadline or not, on its
    // own `CloseRequested`; the frontmost window (not necessarily
    // `save_target`) is saved on `LoopDestroyed`, since `window.json` only
    // ever holds one window's geometry.
    let mut save_deadline: Option<Instant> = None;
    let mut save_target: Option<WindowId> = None;

    event_loop.run(move |event, window_target, control_flow| {
        let mut exit = false;
        let factory = WindowFactory {
            event_loop: window_target,
            proxy: &proxy,
            registry: &registry,
            next_webview_id: &next_webview_id,
            debug,
            saved_window_state,
            monitors: &monitors,
        };

        match event {
            Event::NewEvents(StartCause::ResumeTimeReached { .. })
                if picker_deadline.is_some_and(|deadline| Instant::now() >= deadline) =>
            {
                picker_deadline = None;
                if let Some(path) = pick_file_dialog() {
                    let target = find_empty_window(&windows);
                    open_in_window_or_new(&mut windows, target, path, &factory);
                }
            }
            Event::NewEvents(StartCause::ResumeTimeReached { .. })
                if save_deadline.is_some_and(|deadline| Instant::now() >= deadline) =>
            {
                save_deadline = None;
                if let Some(id) = save_target.take() {
                    if let Some(ctx) = windows.get(&id) {
                        save_window_state(&ctx.window, window_state_path.as_deref());
                    }
                }
            }
            Event::Opened { urls } => {
                // Finder double-click / "Open With" / `open -a`: the OS
                // hands us file URLs, possibly several at once (opening
                // multiple selected files together) — every markdown one
                // gets a window.
                let dropped: Vec<PathBuf> = urls
                    .iter()
                    .filter_map(|url| url.to_file_path().ok())
                    .filter(|path| is_markdown_file(path))
                    .collect();
                if !dropped.is_empty() {
                    picker_deadline = None;
                }
                for path in dropped {
                    // `open_in_window_or_new` checks for an already-open
                    // match itself (focusing it instead of opening a
                    // duplicate), so nothing extra is needed here beyond
                    // picking the fallback target.
                    let target = find_empty_window(&windows);
                    open_in_window_or_new(&mut windows, target, path, &factory);
                }
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                window_id,
                ..
            } => {
                // Unconditional, bypassing the debounce: this is the last
                // chance to persist whatever this window's final geometry
                // turned out to be.
                if let Some(ctx) = windows.remove(&window_id) {
                    save_window_state(&ctx.window, window_state_path.as_deref());
                    registry
                        .lock()
                        .expect("webview registry mutex poisoned")
                        .remove(&ctx.webview_id);
                    // `ctx`'s own drop (end of this block) stops its
                    // watcher and closes its WebView/window.
                }
                if save_target == Some(window_id) {
                    save_deadline = None;
                    save_target = None;
                }
                if windows.is_empty() {
                    exit = true;
                }
            }
            Event::WindowEvent {
                event: WindowEvent::Moved(_) | WindowEvent::Resized(_),
                window_id,
                ..
            } if windows
                .get(&window_id)
                .is_some_and(|ctx| Instant::now() >= ctx.created_at + STARTUP_MOVE_GRACE) =>
            {
                save_deadline = Some(Instant::now() + WINDOW_STATE_SAVE_DEBOUNCE);
                save_target = Some(window_id);
            }
            Event::LoopDestroyed => {
                // Covers the paths that skip `CloseRequested` entirely —
                // notably ⌘Q / "Quit" from the Dock on macOS, which
                // terminates the application directly rather than closing
                // each window first. This is the last point before the
                // process actually exits; only one window's geometry can
                // be remembered (`window.json` holds a single position),
                // so the frontmost one's is what's kept.
                if let Some(ctx) = focused_window_id(&windows).and_then(|id| windows.get(&id)) {
                    save_window_state(&ctx.window, window_state_path.as_deref());
                }
            }
            Event::UserEvent(UserEvent::OpenFile(path, window_id)) => {
                open_in_window_or_new(&mut windows, Some(window_id), path, &factory);
            }
            Event::UserEvent(UserEvent::PickFile) => {
                if let Some(path) = pick_file_dialog() {
                    let target = focused_window_id(&windows);
                    open_in_window_or_new(&mut windows, target, path, &factory);
                }
            }
            Event::UserEvent(UserEvent::Zoom(dir)) => {
                if let Some(ctx) = focused_window_id(&windows).and_then(|id| windows.get(&id)) {
                    let script = format!(
                        "window.__mdviewViewer && window.__mdviewViewer.zoom('{}')",
                        dir.as_js_arg()
                    );
                    if let Err(err) = ctx.webview.evaluate_script(&script) {
                        eprintln!("warning: failed to change zoom: {err}");
                    }
                }
            }
            Event::UserEvent(UserEvent::Reload) => {
                if let Some(ctx) = focused_window_id(&windows).and_then(|id| windows.get(&id)) {
                    if let Err(err) = ctx.webview.load_url(INITIAL_URL) {
                        eprintln!("warning: failed to reload the view: {err}");
                    }
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

/// Builds one window + WebView pair for `file` (`None` for an empty "drop a
/// file here" window) at `size`/`position`, registers it in
/// `factory.registry` under a fresh `w<N>` id (from
/// `factory.next_webview_id`) once its WebView exists so the shared
/// custom-protocol handler can find its file/version state, and starts
/// watching `file` if one was given. Doesn't insert the result into any
/// `windows` map — callers own that (the startup loop in `run` builds
/// several before the map even exists). `size`/`position` are computed by
/// the caller (via [`resolve_window_geometry`]) rather than by this
/// function itself, since the right `existing_window_count` for the
/// cascade depends on the `windows` map this function never sees.
fn create_window(
    factory: &WindowFactory<'_>,
    size: LogicalSize<f64>,
    position: Option<LogicalPosition<f64>>,
    file: Option<PathBuf>,
) -> Result<WindowCtx> {
    let event_loop = factory.event_loop;
    let proxy = factory.proxy;
    let registry = factory.registry;
    let next_webview_id = factory.next_webview_id;
    let debug = factory.debug;

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
        .with_title(window_title(file.as_deref()))
        .with_min_inner_size(LogicalSize::new(
            window_state::MIN_WIDTH,
            window_state::MIN_HEIGHT,
        ))
        .with_inner_size(size)
        .build(event_loop)
        .context("failed to create window")?;
    if let Some(position) = position {
        window.set_outer_position(position);
    }

    let webview_id = format!("w{}", next_webview_id.fetch_add(1, Ordering::SeqCst));
    let file_state: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(file.clone()));
    let version: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));

    let window_id = window.id();
    let drop_proxy = proxy.clone();
    let protocol_registry = Arc::clone(registry);
    let builder = WebViewBuilder::new()
        .with_id(webview_id.as_str())
        .with_custom_protocol(PROTOCOL.into(), move |webview_id, request| {
            protocol_response(&protocol_registry, webview_id, request, debug)
        })
        .with_navigation_handler(navigation_policy)
        .with_new_window_req_handler(|url, _features| {
            // Never spawn a second window; treat it like a plain link.
            navigation_policy(url);
            NewWindowResponse::Deny
        })
        .with_drag_drop_handler(move |event| handle_drag_drop(event, &drop_proxy, window_id))
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

    // Only now that the WebView exists (so requests for this id could
    // actually start arriving) is this window's state published for the
    // shared protocol handler to find.
    registry
        .lock()
        .expect("webview registry mutex poisoned")
        .insert(
            webview_id.clone(),
            (Arc::clone(&file_state), Arc::clone(&version)),
        );

    let canonical_file = file.as_deref().map(canonical_or_given);
    let watcher = file.as_deref().and_then(|path| start_watch(path, &version));

    Ok(WindowCtx {
        webview,
        window,
        webview_id,
        file: file_state,
        canonical_file,
        version,
        watcher,
        created_at: Instant::now(),
    })
}

/// Creates a new, cascaded window for `file` (`None` is never actually
/// passed by any current caller, but is accepted for symmetry with
/// [`create_window`]) and inserts it into `windows`. A failure to create
/// the window is logged and otherwise ignored — the same non-fatal
/// treatment every other post-startup I/O problem in this module gets,
/// rather than tearing down the whole running app over one new window that
/// didn't open.
fn new_window_for_file(
    windows: &mut HashMap<WindowId, WindowCtx>,
    file: Option<PathBuf>,
    factory: &WindowFactory<'_>,
) {
    // The `next_webview_id` counter (not `windows.len()`): it only ever
    // goes up, so a window that cascaded 3 slots out and was later closed
    // doesn't cause the *next* new window to cascade back to a position an
    // already-open window still occupies.
    let existing_window_count = factory.next_webview_id.load(Ordering::SeqCst) as usize;
    let (size, position) = resolve_window_geometry(
        factory.saved_window_state,
        factory.monitors,
        existing_window_count,
    );
    match create_window(factory, size, position, file) {
        Ok(ctx) => {
            windows.insert(ctx.window.id(), ctx);
        }
        Err(err) => eprintln!("warning: failed to open a new window: {err}"),
    }
}

/// Opens `path`: if it's already showing in some window, that window is
/// simply brought to the front (never a second window on the same file —
/// `PUT /review` replaces a file's sidecar wholesale, so two windows
/// unknowingly open on the same underlying file would race to clobber each
/// other's comments). Otherwise, it's opened into `target`'s window if that
/// exists and is currently empty (`file` is `None`), or a brand-new
/// cascaded window if not. This is the shared rule behind ⌘O, dropping
/// onto a window, the startup file-picker, and (when no already-open match
/// was found) `Event::Opened` — each just differs in how it computes
/// `target` (the window that received the drop, the frontmost window, or
/// the sole startup window, respectively).
fn open_in_window_or_new(
    windows: &mut HashMap<WindowId, WindowCtx>,
    target: Option<WindowId>,
    path: PathBuf,
    factory: &WindowFactory<'_>,
) {
    if let Some(existing) = find_window_with_file(windows, &path) {
        if let Some(ctx) = windows.get(&existing) {
            ctx.window.set_focus();
        }
        return;
    }
    let target_is_empty = target.is_some_and(|id| {
        windows.get(&id).is_some_and(|ctx| {
            ctx.file
                .lock()
                .expect("file state mutex poisoned")
                .is_none()
        })
    });
    if target_is_empty {
        if let Some(ctx) = windows.get_mut(&target.expect("checked by target_is_empty")) {
            open_file(ctx, path);
        }
        return;
    }
    new_window_for_file(windows, Some(path), factory);
}

/// Switches `ctx`'s served file, re-points its watcher, bumps its version,
/// updates its window title, and reloads its WebView. The reload (rather
/// than relying on the live-reload poll alone) is what makes opening a file
/// recover a view that's stuck on an error page whose script has died.
fn open_file(ctx: &mut WindowCtx, path: PathBuf) {
    ctx.canonical_file = Some(canonical_or_given(&path));
    *ctx.file.lock().expect("file state mutex poisoned") = Some(path.clone());
    // Drop the old watcher *before* creating the new one so the two never
    // overlap (they'd double-count a save when both files share a directory).
    ctx.watcher = None;
    ctx.watcher = start_watch(&path, &ctx.version);
    ctx.version.fetch_add(1, Ordering::SeqCst);
    ctx.window.set_title(&window_title(Some(&path)));
    if let Err(err) = ctx.webview.load_url(INITIAL_URL) {
        eprintln!("warning: failed to reload the view: {err}");
    }
}

/// `path.canonicalize()`, or `path` itself if that fails (most likely
/// because it doesn't exist, which shouldn't happen for anything reaching
/// this — every caller passes a path the OS/dialog/drop just handed us as
/// an existing file — but is handled rather than unwrapped regardless).
/// Used wherever two paths need comparing as "the same file" — see
/// [`find_window_with_file`] — so a relative path, an absolute path, and a
/// symlink all pointing at one file are recognized as the same file rather
/// than only an exact string match.
fn canonical_or_given(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// The window (if any) already showing `path`, compared via
/// [`canonical_or_given`] against each window's own cached
/// `canonical_file` — so the same file opened via two different
/// relative/absolute spellings, or through a symlink, is still recognized
/// as one file, not two. This matters beyond cosmetics: `PUT /review`
/// replaces a file's comment sidecar wholesale, so two windows unknowingly
/// open on the same underlying file would race to clobber each other's
/// comments — every path that can open a file (see [`open_in_window_or_new`]
/// and `run`'s startup loop) checks this first.
fn find_window_with_file(windows: &HashMap<WindowId, WindowCtx>, path: &Path) -> Option<WindowId> {
    let target = canonical_or_given(path);
    windows.iter().find_map(|(id, ctx)| {
        (ctx.canonical_file.as_deref() == Some(target.as_path())).then_some(*id)
    })
}

/// The window (if any) with no file open yet. There is at most one of
/// these at any point in a run — the only way one is ever created is the
/// startup case in `run` (when `initial` is empty), and every subsequent
/// open either fills that one window or creates a new, already-non-empty
/// one — so "the" is well-defined.
fn find_empty_window(windows: &HashMap<WindowId, WindowCtx>) -> Option<WindowId> {
    windows.iter().find_map(|(id, ctx)| {
        ctx.file
            .lock()
            .expect("file state mutex poisoned")
            .is_none()
            .then_some(*id)
    })
}

/// The window menu-originated events (Zoom/Reload/Open…) apply to: the one
/// `Window::is_focused` reports true for, or — if none does (e.g. right
/// after the file dialog itself had focus) — the most recently created
/// one, since `HashMap` iteration order isn't creation order and there's no
/// other definition of "last" to fall back to.
fn focused_window_id(windows: &HashMap<WindowId, WindowCtx>) -> Option<WindowId> {
    windows
        .values()
        .find(|ctx| ctx.window.is_focused())
        .or_else(|| windows.values().max_by_key(|ctx| ctx.created_at))
        .map(|ctx| ctx.window.id())
}

/// Every attached monitor's bounds, in logical pixels — used only to check
/// whether a saved/cascaded window position (see [`resolve_window_geometry`])
/// is still somewhere visible; the monitor it was on might have been
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
/// present — falls back to the built-in default) and position for the
/// `existing_window_count`-th window being created (0 for the very first
/// window in a run, incrementing for each new window opened after it) — the
/// saved position, cascaded by `existing_window_count *
/// [`window_state::CASCADE_OFFSET`]` (see [`window_state::cascade_position`]),
/// so each new window peeks out from behind the last rather than stacking
/// exactly on top of it. Only `Some` if that (possibly cascaded) position is
/// still within one of `monitors`; otherwise `None`, and the OS chooses
/// where the window lands, same as before window persistence existed.
fn resolve_window_geometry(
    saved: Option<WindowState>,
    monitors: &[MonitorRect],
    existing_window_count: usize,
) -> (LogicalSize<f64>, Option<LogicalPosition<f64>>) {
    let Some(state) = saved else {
        return (LogicalSize::new(WINDOW_WIDTH, WINDOW_HEIGHT), None);
    };
    let (width, height) = window_state::clamp_size(state.width, state.height);
    let (x, y) = window_state::cascade_position(state.x, state.y, existing_window_count);
    let position =
        window_state::position_is_visible(x, y, monitors).then(|| LogicalPosition::new(x, y));
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

/// The custom-protocol handler shared by every window: looks up
/// `webview_id` in `registry` (populated by [`create_window`], cleared by
/// `run` on `CloseRequested`) for that window's current file/version state
/// and routes the request through `routes::handle`, same as the browser
/// server does for an HTTP request. A `webview_id` with no entry — a
/// request arriving for a window that's already been torn down, or
/// (shouldn't happen) one that was never registered — gets a plain 404
/// instead of a panic.
fn protocol_response(
    registry: &SharedRegistry,
    webview_id: WebViewId<'_>,
    request: Request<Vec<u8>>,
    debug: bool,
) -> Response<Cow<'static, [u8]>> {
    let Some((file_state, version)) = registry
        .lock()
        .expect("webview registry mutex poisoned")
        .get(webview_id)
        .map(|(file, version)| (Arc::clone(file), Arc::clone(version)))
    else {
        if debug {
            eprintln!("[mdview] request for unknown webview id {webview_id:?}");
        }
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Cow::Borrowed(b"404 Not Found" as &[u8]))
            .expect("static fallback response is valid");
    };

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

    // Cloned and the lock released immediately (the temporary guard drops
    // at the end of this statement) rather than held across
    // `routes::handle` below — that call does file I/O (reading the
    // Markdown file, the review sidecar) that can take a while, and this
    // same `file` mutex is also what `open_file`, running on the event
    // loop's own thread, locks (briefly) whenever this window's file
    // changes. Holding it here for the length of a request would make
    // switching this window's file wait on whatever the WebView's
    // protocol-handler thread happens to be doing at that moment.
    let file = file_state
        .lock()
        .expect("file state mutex poisoned")
        .clone();
    let reply = routes::handle(&route_request, file.as_deref(), &version);
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
        eprintln!(
            "[mdview] {webview_id} {method} {path} -> {}{body_suffix}",
            reply.status
        );
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

/// Decides whether a WebView may navigate to `url`. Only the app's own
/// protocol stays inside the window; web links open in the default browser
/// (no window has a back button or URL bar to get home from), and anything
/// else is simply ignored.
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
/// default behavior"). `window_id` is the window that received the drop —
/// each window's drag&drop handler closes over its own, so this is always
/// known precisely, never resolved after the fact.
fn handle_drag_drop(
    event: DragDropEvent,
    proxy: &EventLoopProxy<UserEvent>,
    window_id: WindowId,
) -> bool {
    let DragDropEvent::Drop { paths, .. } = event else {
        return false;
    };
    match paths.into_iter().find(|path| is_markdown_file(path)) {
        Some(path) => {
            // If the event loop has already shut down, there's nothing
            // useful to do with the error — the process is exiting anyway.
            let _ = proxy.send_event(UserEvent::OpenFile(path, window_id));
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

/// A native window's title bar text: `<file name> — mdview`, or just
/// `mdview` before any file has been opened in it.
fn window_title(file: Option<&Path>) -> String {
    match file {
        Some(path) => format!("{} — mdview", file_title(path)),
        None => "mdview".to_string(),
    }
}

/// macOS: a minimal, app-wide application menu (there is only ever one, no
/// matter how many windows are open). Without one, none of the standard
/// shortcuts (Cmd+Q, Cmd+W, Cmd+C, Cmd+A) work in any window. File ▸ Open…
/// (Cmd+O) posts [`UserEvent::PickFile`] back to the event loop; the View
/// submenu's Zoom In/Out/Actual Size/Reload post [`UserEvent::Zoom`]/
/// [`UserEvent::Reload`] — `run` resolves all three to whichever window is
/// currently frontmost (see [`focused_window_id`]) and turns them into an
/// `evaluate_script` call into `assets/viewer.js` (zoom) or a full
/// `load_url` (reload) on that window's WebView. Window ▸ Close closes just
/// the frontmost window natively (see `run`'s doc comment). Because these
/// accelerators are handled by the menu, the corresponding keydown never
/// reaches any WebView at all on macOS, so `assets/viewer.js`'s own
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

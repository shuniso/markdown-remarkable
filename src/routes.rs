//! Pure routing logic shared by the browser server (`server.rs`) and the
//! native app's custom protocol handler (`app.rs`).
//!
//! [`handle`] never touches a socket, a `tiny_http::Request`, or a WebView
//! `http::Request` directly — it takes a bare path string and returns a
//! transport-agnostic [`Reply`] that each caller converts into whatever its
//! transport needs. `Host`-header validation deliberately stays out of here
//! and lives in `server.rs` instead: a custom-protocol request has no `Host`
//! header at all (there's no network hop to spoof), so it isn't a routing
//! concern both callers share.

use crate::render::{self, page, to_html};
use crate::review::{self, ReviewDoc};
use crate::util::file_title;
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, CONTROLS};
use serde::Deserialize;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Bytes that must be percent-encoded before a file name can travel in the
/// `X-Mdview-Title` header: controls (never legal in a header value), `%`
/// (so decoding is unambiguous), and — implicitly, `percent_encoding`
/// always encodes them — every non-ASCII byte. `tiny_http` rejects non-ASCII
/// header values outright and `wry` silently drops them, so a raw `メモ.md`
/// would otherwise crash `--browser` mode and lose the title in the native
/// window. `live.js` reverses this with `decodeURIComponent`.
const TITLE_HEADER_ENCODE_SET: &AsciiSet = &CONTROLS.add(b'%');

/// Title/body shown before any file has been opened (`file == None`) — the
/// app was launched with no `FILE` argument and the user either hasn't
/// picked one yet or cancelled the file-picker dialog.
const EMPTY_TITLE: &str = "markdown-remarkable";
const EMPTY_BODY_HTML: &str = "<p>Drop a Markdown file here.</p>";

/// Header that every state-changing request (`PUT /review`, `POST
/// /export`) must carry, with any value, or the request is rejected with
/// `403`. It exists to defeat cross-site request forgery from a page
/// running in some *other* origin: a plain `fetch`/form submission from
/// another site can't attach a custom header without triggering a CORS
/// preflight, and this server never answers a preflight with the
/// `Access-Control-Allow-*` headers that would let it through.
const REQUEST_HEADER: &str = "X-Mdview-Request";

/// A routing response: status, `Content-Type`, any extra headers, and the
/// body bytes. Deliberately transport-agnostic — see the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub content_type: &'static str,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    fn html(status: u16, body: impl Into<String>) -> Self {
        Reply {
            status,
            content_type: "text/html; charset=utf-8",
            headers: Vec::new(),
            body: body.into().into_bytes(),
        }
    }

    fn text(status: u16, body: impl Into<String>) -> Self {
        Reply {
            status,
            content_type: "text/plain; charset=utf-8",
            headers: Vec::new(),
            body: body.into().into_bytes(),
        }
    }

    /// A JSON response body. Serialization failures (which shouldn't
    /// happen for the `serde_json::Value`s built in this module) fall back
    /// to an empty JSON object rather than panicking.
    fn json(status: u16, value: serde_json::Value) -> Self {
        Reply {
            status,
            content_type: "application/json; charset=utf-8",
            headers: Vec::new(),
            body: serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec()),
        }
    }

    fn with_header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }
}

fn error_json(status: u16, message: &str) -> Reply {
    Reply::json(status, serde_json::json!({ "error": message }))
}

/// A side effect [`handle`] wants its caller to perform beyond sending the
/// returned [`Reply`] — today, only "switch the currently-viewed file to
/// this path" (`PUT /open`, see [`handle_open`]). `handle` itself never
/// touches any window/server state to make that happen; each caller
/// (`server.rs`, `app.rs`) applies it in its own way — `server.rs` never
/// actually receives `OpenFile` in practice (`allow_open: false` makes
/// `handle_open` answer `501` before ever returning one), while `app.rs`
/// swaps the owning window's file, re-points its watcher, bumps its
/// version, updates its title, and reloads the WebView.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Nothing beyond the `Reply` itself — every route except a successful
    /// `PUT /open`/`PUT /nav`.
    None,
    /// Switch the current window's file to this (already canonicalized)
    /// path.
    OpenFile(PathBuf),
    /// Move the current window's back/forward history one step in the given
    /// direction and switch to whatever that lands on. Unlike
    /// [`Action::OpenFile`], this carries no path — the history itself
    /// (which `handle` never touches, see [`handle_nav_put`]'s docs) is what
    /// decides the target, and only the caller (`app.rs`) has mutable access
    /// to it.
    Navigate(NavDirection),
}

/// Which way a `PUT /nav` request asks the current window to move through
/// its back/forward history — see [`Action::Navigate`], [`handle_nav_put`],
/// and `app.rs`'s `UserEvent::Navigate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavDirection {
    Back,
    Forward,
}

/// A routing request: transport-agnostic method/path/headers/body, built by
/// each caller (`server.rs` from a `tiny_http::Request`, `app.rs` from a
/// `wry`/`http::Request`) and handed to [`handle`].
pub struct RouteRequest<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub headers: &'a [(String, String)],
    pub body: &'a [u8],
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// Returns `true` if `req` carries the [`REQUEST_HEADER`] (any value, name
/// matched case-insensitively) required of every state-changing request.
fn has_request_header(req: &RouteRequest) -> bool {
    header_value(req.headers, REQUEST_HEADER).is_some()
}

/// Routes `req` to a response, given the currently-viewed `file` (`None` if
/// nothing has been opened yet) and the live-reload `version` counter.
///
/// A query string on `req.path` (`/?x=1`) is ignored for routing purposes,
/// same as the original `server::run` behavior. Every response carries
/// `Cache-Control: no-store`, `X-Content-Type-Options: nosniff`, and
/// `Content-Security-Policy: frame-ancestors 'none'` (an HTTP-level
/// belt-and-suspenders alongside the `<meta>` tag `page()` already emits —
/// applied to every route because `/body` is `text/html` too and has no
/// `<meta>` of its own).
///
/// Routes:
/// - `GET`/`HEAD /` — the full page (`render::page`), live-reload baseline
///   embedded. `file: None` renders the empty-state placeholder instead of
///   reading anything.
/// - `GET`/`HEAD /version` — the current `version`, as plain text.
/// - `GET`/`HEAD /body` — just the rendered body fragment (`text/html`),
///   with a percent-encoded `X-Mdview-Title: <file name>` header when
///   `file` is `Some`. Used by the live-reload script to swap `<main>`'s
///   contents in place instead of a full page reload.
/// - `GET /review` — the review sidecar as JSON, plus a computed
///   `"unanchored"` array of block hashes that no longer appear in the
///   document. No sidecar on disk yields an empty document, not a `404`.
/// - `PUT /review` — replaces the review sidecar. Requires
///   [`REQUEST_HEADER`] (`403` if absent), a JSON body matching the
///   sidecar schema (`400` if it doesn't parse or fails [`review::validate`]),
///   and a successful write (`500` on I/O failure).
/// - `POST /export` — requires [`REQUEST_HEADER`], writes
///   `<stem>.review.md`, and returns its file name and contents as JSON.
/// - `GET`/`HEAD /asset?p=<percent-encoded relative path>` — a local image
///   file next to the open document, for `<img>` targets `render::to_html`
///   rewrote to this route (`rewrite_local_images: true`, only in effect
///   for `/`/`/body`, never `--export`). See [`handle_asset`] for the full
///   set of checks a request has to pass before any file is read. This is
///   the one route whose response carries its *own*
///   `Content-Security-Policy: default-src 'none'; sandbox` instead of the
///   common `frame-ancestors 'none'` below — an image response has no
///   reason to allow itself to be framed *or* to load anything else at
///   all, and `sandbox` (with no allowlisted capabilities) is defense in
///   depth against an `.svg` served here ever being navigated to directly
///   as a top-level document rather than loaded through `<img>` — it can't
///   run scripts, submit forms, or open popups even then. `<img>` loading
///   is unaffected: `sandbox` only restricts a response when it's
///   navigated to/rendered as its own document.
/// - `GET /tree` — the file tree rooted at `root_dir` (the *window's own*
///   fixed root — see [`handle_tree`] for why this is not simply
///   `asset_parent_dir(file)` any more).
/// - `PUT /open` — switches the currently-viewed file to a `.md`/
///   `.markdown` file named by a JSON body, within `root_dir`'s scope.
///   `allow_open` gates this entirely: `false` (the browser server,
///   `server.rs`) always answers `501` — there's no second window to switch
///   there, so the CLI's own FILE argument stays authoritative for the life
///   of the process. `true` (the native app, `app.rs`) applies the switch
///   to *this* WebView's window via the returned [`Action::OpenFile`]. See
///   [`handle_open`].
/// - `GET /nav` — `{"back": bool, "forward": bool}`, whether the current
///   window's history has anything to go back/forward to. Always
///   `{"back": false, "forward": false}` when `allow_open` is `false` or
///   `nav` is `None` — see [`handle_nav_get`].
/// - `PUT /nav` — moves the current window's history one step per a JSON
///   body `{"dir": "back"|"forward"}`, gated by `allow_open` the same way
///   `PUT /open` is (`501` under `--browser`). See [`handle_nav_put`].
/// - anything else — `404`.
///
/// `root_dir`, when `Some`, is the directory `GET /tree`/`PUT /open` treat
/// as their read/switch boundary — the *window's* fixed root (its very
/// first file's parent, established once and never moved by a later
/// switch — see `app.rs`'s `WindowCtx::root_dir`), not
/// `asset_parent_dir(file)` (which would drift every time the current file
/// changes, making "switch to a subfolder, then switch back" impossible).
/// `None` (always passed by `server.rs`,
/// which has no window/root concept of its own) falls back to
/// `asset_parent_dir(file)` for both routes, same as before `root_dir`
/// existed. `GET`/`HEAD /asset` deliberately keeps using
/// `asset_parent_dir(file)` directly regardless of `root_dir` — an image's
/// `src="…"` is a relative reference resolved against the *document
/// currently rendering it*, not the window's root, so switching files must
/// keep resolving images against whichever file is open right now.
///
/// `GET`/`PUT /review` and `POST /export` all answer `409` when `file` is
/// `None` — there's nothing to review yet. So does `GET`/`HEAD /asset` and
/// `GET /tree`/`PUT /open` — there's no document to resolve a relative path
/// against.
///
/// A read failure (file deleted, permissions changed, etc.) never leaks the
/// absolute path or OS error text (those go to stderr, same as every other
/// error body in this module). `/` answers `500` but still with a full
/// `page()` — live-reload script included — so the view recovers on its
/// own once the file is readable again; `/body` answers `200` with an
/// error fragment for the same reason (the script treats a non-200 `/body`
/// as "reload the whole page").
///
/// Returns `(Reply, Action)` rather than a bare `Reply`: routing never
/// mutates any window/server state itself (see the module docs), so a
/// route that needs to (`PUT /open`, `PUT /nav`) hands its request back as
/// an [`Action`] for the caller to apply in its own way instead.
///
/// `nav`, when `Some`, is `(can_back, can_forward)` — a snapshot of the
/// current window's back/forward history state, read (like `file`/
/// `root_dir`) by the caller before this call rather than mutated by it;
/// `GET /nav`/`PUT /nav` only ever read this snapshot, never the history
/// itself (which `handle` has no access to — see [`Action::Navigate`]'s
/// docs). `None` (always passed by `server.rs`, which has no window/history
/// concept of its own) behaves like `(false, false)`.
pub fn handle(
    req: &RouteRequest,
    file: Option<&Path>,
    version: &AtomicU64,
    allow_remote_images: bool,
    allow_open: bool,
    root_dir: Option<&Path>,
    nav: Option<(bool, bool)>,
) -> (Reply, Action) {
    let route = req.path.split('?').next().unwrap_or("/");
    if route == "/asset" && matches!(req.method, "GET" | "HEAD") {
        // Deliberately bypasses the common `Content-Security-Policy:
        // frame-ancestors 'none'` applied below — `handle_asset`'s own
        // reply carries a stricter `default-src 'none'; sandbox` instead
        // (see the route doc above), and adding both would leave two
        // `Content-Security-Policy` headers on the same response instead
        // of one.
        let reply = handle_asset(req, file)
            .with_header("Cache-Control", "no-store")
            .with_header("X-Content-Type-Options", "nosniff")
            .with_header("Content-Security-Policy", "default-src 'none'; sandbox");
        return (reply, Action::None);
    }
    let (reply, action) = match (req.method, route) {
        ("GET", "/") | ("HEAD", "/") => (
            handle_root(file, version, allow_remote_images, !allow_open),
            Action::None,
        ),
        ("GET", "/version") | ("HEAD", "/version") => (handle_version(version), Action::None),
        ("GET", "/body") | ("HEAD", "/body") => (handle_body(file), Action::None),
        ("GET", "/review") => (handle_get_review(file), Action::None),
        ("PUT", "/review") => (handle_put_review(req, file), Action::None),
        ("POST", "/export") => (handle_export(req, file), Action::None),
        ("GET", "/tree") => (handle_tree(file, root_dir), Action::None),
        ("PUT", "/open") => handle_open(req, file, allow_open, root_dir),
        ("GET", "/nav") => (handle_nav_get(nav, allow_open), Action::None),
        ("PUT", "/nav") => handle_nav_put(req, nav, allow_open),
        _ => (Reply::text(404, "404 Not Found"), Action::None),
    };
    (
        reply
            .with_header("Cache-Control", "no-store")
            .with_header("X-Content-Type-Options", "nosniff")
            .with_header("Content-Security-Policy", "frame-ancestors 'none'"),
        action,
    )
}

fn handle_root(
    file: Option<&Path>,
    version: &AtomicU64,
    allow_remote_images: bool,
    browser_mode: bool,
) -> Reply {
    // Read the live-reload baseline *before* reading the file: if a save
    // lands in between, the version embedded here is guaranteed to be no
    // newer than the content about to be rendered, so the client's first
    // comparison can't spuriously miss that save. See `render::page`'s docs.
    let baseline = version.load(Ordering::SeqCst);
    match file {
        None => Reply::html(
            200,
            page(
                EMPTY_TITLE,
                EMPTY_BODY_HTML,
                Some(baseline),
                allow_remote_images,
                browser_mode,
            ),
        ),
        Some(path) => match read_and_render(path) {
            Ok((title, body_html)) => Reply::html(
                200,
                page(
                    &title,
                    &body_html,
                    Some(baseline),
                    allow_remote_images,
                    browser_mode,
                ),
            ),
            // Still a full page (live script included) so the view can
            // recover by itself once the file is back — a bare 500 would
            // leave the native window with no way to refresh.
            Err(title) => Reply::html(
                500,
                page(
                    &title,
                    &error_fragment(&title),
                    Some(baseline),
                    allow_remote_images,
                    browser_mode,
                ),
            ),
        },
    }
}

/// `/body` doesn't embed a live-reload baseline of its own — the page that
/// fetches it already carries one and only calls here once it's detected
/// that baseline is stale.
fn handle_body(file: Option<&Path>) -> Reply {
    match file {
        None => Reply::html(200, EMPTY_BODY_HTML),
        Some(path) => {
            let (title, body_html) = match read_and_render(path) {
                Ok(rendered) => rendered,
                Err(title) => {
                    let fragment = error_fragment(&title);
                    (title, fragment)
                }
            };
            Reply::html(200, body_html).with_header("X-Mdview-Title", encode_title(&title))
        }
    }
}

/// `GET /review`: the sidecar document (or an empty one, if none exists
/// yet) plus a computed `"unanchored"` array — the hashes in it (block,
/// list item, or table row) that no longer appear anywhere in the current
/// document.
fn handle_get_review(file: Option<&Path>) -> Reply {
    let Some(path) = file else {
        return no_file_open();
    };
    let doc = match review::load(path) {
        Ok(doc) => doc,
        Err(err) => {
            eprintln!("warning: failed to load review sidecar: {err}");
            return error_json(500, "failed to read review data");
        }
    };
    let markdown = match fs::read_to_string(path) {
        Ok(markdown) => markdown,
        Err(err) => {
            eprintln!("warning: failed to read {}: {err}", path.display());
            return error_json(500, "failed to read document");
        }
    };
    let live_anchors = render::anchors(&markdown);
    let unanchored: Vec<&str> = review::unanchored(&doc, &live_anchors);

    let mut value = serde_json::to_value(&doc).unwrap_or_else(|_| serde_json::json!({}));
    value["unanchored"] = serde_json::json!(unanchored);
    Reply::json(200, value)
}

/// `PUT /review`: replaces the sidecar wholesale. Requires
/// [`REQUEST_HEADER`], a body that parses as [`ReviewDoc`] whose `file`
/// names the currently-open document (guards against a stale client — one
/// that loaded a different file, or hasn't finished its first `GET
/// /review` yet — clobbering the wrong sidecar) and passes
/// [`review::validate`], and a successful write.
fn handle_put_review(req: &RouteRequest, file: Option<&Path>) -> Reply {
    if !has_request_header(req) {
        return error_json(403, "missing X-Mdview-Request header");
    }
    let Some(path) = file else {
        return no_file_open();
    };

    let mut doc: ReviewDoc = match serde_json::from_slice(req.body) {
        Ok(doc) => doc,
        Err(err) => {
            eprintln!("warning: PUT /review body is not a valid review document: {err}");
            return error_json(400, "invalid review document");
        }
    };
    if doc.file != file_title(path) {
        // `doc.file` is client-supplied — never logged verbatim (it could
        // otherwise be used to smuggle attacker-chosen text into stderr, or
        // to leak a path/name a script running elsewhere on the machine
        // wouldn't otherwise be able to observe). Only its length is
        // reported.
        eprintln!(
            "warning: review document file name mismatch (got {} bytes)",
            doc.file.len()
        );
        return error_json(400, "document file name mismatch");
    }
    if let Err(err) = review::validate(&mut doc) {
        eprintln!("warning: PUT /review body failed validation: {err}");
        return error_json(400, "invalid review document");
    }
    if let Err(err) = review::save(path, &doc) {
        eprintln!("warning: failed to save review sidecar: {err}");
        return error_json(500, "failed to save review data");
    }
    Reply::json(200, serde_json::json!({ "ok": true }))
}

/// `POST /export`: writes `<stem>.review.md` next to the document and
/// returns its file name (never a full path) and contents. Requires
/// [`REQUEST_HEADER`].
fn handle_export(req: &RouteRequest, file: Option<&Path>) -> Reply {
    if !has_request_header(req) {
        return error_json(403, "missing X-Mdview-Request header");
    }
    let Some(path) = file else {
        return no_file_open();
    };

    let markdown = match fs::read_to_string(path) {
        Ok(markdown) => markdown,
        Err(err) => {
            eprintln!("warning: failed to read {}: {err}", path.display());
            return error_json(500, "failed to read document");
        }
    };
    let doc = match review::load(path) {
        Ok(doc) => doc,
        Err(err) => {
            eprintln!("warning: failed to load review sidecar: {err}");
            return error_json(500, "failed to read review data");
        }
    };
    match review::export(path, &markdown, &doc) {
        Ok((export_path, exported_markdown)) => {
            let file_name = export_path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            Reply::json(
                200,
                serde_json::json!({ "path": file_name, "markdown": exported_markdown }),
            )
        }
        Err(err) => {
            eprintln!("warning: failed to write review export: {err}");
            error_json(500, "failed to write export file")
        }
    }
}

/// Maximum directory depth [`handle_tree`] walks below the open document's
/// parent directory. `0` is the parent directory's own direct children; a
/// subdirectory found there is itself scanned (depth `1`) as long as
/// `1 <= TREE_MAX_DEPTH`, and so on. A subdirectory found at depth
/// `TREE_MAX_DEPTH` is listed (if it has a `.md`/`.markdown` file directly
/// in it — see [`scan_tree_dir`]) but never itself descended into, so
/// nothing below it appears at all — not even to decide whether to prune
/// it, unlike every shallower directory.
const TREE_MAX_DEPTH: usize = 4;

/// Maximum number of entries [`handle_tree`] returns before giving up and
/// marking the response `"truncated": true` — see [`scan_tree_dir`]. Bounds
/// the *response size* a directory tree with an enormous number of
/// Markdown files can force onto a single `GET /tree` request.
const TREE_MAX_ENTRIES: usize = 2000;

/// Maximum number of directory entries (of any kind — file, dir, symlink,
/// excluded, non-UTF-8, ...) [`scan_tree_dir`] will look at in total across
/// the whole walk before giving up, independently of [`TREE_MAX_ENTRIES`].
/// Bounds the *work* the walk itself can be made to do: a directory subtree
/// containing thousands of folders with no Markdown in any of them would
/// all get pruned from the output (never touching [`TREE_MAX_ENTRIES`] at
/// all) but could still make the walk itself take a very long time — this
/// caps that separately from how many entries actually end up in the
/// response.
const TREE_MAX_VISITED_ENTRIES: usize = 20_000;

/// One entry in `GET /tree`'s `"entries"` array — see [`handle_tree`].
struct TreeEntry {
    /// `/`-separated path relative to the open document's parent directory
    /// — what a `PUT /open` request should echo back as its own `path`.
    path: String,
    /// The entry's own file/directory name (`path`'s last segment).
    name: String,
    /// `"dir"` or `"file"`.
    kind: &'static str,
}

/// The directory `GET /tree`/`PUT /open` scope their walk/switch to —
/// `root_dir` if the caller passed one (the native app always does, once a
/// window has a file open at all — see `app.rs`'s `WindowCtx::root_dir`),
/// or `asset_parent_dir(md_path)` otherwise (`server.rs`, which has no
/// window/root concept, always passes `None`). See [`handle`]'s docs for
/// why this must be the window's *fixed* root rather than
/// `asset_parent_dir` of whatever happens to be open right now.
fn tree_root_dir<'a>(root_dir: Option<&'a Path>, md_path: &'a Path) -> &'a Path {
    root_dir.unwrap_or_else(|| asset_parent_dir(md_path))
}

/// `md_path`, expressed relative to `root` and `/`-joined — the same form
/// [`scan_tree_dir`]'s own `path`/`current` values take. Falls back to just
/// `md_path`'s own file name if `md_path` isn't actually under `root` (a
/// `strip_prefix` mismatch — expected whenever `root_dir` is `None` and
/// `asset_parent_dir` was used instead, since that's always exactly
/// `md_path`'s own parent, or when `md_path` hasn't been canonicalized the
/// same way `root` has — see `app.rs`'s notes on why this still ends up
/// correct in every reachable case), and to an *empty string* — never a
/// leaked absolute/relative path — if `md_path` doesn't even have a file
/// name (see [`file_title`]'s own fallback, which this deliberately does
/// *not* use, for the case this guards against).
fn tree_relative_path(md_path: &Path, root: &Path) -> String {
    if let Ok(relative) = md_path.strip_prefix(root) {
        return path_components_to_slash(relative);
    }
    md_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Joins a [`Path`]'s [`Component::Normal`] segments with `/`, ignoring any
/// other component kind (there shouldn't be any in a path this module ever
/// builds this way, but silently dropping rather than propagating an
/// error/panic is consistent with every other best-effort path-to-string
/// conversion here).
fn path_components_to_slash(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// `GET /tree`: the Markdown file tree rooted at [`tree_root_dir`], as
/// JSON:
/// ```json
/// { "root": "<root dir name>", "current": "<path relative to root>",
///   "entries": [ { "path": "...", "name": "...", "kind": "dir"|"file" } ],
///   "truncated": true }
/// ```
/// `root` is the root directory's *name* only (never an absolute path —
/// same reasoning as every other path this module ever hands back to a
/// client). `current` is the currently-open file's path relative to that
/// root (see [`tree_relative_path`]) — a bare file name when the window's
/// root is that file's own parent (the common case: no switch has
/// descended into a subfolder yet), a `/`-joined relative path otherwise.
/// `"truncated"` is present (and `true`) only once [`TREE_MAX_ENTRIES`] or
/// [`TREE_MAX_VISITED_ENTRIES`] is hit; otherwise the field is omitted
/// entirely.
///
/// `409` if `file` is `None` — there's no document to root the tree at.
/// A directory that can't be read (permissions, deleted mid-walk, ...) is
/// skipped rather than failing the whole request, same as every other
/// best-effort read in this module.
fn handle_tree(file: Option<&Path>, root_dir: Option<&Path>) -> Reply {
    let Some(md_path) = file else {
        return no_file_open();
    };
    let parent = tree_root_dir(root_dir, md_path);
    let root = parent
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".to_string());
    let current = tree_relative_path(md_path, parent);

    let (entries, truncated) =
        collect_tree_entries(parent, TREE_MAX_ENTRIES, TREE_MAX_VISITED_ENTRIES);

    let entries_json: Vec<serde_json::Value> = entries
        .into_iter()
        .map(|entry| {
            serde_json::json!({
                "path": entry.path,
                "name": entry.name,
                "kind": entry.kind,
            })
        })
        .collect();
    let mut value = serde_json::json!({
        "root": root,
        "current": current,
        "entries": entries_json,
    });
    if truncated {
        value["truncated"] = serde_json::json!(true);
    }
    Reply::json(200, value)
}

/// The testable core of `GET /tree`'s walk: every entry under `parent`
/// (see [`scan_tree_dir`]) plus whether either budget was exhausted.
/// Factored out from [`handle_tree`] purely so tests can exercise the
/// budget-exhaustion edge cases (see `scan_tree_dir`'s docs on why
/// discarding already-collected entries there was a bug) with small budgets
/// instead of having to create thousands of real files on disk to reach
/// [`TREE_MAX_ENTRIES`]/[`TREE_MAX_VISITED_ENTRIES`].
fn collect_tree_entries(
    parent: &Path,
    entries_budget: usize,
    visited_budget: usize,
) -> (Vec<TreeEntry>, bool) {
    let mut entries = Vec::new();
    let mut state = ScanBudget {
        budget: entries_budget,
        truncated: false,
        visited_budget,
        visited: 0,
    };
    scan_tree_dir(parent, "", 0, &mut state, &mut entries);
    (entries, state.truncated)
}

/// The mutable state [`scan_tree_dir`] threads through its whole recursive
/// walk, bundled into one struct purely to keep that function's own
/// parameter list short (`clippy::too_many_arguments`) — every recursive
/// call passes the same `&mut ScanBudget` straight through, so this is
/// exactly the flat set of `&mut usize`/`&mut bool` parameters an earlier
/// revision threaded by hand. See [`scan_tree_dir`]'s docs for what each
/// field means and how the two budgets interact.
struct ScanBudget {
    budget: usize,
    truncated: bool,
    visited_budget: usize,
    visited: usize,
}

/// Recursively fills `out` with every entry [`handle_tree`] should list
/// inside `root.join(rel_prefix)` (`rel_prefix == ""` for `root` itself),
/// in "each level: directories then files, both name-sorted
/// case-insensitively" depth-first order — a directory's own row is
/// immediately followed by everything inside it.
///
/// A subdirectory is only recursed into (to decide whether it belongs in
/// the output at all) while `depth < TREE_MAX_DEPTH`; deeper than that it's
/// simply never looked at. Once recursed into, a subdirectory that turns
/// out to contain no `.md`/`.markdown` file anywhere within the depth
/// budget contributes nothing and is pruned — neither it nor its (empty)
/// contents are added to `out`. Hidden entries (name starting with `.`,
/// which also covers `.git`), `node_modules`, and `target` are skipped
/// outright, as directories, without being recursed into at all. A
/// directory entry (symlink or not) that `DirEntry::file_type` reports as
/// [`std::fs::FileType::is_symlink`] is skipped too — this walk never
/// follows a symlink, matching `GET /asset`'s own refusal to (see
/// `docs/SECURITY.md`). A name that isn't valid UTF-8 is skipped too rather
/// than shown lossily (`OsStr::to_str` returning `None`) — a
/// `to_string_lossy` placeholder full of U+FFFD wouldn't actually be
/// openable via `PUT /open` (which requires the exact UTF-8 name back), so
/// showing one at all would just be a dead end in the UI.
///
/// `state` (see [`ScanBudget`]) bundles the mutable counters threaded
/// through the whole recursive walk, one `&mut` passed straight down every
/// call. `state.budget` is the number of entries still allowed before
/// hitting the caller's entries cap ([`TREE_MAX_ENTRIES`] in production,
/// parameterized via [`collect_tree_entries`] for testing); once it reaches
/// `0`, `state.truncated` is set. `state.visited`/`state.visited_budget`
/// are the separate, independent cap on total directory entries *looked
/// at* (including ones that are pruned, excluded, or don't qualify) — see
/// [`TREE_MAX_VISITED_ENTRIES`]. Once *either* budget is exhausted,
/// `state.truncated` is set and nothing more is added anywhere in the walk
/// (every recursive call shares the same `state`, so this stops the
/// *whole* traversal, not just the directory currently being
/// scanned) — but, critically, whatever was already collected (in `out`,
/// and in any still-pending `child_entries` a caller higher up the call
/// stack is holding) is *never discarded* once budgeted; only entries that
/// would come *after* the cap are ever left out. Earlier revisions of this
/// function discarded an entire subdirectory's worth of already-budgeted
/// results whenever the cap was hit while about to add *that
/// subdirectory's own* row — this is the fix.
fn scan_tree_dir(
    root: &Path,
    rel_prefix: &str,
    depth: usize,
    state: &mut ScanBudget,
    out: &mut Vec<TreeEntry>,
) {
    if state.truncated {
        return;
    }
    let abs_dir = if rel_prefix.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel_prefix)
    };
    let Ok(read_dir) = fs::read_dir(&abs_dir) else {
        // Unreadable directory (permissions, deleted mid-walk, ...): skip
        // it silently, same as every other best-effort read in this
        // module — a `GET /tree` for the rest of the tree shouldn't fail
        // just because one subdirectory couldn't be listed.
        return;
    };

    let mut dir_names: Vec<String> = Vec::new();
    let mut file_names: Vec<String> = Vec::new();
    for entry in read_dir.flatten() {
        state.visited += 1;
        if state.visited > state.visited_budget {
            // Stop reading *this* directory's own entries right away, but
            // still process whatever was already found above (sorted,
            // possibly recursed into, possibly added to `out` below) —
            // only further exploration (deeper directories, later
            // siblings elsewhere in the tree) is what actually stops, via
            // every other call's own `if state.truncated { return; }` at
            // the top. Nothing already found here is thrown away.
            state.truncated = true;
            break;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            if is_excluded_tree_dir(&name) {
                continue;
            }
            dir_names.push(name);
        } else if file_type.is_file() && is_markdown_name(&name) {
            file_names.push(name);
        }
    }
    dir_names.sort_by_key(|name| name.to_lowercase());
    file_names.sort_by_key(|name| name.to_lowercase());

    for name in dir_names {
        if depth >= TREE_MAX_DEPTH {
            continue;
        }
        let child_rel = join_tree_path(rel_prefix, &name);
        let mut child_entries = Vec::new();
        scan_tree_dir(root, &child_rel, depth + 1, state, &mut child_entries);
        if child_entries.is_empty() {
            // No `.md`/`.markdown` file anywhere inside (within the depth
            // budget, or nothing could be verified because a budget ran
            // out while exploring it) — prune this directory entirely
            // rather than show an empty folder.
            continue;
        }
        if state.budget == 0 {
            // No budget left for *this directory's own* placeholder row,
            // but everything already found inside it was legitimately
            // budgeted by the recursive call above — keep it (as orphaned
            // rows, minus their own parent row) rather than discard
            // already-valid results just because one more row won't fit.
            out.extend(child_entries);
            continue;
        }
        state.budget -= 1;
        out.push(TreeEntry {
            path: child_rel,
            name,
            kind: "dir",
        });
        out.extend(child_entries);
    }

    for name in file_names {
        if state.budget == 0 {
            state.truncated = true;
            return;
        }
        state.budget -= 1;
        let child_rel = join_tree_path(rel_prefix, &name);
        out.push(TreeEntry {
            path: child_rel,
            name,
            kind: "file",
        });
    }
}

fn join_tree_path(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

/// `true` for a directory name [`scan_tree_dir`] never even looks inside:
/// hidden directories (`.` prefix — this also covers `.git`), plus
/// `node_modules` and `target` by name.
fn is_excluded_tree_dir(name: &str) -> bool {
    name.starts_with('.') || matches!(name, "node_modules" | "target")
}

/// `true` if `name`'s extension is `.md`/`.markdown`, matched
/// case-insensitively — the same test `app.rs`'s `is_markdown_file` applies
/// to a dropped/opened file, applied here to a bare file name instead of a
/// full path.
fn is_markdown_name(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown"))
}

/// The JSON body `PUT /open` expects: `{"path": "<relative path>"}`, the
/// same `path` value a `GET /tree` entry reported.
#[derive(Deserialize)]
struct OpenRequest {
    path: String,
}

/// `PUT /open`: switches the currently-viewed file to `path` (a JSON body,
/// `{"path": "..."}`), within [`tree_root_dir`]'s scope. Gated entirely by
/// `allow_open` — `false` (the browser server) always answers `501` before
/// checking anything else, so `--browser`'s single served file never
/// changes out from under whatever else might have it open in a tab.
///
/// With `allow_open: true` (the native app), every one of the following
/// must hold, checked in this order, before the switch is accepted:
/// - [`REQUEST_HEADER`] is present — `403` otherwise (same CSRF defense as
///   `PUT /review`/`POST /export`).
/// - `file` is `Some` — `409` otherwise (nothing to resolve a relative
///   path against).
/// - The body parses as [`OpenRequest`] — `400` otherwise. `path` is taken
///   as a literal, already-decoded relative path — a JSON string carries
///   its bytes directly, with no percent-encoding layer of its own. Every
///   caller is expected to have decoded any percent-encoding *before*
///   putting a segment in this field: `assets/tree.js`'s `GET /tree`
///   response is already plain text, and `assets/viewer.js`'s relative-link
///   click handler explicitly `decodeURIComponent`s each `href` path
///   segment first (`render::to_html`'s `escape_href` percent-encodes
///   spaces/non-ASCII characters into a rendered `<a href>`, so a link to a
///   file whose name isn't plain ASCII would otherwise arrive here still
///   percent-encoded and fail every check below against the real file
///   name). A caller that sends a still-encoded segment doesn't reach
///   anything unintended — it simply fails to resolve, the same as any
///   other nonexistent `path` — but won't successfully switch to the file
///   it meant either.
/// - `path`, parsed as a [`Path`], is made up *only* of
///   [`Component::Normal`] segments (see [`is_plain_relative_path`], the
///   same check [`handle_asset`] applies to its own `p` query value) and
///   its extension (lowercased) is `md` or `markdown` — `400` otherwise.
/// - `root_dir.join(path)` (not yet canonicalized) is not itself a symlink
///   — `404` otherwise, checked via `fs::symlink_metadata` so the check
///   can't be fooled by `canonicalize` transparently resolving it first.
///   This is stricter than the escape check just below on its own: a
///   symlink that resolves to a target *inside* the root would pass that
///   check, but `GET /tree` would never have listed it in the first place
///   (`scan_tree_dir` skips every symlink outright) — this keeps `PUT
///   /open` from accepting, via a hand-crafted `path`, something the tree
///   itself would never offer to switch to.
/// - Joining it onto `root_dir` and calling `canonicalize()` on both
///   succeeds, *and* the joined path's canonical form has `root_dir`'s own
///   canonical form as a prefix — `404` otherwise. Same symlink-*escape*
///   defense as [`handle_asset`] (distinct from the symlink-*at-all* check
///   just above).
/// - The canonicalized target's metadata says it's a regular file
///   (`fs::metadata(..).is_file()`) — `404` otherwise. Without this, a
///   `path` naming a directory, FIFO, or device node that happens to end
///   in `.md` would pass every check above and then hang the WebView's
///   protocol-handler thread the moment the switch lands and something
///   tries to `fs::read_to_string` it (a FIFO's read blocks until a writer
///   opens the other end, which may be never).
///
/// The [`TREE_MAX_DEPTH`]/hidden-directory/`node_modules`/`target`
/// exclusions and the [`TREE_MAX_ENTRIES`]/[`TREE_MAX_VISITED_ENTRIES`]
/// caps `GET /tree` applies are a *display* concern only (what the tree
/// pane chooses to draw) — none of them are access-control boundaries this
/// function enforces. A `.md` file that lives inside a hidden directory or
/// `node_modules` (something `GET /tree` would never list) is still a
/// perfectly valid `PUT /open` target as long as it's within `root_dir` and
/// passes every check above; the only boundary that matters here is the
/// canonicalize-and-prefix escape check.
///
/// On success: `200` `{"ok": true, "reloaded": true}`, plus
/// [`Action::OpenFile`] carrying the canonicalized target path for the
/// caller to actually apply — this function never touches any window/server
/// state itself (see [`Action`]'s docs). `"reloaded": true` tells the
/// client it never needs to reload itself: this only ever succeeds under
/// `allow_open: true` (the native app), whose caller *always* ends up
/// reloading some window in response to [`Action::OpenFile`] — either this
/// one (the common case) or, if the target is already open in a different
/// window, that other window is focused instead and brought to front while
/// this one is left untouched (see `app.rs`'s `UserEvent::SwitchFile`
/// handling) — neither outcome ever needs `assets/tree.js` to additionally
/// call `location.reload()` itself.
fn handle_open(
    req: &RouteRequest,
    file: Option<&Path>,
    allow_open: bool,
    root_dir: Option<&Path>,
) -> (Reply, Action) {
    if !allow_open {
        return (
            error_json(501, "switching files is not supported in --browser mode"),
            Action::None,
        );
    }
    if !has_request_header(req) {
        return (
            error_json(403, "missing X-Mdview-Request header"),
            Action::None,
        );
    }
    let Some(md_path) = file else {
        return (no_file_open(), Action::None);
    };

    let Ok(open_request) = serde_json::from_slice::<OpenRequest>(req.body) else {
        return (error_json(400, "invalid request body"), Action::None);
    };
    let rel_path = Path::new(&open_request.path);
    if !is_plain_relative_path(rel_path) {
        return (error_json(400, "invalid path"), Action::None);
    }
    let extension_allowed = rel_path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown"));
    if !extension_allowed {
        return (error_json(400, "invalid file extension"), Action::None);
    }

    let parent = tree_root_dir(root_dir, md_path);
    let candidate = parent.join(rel_path);
    // Checked on the *un*canonicalized `candidate` — canonicalize()
    // transparently follows symlinks, which is exactly what would hide
    // this from a check made afterward. See the doc comment above for why
    // this is stricter than (and separate from) the escape check below.
    if fs::symlink_metadata(&candidate).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return (error_json(404, "not found"), Action::None);
    }

    let Ok(parent_canonical) = parent.canonicalize() else {
        return (error_json(404, "not found"), Action::None);
    };
    let Ok(candidate_canonical) = candidate.canonicalize() else {
        return (error_json(404, "not found"), Action::None);
    };
    if !candidate_canonical.starts_with(&parent_canonical) {
        return (error_json(404, "not found"), Action::None);
    }
    let Ok(metadata) = fs::metadata(&candidate_canonical) else {
        return (error_json(404, "not found"), Action::None);
    };
    if !metadata.is_file() {
        return (error_json(404, "not found"), Action::None);
    }

    (
        Reply::json(200, serde_json::json!({ "ok": true, "reloaded": true })),
        Action::OpenFile(candidate_canonical),
    )
}

/// `GET /nav`: `{"back": bool, "forward": bool}`, read straight off the
/// `nav` snapshot [`handle`] was given — `(false, false)` whenever
/// `allow_open` is `false` (there's no per-window history under
/// `--browser`, same reasoning as `PUT /open`'s `501`) or `nav` is `None`
/// (no window/history yet — e.g. an empty native window with no file open).
/// Never fails: unlike `GET /tree`, there's nothing here that needs a file
/// to be open to answer meaningfully — "nothing to go back/forward to" is
/// itself a valid, `200` answer.
fn handle_nav_get(nav: Option<(bool, bool)>, allow_open: bool) -> Reply {
    let (can_back, can_forward) = if allow_open {
        nav.unwrap_or((false, false))
    } else {
        (false, false)
    };
    Reply::json(
        200,
        serde_json::json!({ "back": can_back, "forward": can_forward }),
    )
}

/// The JSON body `PUT /nav` expects: `{"dir": "back"|"forward"}`.
#[derive(Deserialize)]
struct NavRequest {
    dir: String,
}

/// `PUT /nav`: moves the current window's back/forward history one step, per
/// a JSON body `{"dir": "back"|"forward"}`. Gated by `allow_open` exactly
/// like [`handle_open`] — `false` (the browser server) always answers `501`
/// first, before checking anything else, since `--browser` has no
/// per-window history to move through at all.
///
/// With `allow_open: true`, checked in this order:
/// - [`REQUEST_HEADER`] is present — `403` otherwise (same CSRF defense as
///   every other state-changing route in this module).
/// - `nav` is `Some` — `409` otherwise (no window/history yet, e.g. an
///   empty window with no file open — nothing to navigate).
/// - The body parses as [`NavRequest`] and `dir` is exactly `"back"` or
///   `"forward"` — `400` otherwise.
/// - The requested direction is actually available, per the `nav` snapshot
///   (`can_back`/`can_forward`) — `409` otherwise (asking to go back with
///   nothing behind it, or forward with nothing ahead — a stale client, or
///   a race with another request that already moved the cursor).
///
/// On success: `200` `{"ok": true, "reloaded": true}` (same "the caller
/// always ends up reloading some window" contract [`handle_open`]'s own
/// `"reloaded"` documents), plus [`Action::Navigate`] for the caller to
/// actually move its history and switch the window's file — this function
/// never touches the history itself, only the `nav` snapshot it was handed
/// (see [`Action::Navigate`]'s docs for why: `handle` has no mutable access
/// to it, only the caller does).
fn handle_nav_put(
    req: &RouteRequest,
    nav: Option<(bool, bool)>,
    allow_open: bool,
) -> (Reply, Action) {
    if !allow_open {
        return (
            error_json(501, "navigation is not supported in --browser mode"),
            Action::None,
        );
    }
    if !has_request_header(req) {
        return (
            error_json(403, "missing X-Mdview-Request header"),
            Action::None,
        );
    }
    let Some((can_back, can_forward)) = nav else {
        return (no_file_open(), Action::None);
    };

    let Ok(nav_request) = serde_json::from_slice::<NavRequest>(req.body) else {
        return (error_json(400, "invalid request body"), Action::None);
    };
    let direction = match nav_request.dir.as_str() {
        "back" => NavDirection::Back,
        "forward" => NavDirection::Forward,
        _ => return (error_json(400, "invalid direction"), Action::None),
    };
    let available = match direction {
        NavDirection::Back => can_back,
        NavDirection::Forward => can_forward,
    };
    if !available {
        return (error_json(409, "no further history"), Action::None);
    }

    (
        Reply::json(200, serde_json::json!({ "ok": true, "reloaded": true })),
        Action::Navigate(direction),
    )
}

/// File extensions (already lowercased for comparison) `GET /asset` will
/// serve — see [`handle_asset`]. Deliberately an allowlist, not a
/// denylist: any extension not on it (`.txt`, `.md`, no extension at all,
/// ...) is treated the same as a file that doesn't exist (`404`), so this
/// route can never be used to read an arbitrary file next to the document
/// regardless of what other checks it passes.
const ASSET_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "svg", "avif", "ico",
];

/// Largest file `GET /asset` will read and return — see [`handle_asset`].
/// Comfortably larger than any image actually worth inlining in a
/// Markdown document, while still bounding how much memory/response size
/// an oversized (accidental or planted) file next to the document can
/// force onto a single request.
const ASSET_MAX_BYTES: u64 = 20 * 1024 * 1024;

/// The directory `GET /asset` resolves its `p` query value against —
/// `md_path`'s parent, falling back to `.` (the current directory) when
/// that parent is empty. `Path::parent()` on a bare relative file name
/// with no directory components at all (e.g. `doc.md`, from `markdown-remarkable
/// doc.md` run in that file's own directory) returns `Some("")`, not
/// `None` and not `.` — and `Path::new("").canonicalize()` fails with
/// `ENOENT` rather than resolving to the current directory the way a
/// shell would treat a bare file name. Left unhandled, that used to make
/// every `/asset` request `404` whenever markdown-remarkable was opened this way. Same
/// fallback `main::ensure_not_same_file` uses, for the same reason.
pub(crate) fn asset_parent_dir(md_path: &Path) -> &Path {
    md_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

/// `GET /asset?p=<percent-encoded relative path>`: serves a local image
/// file from next to the currently-open Markdown document. The one route
/// in this module that reads a file other than the document itself or its
/// review sidecar — see `docs/SECURITY.md`'s "Files read" section for the
/// resulting guarantee this has to uphold.
///
/// Every one of the following must hold, checked in this order, before any
/// file is touched:
/// - `file` is `Some` — `409` otherwise (nothing to resolve a relative
///   path against).
/// - The query string carries a `p` parameter, and percent-decoding its
///   value succeeds as UTF-8 — `400` otherwise.
/// - The decoded value, parsed as a [`Path`], is made up *only* of
///   [`Component::Normal`] segments — no `..` ([`Component::ParentDir`]),
///   no absolute root ([`Component::RootDir`]), no Windows drive prefix
///   ([`Component::Prefix`]), no bare `.` ([`Component::CurDir`]), and not
///   empty. `400` otherwise — this rejects `p=../secret`, `p=/etc/hosts`,
///   and (since decoding already happened) `p=%2e%2e%2fsecret` alike.
/// - Its extension, lowercased, is on [`ASSET_EXTENSIONS`] — `404`
///   otherwise, the same status a nonexistent file would get, so this
///   can't be used to distinguish "wrong extension" from "no such file".
/// - Joining it onto the document's parent directory and calling
///   `canonicalize()` on both succeeds, *and* the joined path's
///   canonical form has the parent directory's own canonical form as a
///   prefix — `404` on either failure. This is what actually blocks a
///   symlink planted inside the allowed directory from resolving to a
///   target *outside* it; the `Component::Normal` check above can't catch
///   that by itself.
/// - Its metadata is readable and `len() <= ASSET_MAX_BYTES` — `413` if
///   larger.
///
/// A read failure past all of the above (deleted between the metadata
/// check and the read, permissions changed, ...) is `404` — the OS error
/// and path go to stderr only, never the response body, same as every
/// other read failure in this module.
///
/// On success: `200`, the file's raw bytes, and `Content-Type` derived
/// from its extension (`image/svg+xml` for `.svg`; the obvious `image/*`
/// MIME type for every other allowed extension). The
/// `Content-Security-Policy: default-src 'none'; sandbox` this response
/// needs instead of the common `frame-ancestors 'none'` is applied by the
/// caller ([`handle`]), not here.
fn handle_asset(req: &RouteRequest, file: Option<&Path>) -> Reply {
    let Some(md_path) = file else {
        return no_file_open();
    };
    let parent = asset_parent_dir(md_path);

    let query = req.path.split_once('?').map(|(_, q)| q).unwrap_or("");
    let Some(raw_p) = query_param(query, "p") else {
        return error_json(400, "missing p query parameter");
    };
    let Ok(decoded) = percent_decode_str(raw_p).decode_utf8() else {
        return error_json(400, "invalid p query parameter");
    };
    let rel_path = Path::new(decoded.as_ref());
    if !is_plain_relative_path(rel_path) {
        return error_json(400, "invalid path");
    }

    let extension_allowed = rel_path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ASSET_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()));
    if !extension_allowed {
        return Reply::text(404, "404 Not Found");
    }

    let Ok(parent_canonical) = parent.canonicalize() else {
        return Reply::text(404, "404 Not Found");
    };
    let Ok(candidate_canonical) = parent.join(rel_path).canonicalize() else {
        return Reply::text(404, "404 Not Found");
    };
    if !candidate_canonical.starts_with(&parent_canonical) {
        return Reply::text(404, "404 Not Found");
    }

    let metadata = match fs::metadata(&candidate_canonical) {
        Ok(metadata) => metadata,
        Err(err) => {
            eprintln!("warning: failed to stat asset file: {err}");
            return Reply::text(404, "404 Not Found");
        }
    };
    if metadata.len() > ASSET_MAX_BYTES {
        return Reply::text(413, "413 Payload Too Large");
    }

    match fs::read(&candidate_canonical) {
        Ok(body) => Reply {
            status: 200,
            content_type: asset_content_type(rel_path),
            headers: Vec::new(),
            body,
        },
        Err(err) => {
            eprintln!("warning: failed to read asset file: {err}");
            Reply::text(404, "404 Not Found")
        }
    }
}

/// `true` if `path` is a non-empty relative reference with no `..`,
/// absolute root, drive prefix, or `.` component anywhere in it — see
/// [`handle_asset`].
fn is_plain_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

/// The `Content-Type` for an asset whose extension already passed
/// [`ASSET_EXTENSIONS`] — every arm here is one of those extensions, so
/// the fallback is unreachable in practice but kept rather than panicking
/// on a future extension added to the allowlist without a matching arm
/// here.
fn asset_content_type(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

/// Extracts `name`'s raw (still percent-encoded) value from a query
/// string — the substring of `req.path` after `?`, e.g. `p=img.png`.
/// `None` if `name` doesn't appear. Only the first occurrence is
/// returned; nothing in this module relies on a query string carrying the
/// same key twice.
fn query_param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then_some(value)
    })
}

fn no_file_open() -> Reply {
    error_json(409, "no file open")
}

/// The body shown in place of the document when it can't be read. `title`
/// is user-controlled (a file name), so it's escaped.
fn error_fragment(title: &str) -> String {
    format!(
        "<p class=\"mdview-error\">Failed to read {}</p>",
        escape_html_text(title)
    )
}

fn encode_title(title: &str) -> String {
    utf8_percent_encode(title, TITLE_HEADER_ENCODE_SET).to_string()
}

fn escape_html_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

fn handle_version(version: &AtomicU64) -> Reply {
    Reply::text(200, version.load(Ordering::SeqCst).to_string())
}

/// Reads and renders `path`, returning `(title, body_html)` on success or
/// just `title` (for the `500` body) on failure. The OS error itself is
/// logged to stderr, never returned to the caller.
fn read_and_render(path: &Path) -> Result<(String, String), String> {
    let title = file_title(path);
    match fs::read_to_string(path) {
        Ok(markdown) => Ok((title, to_html(&markdown, true))),
        Err(err) => {
            eprintln!("warning: failed to read {}: {err}", path.display());
            Err(title)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    fn header<'a>(reply: &'a Reply, name: &str) -> Option<&'a str> {
        reply
            .headers
            .iter()
            .find(|(header_name, _)| header_name.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// `handle`, but returning just the `Reply` half of its
    /// `(Reply, Action)` result and always passing `allow_open: false`,
    /// `root_dir: None` — what every test below that predates `PUT
    /// /open`/`Action`/`root_dir` (i.e. nearly all of them) wants, so they
    /// didn't have to be individually rewritten to thread extra arguments
    /// through and discard a tuple element they don't care about. `/tree`
    /// tests that use this still get the pre-`root_dir` fallback behavior
    /// (`asset_parent_dir(file)`), which is what they were written against.
    /// Tests that actually exercise `PUT /open`, `Action`, or a non-`None`
    /// `root_dir` call `handle` directly instead.
    fn handle_reply(
        req: &RouteRequest,
        file: Option<&Path>,
        version: &AtomicU64,
        allow_remote_images: bool,
    ) -> Reply {
        handle(req, file, version, allow_remote_images, false, None, None).0
    }

    /// A `GET` request with no headers/body — what nearly every test that
    /// isn't specifically about `PUT`/`POST` wants.
    fn get(path: &str) -> RouteRequest<'_> {
        RouteRequest {
            method: "GET",
            path,
            headers: &[],
            body: &[],
        }
    }

    const REQUEST_HEADER_PAIR: [(String, String); 0] = [];

    fn put_review<'a>(body: &'a [u8], headers: &'a [(String, String)]) -> RouteRequest<'a> {
        RouteRequest {
            method: "PUT",
            path: "/review",
            headers,
            body,
        }
    }

    fn post_export(headers: &[(String, String)]) -> RouteRequest<'_> {
        RouteRequest {
            method: "POST",
            path: "/export",
            headers,
            body: &[],
        }
    }

    fn put_open<'a>(body: &'a [u8], headers: &'a [(String, String)]) -> RouteRequest<'a> {
        RouteRequest {
            method: "PUT",
            path: "/open",
            headers,
            body,
        }
    }

    fn with_request_header() -> Vec<(String, String)> {
        vec![("X-Mdview-Request".to_string(), "1".to_string())]
    }

    #[test]
    fn root_renders_the_file_and_carries_common_headers() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hello\n").expect("write markdown file");
        let version = AtomicU64::new(3);

        let reply = handle_reply(&get("/"), Some(&file_path), &version, false);

        assert_eq!(reply.status, 200);
        assert_eq!(reply.content_type, "text/html; charset=utf-8");
        assert_eq!(header(&reply, "Cache-Control"), Some("no-store"));
        assert_eq!(header(&reply, "X-Content-Type-Options"), Some("nosniff"));
        assert_eq!(
            header(&reply, "Content-Security-Policy"),
            Some("frame-ancestors 'none'")
        );
        let body = String::from_utf8(reply.body).expect("utf8 body");
        assert!(body.contains("<h1>Hello</h1>"));
        assert!(body.contains("__mdviewVersion=\"3\""));
    }

    #[test]
    fn root_stamps_browser_mode_when_allow_open_is_false() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        let version = AtomicU64::new(0);

        let (reply, _action) = handle(
            &get("/"),
            Some(&file_path),
            &version,
            false,
            false,
            None,
            None,
        );

        let body = String::from_utf8(reply.body).expect("utf8 body");
        assert!(body.contains(r#"<body data-mode="browser">"#));
    }

    #[test]
    fn root_stamps_native_mode_when_allow_open_is_true() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        let version = AtomicU64::new(0);

        let (reply, _action) = handle(
            &get("/"),
            Some(&file_path),
            &version,
            false,
            true,
            None,
            None,
        );

        let body = String::from_utf8(reply.body).expect("utf8 body");
        assert!(body.contains(r#"<body data-mode="native">"#));
    }

    #[test]
    fn head_root_behaves_like_get() {
        let version = AtomicU64::new(0);
        let reply = handle_reply(
            &RouteRequest {
                method: "HEAD",
                path: "/",
                headers: &[],
                body: &[],
            },
            None,
            &version,
            false,
        );
        assert_eq!(reply.status, 200);
    }

    #[test]
    fn version_returns_the_counter_as_plain_text() {
        let version = AtomicU64::new(42);
        let reply = handle_reply(&get("/version"), None, &version, false);

        assert_eq!(reply.status, 200);
        assert_eq!(reply.content_type, "text/plain; charset=utf-8");
        assert_eq!(header(&reply, "Cache-Control"), Some("no-store"));
        assert_eq!(String::from_utf8(reply.body).unwrap(), "42");
    }

    #[test]
    fn body_route_returns_fragment_and_title_header() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("notes.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        let version = AtomicU64::new(0);

        let reply = handle_reply(&get("/body"), Some(&file_path), &version, false);

        assert_eq!(reply.status, 200);
        assert_eq!(reply.content_type, "text/html; charset=utf-8");
        assert_eq!(header(&reply, "X-Mdview-Title"), Some("notes.md"));
        let body = String::from_utf8(reply.body).unwrap();
        assert!(body.contains("<h1>Hi</h1>"));
        assert!(body.contains("class=\"blk\""));
        // /body is a fragment, not a full page: no <html>/<main> scaffolding.
        assert!(!body.contains("<main"));
    }

    #[test]
    fn no_file_selected_renders_empty_page_on_root_and_body() {
        let version = AtomicU64::new(0);

        let root = handle_reply(&get("/"), None, &version, false);
        assert_eq!(root.status, 200);
        assert!(String::from_utf8(root.body)
            .unwrap()
            .contains("Drop a Markdown file here"));

        let body = handle_reply(&get("/body"), None, &version, false);
        assert_eq!(body.status, 200);
        // No file means no file name to report.
        assert_eq!(header(&body, "X-Mdview-Title"), None);
        assert!(String::from_utf8(body.body)
            .unwrap()
            .contains("Drop a Markdown file here"));
    }

    #[test]
    fn unreadable_file_returns_500_page_with_live_script_and_no_path() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let missing_path = dir.path().join("gone.md");
        let version = AtomicU64::new(7);

        let root = handle_reply(&get("/"), Some(&missing_path), &version, false);
        assert_eq!(root.status, 500);
        assert_eq!(root.content_type, "text/html; charset=utf-8");
        let body = String::from_utf8(root.body).unwrap();
        assert!(body.contains("Failed to read gone.md"));
        assert!(!body.contains(missing_path.to_str().unwrap()));
        // The live-reload script must survive so the view recovers on its own.
        assert!(body.contains("/version"));
        assert!(body.contains("__mdviewVersion=\"7\""));
    }

    #[test]
    fn unreadable_file_body_route_is_200_error_fragment() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let missing_path = dir.path().join("gone.md");
        let version = AtomicU64::new(0);

        let reply = handle_reply(&get("/body"), Some(&missing_path), &version, false);
        assert_eq!(reply.status, 200);
        assert_eq!(header(&reply, "X-Mdview-Title"), Some("gone.md"));
        let body = String::from_utf8(reply.body).unwrap();
        assert!(body.contains("Failed to read gone.md"));
        assert!(!body.contains(missing_path.to_str().unwrap()));
    }

    #[test]
    fn non_ascii_title_is_percent_encoded_in_header() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("メモ.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        let version = AtomicU64::new(0);

        let reply = handle_reply(&get("/body"), Some(&file_path), &version, false);
        assert_eq!(reply.status, 200);
        let title = header(&reply, "X-Mdview-Title").expect("title header present");
        assert!(title.is_ascii(), "header must be ASCII-safe: {title}");
        assert!(title.starts_with("%E3%83%A1%E3%83%A2"), "{title}");
        assert!(title.ends_with(".md"), "{title}");

        std::fs::remove_file(&file_path).expect("delete file");
        let failed = handle_reply(&get("/body"), Some(&file_path), &version, false);
        let body = String::from_utf8(failed.body).unwrap();
        assert!(body.contains("メモ.md"), "{body}");
    }

    /// `<`/`>` are legal in Unix file names but not on Windows, so the
    /// HTML-escaping half of the title handling can only be exercised with
    /// a real file on Unix.
    #[cfg(unix)]
    #[test]
    fn angle_brackets_in_title_are_html_escaped_in_error_body() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("メモ<1>.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        let version = AtomicU64::new(0);

        let reply = handle_reply(&get("/body"), Some(&file_path), &version, false);
        let title = header(&reply, "X-Mdview-Title").expect("title header present");
        assert!(title.ends_with("<1>.md"), "{title}");

        std::fs::remove_file(&file_path).expect("delete file");
        let failed = handle_reply(&get("/body"), Some(&file_path), &version, false);
        let body = String::from_utf8(failed.body).unwrap();
        assert!(body.contains("メモ&lt;1&gt;.md"), "{body}");
    }

    #[test]
    fn every_route_carries_common_security_headers() {
        let version = AtomicU64::new(0);
        for route in ["/", "/version", "/body", "/nope"] {
            let reply = handle_reply(&get(route), None, &version, false);
            assert_eq!(header(&reply, "Cache-Control"), Some("no-store"), "{route}");
            assert_eq!(
                header(&reply, "X-Content-Type-Options"),
                Some("nosniff"),
                "{route}"
            );
            assert_eq!(
                header(&reply, "Content-Security-Policy"),
                Some("frame-ancestors 'none'"),
                "{route}"
            );
        }
    }

    #[test]
    fn query_string_is_ignored_for_routing() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        let version = AtomicU64::new(0);

        let reply = handle_reply(&get("/?x=1"), Some(&file_path), &version, false);
        assert_eq!(reply.status, 200);

        let version_reply = handle_reply(&get("/version?t=1"), Some(&file_path), &version, false);
        assert_eq!(version_reply.status, 200);
    }

    #[test]
    fn unknown_path_is_404() {
        let version = AtomicU64::new(0);
        let reply = handle_reply(&get("/nope"), None, &version, false);
        assert_eq!(reply.status, 404);
    }

    // -- /review, /export -----------------------------------------------

    #[test]
    fn get_review_without_a_sidecar_returns_an_empty_document() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("notes.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        let version = AtomicU64::new(0);

        let reply = handle_reply(&get("/review"), Some(&file_path), &version, false);
        assert_eq!(reply.status, 200);
        assert_eq!(reply.content_type, "application/json; charset=utf-8");
        let value: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(value["version"], 1);
        assert_eq!(value["file"], "notes.md");
        assert_eq!(value["blocks"], serde_json::json!([]));
        assert_eq!(value["file_comments"], serde_json::json!([]));
        assert_eq!(value["unanchored"], serde_json::json!([]));
    }

    #[test]
    fn get_review_recognizes_a_commented_list_item_as_anchored() {
        // A comment on an item/row hash must not show up in "unanchored"
        // just because it's absent from render::blocks (block-level
        // only) — GET /review's computation has to use render::anchors,
        // which includes nested item/row anchors too.
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("notes.md");
        let markdown = "- one\n- two\n";
        std::fs::write(&file_path, markdown).expect("write markdown file");
        let version = AtomicU64::new(0);

        let item_hash = render::anchors(markdown)
            .into_iter()
            .find(|a| a.kind == render::AnchorKind::Item)
            .expect("an item anchor exists")
            .hash;

        let headers = with_request_header();
        let doc_json = serde_json::json!({
            "version": 1,
            "file": "notes.md",
            "blocks": [{
                "hash": item_hash,
                "excerpt": "one",
                "kind": "item",
                "comments": [{
                    "id": "c_0123456789abcdef",
                    "text": "looks good",
                    "created": "2026-08-22T07:00:00Z",
                    "updated": "2026-08-22T07:00:00Z",
                }]
            }]
        });
        let body = serde_json::to_vec(&doc_json).unwrap();
        let put_reply = handle_reply(
            &put_review(&body, &headers),
            Some(&file_path),
            &version,
            false,
        );
        assert_eq!(put_reply.status, 200);

        let get_reply = handle_reply(&get("/review"), Some(&file_path), &version, false);
        let value: serde_json::Value = serde_json::from_slice(&get_reply.body).unwrap();
        assert_eq!(value["unanchored"], serde_json::json!([]));
        assert_eq!(value["blocks"][0]["kind"], "item");
    }

    #[test]
    fn get_review_with_no_file_open_is_409() {
        let version = AtomicU64::new(0);
        let reply = handle_reply(&get("/review"), None, &version, false);
        assert_eq!(reply.status, 409);
        let value: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(value["error"], "no file open");
    }

    #[test]
    fn get_review_when_document_is_unreadable_is_500() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let missing_path = dir.path().join("gone.md");
        let version = AtomicU64::new(0);

        let reply = handle_reply(&get("/review"), Some(&missing_path), &version, false);
        assert_eq!(reply.status, 500);
        let value: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(value["error"], "failed to read document");
        // Must not leak the absolute path.
        assert!(!String::from_utf8_lossy(&reply.body).contains(missing_path.to_str().unwrap()));
    }

    #[test]
    fn put_review_without_the_request_header_is_403() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("notes.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        let version = AtomicU64::new(0);

        let body = br#"{"version":1,"file":"notes.md","blocks":[]}"#;
        let reply = handle_reply(
            &put_review(body, &REQUEST_HEADER_PAIR),
            Some(&file_path),
            &version,
            false,
        );
        assert_eq!(reply.status, 403);
    }

    #[test]
    fn put_review_with_invalid_json_is_400() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("notes.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        let version = AtomicU64::new(0);
        let headers = with_request_header();

        let reply = handle_reply(
            &put_review(b"not json", &headers),
            Some(&file_path),
            &version,
            false,
        );
        assert_eq!(reply.status, 400);
        let value: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
        assert!(value["error"].is_string());
        // The error body must not leak the absolute path.
        assert!(!String::from_utf8_lossy(&reply.body).contains(file_path.to_str().unwrap()));
    }

    #[test]
    fn put_review_with_no_file_open_is_409() {
        let version = AtomicU64::new(0);
        let headers = with_request_header();
        let body = br#"{"version":1,"file":"notes.md","blocks":[]}"#;
        let reply = handle_reply(&put_review(body, &headers), None, &version, false);
        assert_eq!(reply.status, 409);
    }

    #[test]
    fn put_review_with_mismatched_file_name_is_400() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("notes.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        let version = AtomicU64::new(0);
        let headers = with_request_header();

        let body = br#"{"version":1,"file":"other.md","blocks":[]}"#;
        let reply = handle_reply(
            &put_review(body, &headers),
            Some(&file_path),
            &version,
            false,
        );
        assert_eq!(reply.status, 400);
        let value: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(value["error"], "document file name mismatch");
    }

    #[test]
    fn put_review_with_empty_file_name_is_400() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("notes.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        let version = AtomicU64::new(0);
        let headers = with_request_header();

        let body = br#"{"version":1,"file":"","blocks":[]}"#;
        let reply = handle_reply(
            &put_review(body, &headers),
            Some(&file_path),
            &version,
            false,
        );
        assert_eq!(reply.status, 400);
        let value: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(value["error"], "document file name mismatch");
    }

    #[test]
    fn put_review_with_matching_file_name_succeeds() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("notes.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        let version = AtomicU64::new(0);
        let headers = with_request_header();

        let body = br#"{"version":1,"file":"notes.md","blocks":[]}"#;
        let reply = handle_reply(
            &put_review(body, &headers),
            Some(&file_path),
            &version,
            false,
        );
        assert_eq!(reply.status, 200);
    }

    #[test]
    fn put_review_then_get_review_round_trips() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("notes.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        let blocks = render::blocks("# Hi\n");
        let hash = &blocks[0].hash;
        let version = AtomicU64::new(0);
        let headers = with_request_header();

        let doc_json = serde_json::json!({
            "version": 1,
            "file": "notes.md",
            "blocks": [{
                "hash": hash,
                "excerpt": "# Hi",
                "comments": [{
                    "id": "c_0123456789abcdef",
                    "text": "looks good",
                    "created": "2026-08-22T07:00:00Z",
                    "updated": "2026-08-22T07:00:00Z",
                }]
            }]
        });
        let body = serde_json::to_vec(&doc_json).unwrap();

        let put_reply = handle_reply(
            &put_review(&body, &headers),
            Some(&file_path),
            &version,
            false,
        );
        assert_eq!(put_reply.status, 200);
        let put_value: serde_json::Value = serde_json::from_slice(&put_reply.body).unwrap();
        assert_eq!(put_value["ok"], true);

        let get_reply = handle_reply(&get("/review"), Some(&file_path), &version, false);
        assert_eq!(get_reply.status, 200);
        let get_value: serde_json::Value = serde_json::from_slice(&get_reply.body).unwrap();
        assert_eq!(get_value["blocks"][0]["hash"], hash.as_str());
        assert_eq!(get_value["blocks"][0]["comments"][0]["text"], "looks good");
        assert_eq!(get_value["unanchored"], serde_json::json!([]));
    }

    #[test]
    fn put_review_then_get_review_round_trips_file_comments() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("notes.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        let version = AtomicU64::new(0);
        let headers = with_request_header();

        let doc_json = serde_json::json!({
            "version": 1,
            "file": "notes.md",
            "blocks": [],
            "file_comments": [{
                "id": "c_0123456789abcdef",
                "text": "全体として章立てが前後している",
                "created": "2026-08-22T07:00:00Z",
                "updated": "2026-08-22T07:00:00Z",
            }]
        });
        let body = serde_json::to_vec(&doc_json).unwrap();

        let put_reply = handle_reply(
            &put_review(&body, &headers),
            Some(&file_path),
            &version,
            false,
        );
        assert_eq!(put_reply.status, 200);

        let get_reply = handle_reply(&get("/review"), Some(&file_path), &version, false);
        assert_eq!(get_reply.status, 200);
        let get_value: serde_json::Value = serde_json::from_slice(&get_reply.body).unwrap();
        assert_eq!(
            get_value["file_comments"][0]["text"],
            "全体として章立てが前後している"
        );
        assert_eq!(get_value["file_comments"][0]["id"], "c_0123456789abcdef");
    }

    #[test]
    fn export_without_the_request_header_is_403() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("notes.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        let version = AtomicU64::new(0);

        let reply = handle_reply(
            &post_export(&REQUEST_HEADER_PAIR),
            Some(&file_path),
            &version,
            false,
        );
        assert_eq!(reply.status, 403);
    }

    #[test]
    fn export_with_no_file_open_is_409() {
        let version = AtomicU64::new(0);
        let headers = with_request_header();
        let reply = handle_reply(&post_export(&headers), None, &version, false);
        assert_eq!(reply.status, 409);
    }

    #[test]
    fn export_writes_the_file_and_returns_json() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("notes.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        let version = AtomicU64::new(0);
        let headers = with_request_header();

        let reply = handle_reply(&post_export(&headers), Some(&file_path), &version, false);
        assert_eq!(reply.status, 200);
        let value: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(value["path"], "notes.review.md");
        assert!(value["markdown"]
            .as_str()
            .unwrap()
            .starts_with("# Review: notes.md"));
        assert!(dir.path().join("notes.review.md").exists());
    }

    // -- /asset -----------------------------------------------------------

    #[test]
    fn asset_parent_dir_falls_back_to_current_dir_for_a_bare_relative_file_name() {
        // `Path::new("doc.md").parent()` is `Some("")`, not `None` and not
        // `.` — `"".canonicalize()` fails with `ENOENT`, which used to make
        // every `/asset` request 404 whenever markdown-remarkable was opened with a bare
        // relative file name (e.g. `markdown-remarkable doc.md` from that file's own
        // directory) instead of an absolute/directory-qualified path.
        assert_eq!(asset_parent_dir(Path::new("doc.md")), Path::new("."));
    }

    #[test]
    fn asset_parent_dir_uses_the_real_parent_when_there_is_one() {
        assert_eq!(
            asset_parent_dir(Path::new("/tmp/x/doc.md")),
            Path::new("/tmp/x")
        );
        assert_eq!(
            asset_parent_dir(Path::new("subdir/doc.md")),
            Path::new("subdir")
        );
    }

    #[test]
    fn asset_serves_a_local_image_next_to_the_document() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        std::fs::write(dir.path().join("img.png"), b"not-really-a-png").expect("write image");
        let version = AtomicU64::new(0);

        let reply = handle_reply(&get("/asset?p=img.png"), Some(&file_path), &version, false);

        assert_eq!(reply.status, 200);
        assert_eq!(reply.content_type, "image/png");
        assert_eq!(reply.body, b"not-really-a-png");
        assert_eq!(
            header(&reply, "Content-Security-Policy"),
            Some("default-src 'none'; sandbox")
        );
        assert_eq!(header(&reply, "Cache-Control"), Some("no-store"));
        assert_eq!(header(&reply, "X-Content-Type-Options"), Some("nosniff"));
        // The common `frame-ancestors 'none'` CSP every other route gets
        // must not also be present alongside asset's own stricter one.
        assert_eq!(
            reply
                .headers
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("Content-Security-Policy"))
                .count(),
            1
        );
    }

    #[test]
    fn asset_serves_an_image_in_a_subdirectory() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        std::fs::create_dir(dir.path().join("imgs")).expect("create subdir");
        std::fs::write(dir.path().join("imgs").join("a.png"), b"sub-image").expect("write image");
        let version = AtomicU64::new(0);

        let reply = handle_reply(
            &get("/asset?p=imgs/a.png"),
            Some(&file_path),
            &version,
            false,
        );

        assert_eq!(reply.status, 200);
        assert_eq!(reply.content_type, "image/png");
        assert_eq!(reply.body, b"sub-image");
    }

    #[test]
    fn asset_rejects_a_parent_directory_escape() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        let version = AtomicU64::new(0);

        let reply = handle_reply(
            &get("/asset?p=../secret.txt"),
            Some(&file_path),
            &version,
            false,
        );
        assert_eq!(reply.status, 400);
    }

    #[test]
    fn asset_rejects_an_absolute_path() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        let version = AtomicU64::new(0);

        let reply = handle_reply(
            &get("/asset?p=/etc/hosts"),
            Some(&file_path),
            &version,
            false,
        );
        assert_eq!(reply.status, 400);
    }

    #[test]
    fn asset_rejects_a_percent_encoded_parent_directory_escape() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        let version = AtomicU64::new(0);

        // `%2e%2e%2fsecret` decodes to `../secret` — the rejection has to
        // happen on the *decoded* path, not the raw query text.
        let reply = handle_reply(
            &get("/asset?p=%2e%2e%2fsecret"),
            Some(&file_path),
            &version,
            false,
        );
        assert_eq!(reply.status, 400);
    }

    #[test]
    fn asset_rejects_a_disallowed_extension() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        std::fs::write(dir.path().join("note.txt"), b"hello").expect("write file");
        let version = AtomicU64::new(0);

        let reply = handle_reply(&get("/asset?p=note.txt"), Some(&file_path), &version, false);
        assert_eq!(reply.status, 404);
    }

    #[test]
    fn asset_for_a_nonexistent_file_is_404() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        let version = AtomicU64::new(0);

        let reply = handle_reply(
            &get("/asset?p=missing.png"),
            Some(&file_path),
            &version,
            false,
        );
        assert_eq!(reply.status, 404);
    }

    #[test]
    fn asset_with_no_file_open_is_409() {
        let version = AtomicU64::new(0);
        let reply = handle_reply(&get("/asset?p=img.png"), None, &version, false);
        assert_eq!(reply.status, 409);
    }

    #[cfg(unix)]
    #[test]
    fn asset_rejects_a_symlink_that_escapes_the_document_directory() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");

        let outside = tempfile::tempdir().expect("create outside tempdir");
        let secret = outside.path().join("secret.png");
        std::fs::write(&secret, b"outside-bytes").expect("write outside file");

        let link_path = dir.path().join("escape.png");
        std::os::unix::fs::symlink(&secret, &link_path).expect("create symlink");

        let version = AtomicU64::new(0);
        let reply = handle_reply(
            &get("/asset?p=escape.png"),
            Some(&file_path),
            &version,
            false,
        );
        assert_eq!(reply.status, 404);
    }

    #[test]
    fn asset_over_the_size_limit_is_413() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        let big = vec![0u8; ASSET_MAX_BYTES as usize + 1];
        std::fs::write(dir.path().join("big.png"), &big).expect("write big image");
        let version = AtomicU64::new(0);

        let reply = handle_reply(&get("/asset?p=big.png"), Some(&file_path), &version, false);
        assert_eq!(reply.status, 413);
    }

    #[test]
    fn asset_content_type_covers_every_allowed_extension() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        let version = AtomicU64::new(0);

        let cases = [
            ("a.png", "image/png"),
            ("a.jpg", "image/jpeg"),
            ("a.jpeg", "image/jpeg"),
            ("a.gif", "image/gif"),
            ("a.webp", "image/webp"),
            ("a.bmp", "image/bmp"),
            ("a.svg", "image/svg+xml"),
            ("a.avif", "image/avif"),
            ("a.ico", "image/x-icon"),
        ];
        for (name, expected_type) in cases {
            std::fs::write(dir.path().join(name), b"data").expect("write image");
            let reply = handle_reply(
                &get(&format!("/asset?p={name}")),
                Some(&file_path),
                &version,
                false,
            );
            assert_eq!(reply.status, 200, "{name}");
            assert_eq!(reply.content_type, expected_type, "{name}");
        }
    }

    // -- /tree --------------------------------------------------------------

    #[test]
    fn tree_lists_files_and_dirs_dir_before_file_with_root_and_current() {
        let outer = tempfile::tempdir().expect("create tempdir");
        let root_dir = outer.path().join("imgdemo");
        std::fs::create_dir(&root_dir).expect("create root dir");
        let file_path = root_dir.join("doc.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        std::fs::create_dir(root_dir.join("sub")).expect("create sub dir");
        std::fs::write(root_dir.join("sub").join("a.md"), "# A\n")
            .expect("write nested markdown file");
        let version = AtomicU64::new(0);

        let reply = handle_reply(&get("/tree"), Some(&file_path), &version, false);
        assert_eq!(reply.status, 200);
        assert_eq!(reply.content_type, "application/json; charset=utf-8");
        let value: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(value["root"], "imgdemo");
        assert_eq!(value["current"], "doc.md");
        assert!(value.get("truncated").is_none());

        let entries = value["entries"].as_array().expect("entries array");
        assert_eq!(entries.len(), 3);
        // Each level lists directories before files: "sub" (dir) precedes
        // "doc.md" (file) even though "doc.md" sorts first alphabetically.
        // "sub"'s own contents (depth-first) immediately follow it.
        assert_eq!(entries[0]["path"], "sub");
        assert_eq!(entries[0]["name"], "sub");
        assert_eq!(entries[0]["kind"], "dir");
        assert_eq!(entries[1]["path"], "sub/a.md");
        assert_eq!(entries[1]["name"], "a.md");
        assert_eq!(entries[1]["kind"], "file");
        assert_eq!(entries[2]["path"], "doc.md");
        assert_eq!(entries[2]["name"], "doc.md");
        assert_eq!(entries[2]["kind"], "file");
    }

    #[test]
    fn tree_sorts_names_case_insensitively() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        std::fs::write(dir.path().join("B.md"), "# B\n").expect("write file");
        std::fs::write(dir.path().join("a.md"), "# a\n").expect("write file");
        std::fs::write(dir.path().join("C.md"), "# C\n").expect("write file");
        let version = AtomicU64::new(0);

        let reply = handle_reply(&get("/tree"), Some(&file_path), &version, false);
        let value: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
        let names: Vec<&str> = value["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["a.md", "B.md", "C.md", "doc.md"]);
    }

    #[test]
    fn tree_stops_descending_past_max_depth() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        let deep = dir.path().join("a").join("b").join("c").join("d");
        std::fs::create_dir_all(&deep).expect("create nested dirs");
        std::fs::write(deep.join("shallow.md"), "# Shallow\n").expect("write file");
        std::fs::create_dir(deep.join("e")).expect("create dir beyond max depth");
        std::fs::write(deep.join("e").join("deep.md"), "# Deep\n").expect("write file");
        let version = AtomicU64::new(0);

        let reply = handle_reply(&get("/tree"), Some(&file_path), &version, false);
        let value: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
        let paths: Vec<&str> = value["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["path"].as_str().unwrap())
            .collect();
        assert!(paths.contains(&"a/b/c/d/shallow.md"), "{paths:?}");
        assert!(
            !paths.iter().any(|path| path.contains("/e")),
            "directory beyond max depth must not appear: {paths:?}"
        );
    }

    #[test]
    fn tree_truncates_past_the_entry_cap() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        for i in 0..(TREE_MAX_ENTRIES + 5) {
            std::fs::write(dir.path().join(format!("f{i:05}.md")), "# x\n").expect("write file");
        }
        let version = AtomicU64::new(0);

        let reply = handle_reply(&get("/tree"), Some(&file_path), &version, false);
        let value: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(value["truncated"], true);
        assert_eq!(value["entries"].as_array().unwrap().len(), TREE_MAX_ENTRIES);
    }

    #[test]
    fn tree_skips_hidden_and_excluded_directories() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        for name in [".hidden", "node_modules", "target", ".git"] {
            let sub = dir.path().join(name);
            std::fs::create_dir(&sub).expect("create excluded dir");
            std::fs::write(sub.join("x.md"), "# x\n").expect("write file");
        }
        std::fs::create_dir(dir.path().join("visible")).expect("create visible dir");
        std::fs::write(dir.path().join("visible").join("x.md"), "# x\n").expect("write file");
        let version = AtomicU64::new(0);

        let reply = handle_reply(&get("/tree"), Some(&file_path), &version, false);
        let value: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
        let names: Vec<&str> = value["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"visible"), "{names:?}");
        for excluded in [".hidden", "node_modules", "target", ".git"] {
            assert!(!names.contains(&excluded), "{excluded} in {names:?}");
        }
    }

    #[test]
    fn tree_omits_a_folder_with_no_markdown_inside() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        std::fs::create_dir(dir.path().join("assets")).expect("create dir");
        std::fs::write(dir.path().join("assets").join("readme.txt"), "hi").expect("write file");
        let version = AtomicU64::new(0);

        let reply = handle_reply(&get("/tree"), Some(&file_path), &version, false);
        let value: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
        let names: Vec<&str> = value["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["doc.md"]);
    }

    #[cfg(unix)]
    #[test]
    fn tree_does_not_follow_a_symlinked_directory() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");

        let outside = tempfile::tempdir().expect("create outside tempdir");
        std::fs::write(outside.path().join("secret.md"), "# secret\n").expect("write outside file");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("linked"))
            .expect("create symlink");

        let version = AtomicU64::new(0);
        let reply = handle_reply(&get("/tree"), Some(&file_path), &version, false);
        let value: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
        let names: Vec<&str> = value["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["name"].as_str().unwrap())
            .collect();
        assert!(!names.contains(&"linked"), "{names:?}");
    }

    #[test]
    fn tree_with_no_file_open_is_409() {
        let version = AtomicU64::new(0);
        let reply = handle_reply(&get("/tree"), None, &version, false);
        assert_eq!(reply.status, 409);
    }

    // -- PUT /open ------------------------------------------------------

    #[test]
    fn open_in_browser_mode_is_501() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        std::fs::write(dir.path().join("b.md"), "# B\n").expect("write file");
        let version = AtomicU64::new(0);
        let headers = with_request_header();
        let body = br#"{"path":"b.md"}"#;

        // `allow_open: false` — what server.rs (`--browser`) always passes.
        let (reply, action) = handle(
            &put_open(body, &headers),
            Some(&file_path),
            &version,
            false,
            false,
            None,
            None,
        );
        assert_eq!(reply.status, 501);
        assert_eq!(action, Action::None);
    }

    #[test]
    fn open_without_the_request_header_is_403() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        let version = AtomicU64::new(0);
        let body = br#"{"path":"doc.md"}"#;

        let (reply, action) = handle(
            &put_open(body, &REQUEST_HEADER_PAIR),
            Some(&file_path),
            &version,
            false,
            true,
            None,
            None,
        );
        assert_eq!(reply.status, 403);
        assert_eq!(action, Action::None);
    }

    #[test]
    fn open_with_no_file_open_is_409() {
        let version = AtomicU64::new(0);
        let headers = with_request_header();
        let body = br#"{"path":"doc.md"}"#;

        let (reply, action) = handle(
            &put_open(body, &headers),
            None,
            &version,
            false,
            true,
            None,
            None,
        );
        assert_eq!(reply.status, 409);
        assert_eq!(action, Action::None);
    }

    #[test]
    fn open_with_invalid_json_is_400() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        let version = AtomicU64::new(0);
        let headers = with_request_header();

        let (reply, action) = handle(
            &put_open(b"not json", &headers),
            Some(&file_path),
            &version,
            false,
            true,
            None,
            None,
        );
        assert_eq!(reply.status, 400);
        assert_eq!(action, Action::None);
    }

    #[test]
    fn open_rejects_a_parent_directory_escape() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        let version = AtomicU64::new(0);
        let headers = with_request_header();
        let body = br#"{"path":"../secret.md"}"#;

        let (reply, action) = handle(
            &put_open(body, &headers),
            Some(&file_path),
            &version,
            false,
            true,
            None,
            None,
        );
        assert_eq!(reply.status, 400);
        assert_eq!(action, Action::None);
    }

    #[test]
    fn open_rejects_an_absolute_path() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        let version = AtomicU64::new(0);
        let headers = with_request_header();
        let body = br#"{"path":"/etc/hosts"}"#;

        let (reply, action) = handle(
            &put_open(body, &headers),
            Some(&file_path),
            &version,
            false,
            true,
            None,
            None,
        );
        assert_eq!(reply.status, 400);
        assert_eq!(action, Action::None);
    }

    #[test]
    fn open_rejects_a_non_markdown_extension() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        std::fs::write(dir.path().join("notes.txt"), "hi").expect("write file");
        let version = AtomicU64::new(0);
        let headers = with_request_header();
        let body = br#"{"path":"notes.txt"}"#;

        let (reply, action) = handle(
            &put_open(body, &headers),
            Some(&file_path),
            &version,
            false,
            true,
            None,
            None,
        );
        assert_eq!(reply.status, 400);
        assert_eq!(action, Action::None);
    }

    #[test]
    fn open_for_a_nonexistent_file_is_404() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        let version = AtomicU64::new(0);
        let headers = with_request_header();
        let body = br#"{"path":"missing.md"}"#;

        let (reply, action) = handle(
            &put_open(body, &headers),
            Some(&file_path),
            &version,
            false,
            true,
            None,
            None,
        );
        assert_eq!(reply.status, 404);
        assert_eq!(action, Action::None);
    }

    #[cfg(unix)]
    #[test]
    fn open_rejects_a_symlink_that_escapes_the_document_directory() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");

        let outside = tempfile::tempdir().expect("create outside tempdir");
        let secret = outside.path().join("secret.md");
        std::fs::write(&secret, "# secret\n").expect("write outside file");
        std::os::unix::fs::symlink(&secret, dir.path().join("escape.md")).expect("create symlink");

        let version = AtomicU64::new(0);
        let headers = with_request_header();
        let body = br#"{"path":"escape.md"}"#;

        let (reply, action) = handle(
            &put_open(body, &headers),
            Some(&file_path),
            &version,
            false,
            true,
            None,
            None,
        );
        assert_eq!(reply.status, 404);
        assert_eq!(action, Action::None);
    }

    #[cfg(unix)]
    #[test]
    fn open_rejects_a_symlink_target_even_when_it_resolves_inside_the_root() {
        // Advisory fix: `GET /tree` never lists a symlink at all
        // (`scan_tree_dir` skips every one outright), so `PUT /open`
        // shouldn't accept one via a hand-crafted `path` either — even one
        // that resolves to a perfectly in-bounds target. This is a
        // *stricter* check than the escape check above: an escaping
        // symlink is already caught by the canonicalize+prefix check, but
        // an in-bounds one wouldn't be without this dedicated
        // `symlink_metadata` check on the un-canonicalized candidate.
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        std::fs::write(dir.path().join("real.md"), "# Real\n").expect("write real target");
        std::os::unix::fs::symlink(dir.path().join("real.md"), dir.path().join("link.md"))
            .expect("create symlink");

        let version = AtomicU64::new(0);
        let headers = with_request_header();
        let (reply, action) = handle(
            &put_open(br#"{"path":"link.md"}"#, &headers),
            Some(&file_path),
            &version,
            false,
            true,
            None,
            None,
        );
        assert_eq!(reply.status, 404);
        assert_eq!(action, Action::None);
    }

    #[test]
    fn open_rejects_a_directory_target() {
        // Advisory fix: a directory (or, in principle, a FIFO/device node)
        // named with a `.md` extension must not be accepted — switching to
        // it would later hang the WebView's protocol-handler thread trying
        // to `fs::read_to_string` something that isn't a regular file.
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        std::fs::create_dir(dir.path().join("looks-like-a-file.md")).expect("create dir");

        let version = AtomicU64::new(0);
        let headers = with_request_header();
        let (reply, action) = handle(
            &put_open(br#"{"path":"looks-like-a-file.md"}"#, &headers),
            Some(&file_path),
            &version,
            false,
            true,
            None,
            None,
        );
        assert_eq!(reply.status, 404);
        assert_eq!(action, Action::None);
    }

    #[test]
    fn open_succeeds_and_returns_an_open_file_action() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        let target = dir.path().join("b.md");
        std::fs::write(&target, "# B\n").expect("write file");
        let version = AtomicU64::new(0);
        let headers = with_request_header();
        let body = br#"{"path":"b.md"}"#;

        let (reply, action) = handle(
            &put_open(body, &headers),
            Some(&file_path),
            &version,
            false,
            true,
            None,
            None,
        );
        assert_eq!(reply.status, 200);
        let value: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(value["reloaded"], true);
        assert_eq!(
            action,
            Action::OpenFile(target.canonicalize().expect("canonicalize target"))
        );
    }

    #[test]
    fn open_succeeds_for_a_file_in_a_subdirectory() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        std::fs::create_dir(dir.path().join("sub")).expect("create sub dir");
        let target = dir.path().join("sub").join("a.md");
        std::fs::write(&target, "# A\n").expect("write file");
        let version = AtomicU64::new(0);
        let headers = with_request_header();
        let body = br#"{"path":"sub/a.md"}"#;

        let (reply, action) = handle(
            &put_open(body, &headers),
            Some(&file_path),
            &version,
            false,
            true,
            None,
            None,
        );
        assert_eq!(reply.status, 200);
        assert!(value_reloaded(&reply));
        assert_eq!(
            action,
            Action::OpenFile(target.canonicalize().expect("canonicalize target"))
        );
    }

    fn value_reloaded(reply: &Reply) -> bool {
        let value: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
        value["reloaded"].as_bool().unwrap_or(false)
    }

    // -- GET/PUT /nav -----------------------------------------------------

    fn put_nav<'a>(body: &'a [u8], headers: &'a [(String, String)]) -> RouteRequest<'a> {
        RouteRequest {
            method: "PUT",
            path: "/nav",
            headers,
            body,
        }
    }

    #[test]
    fn nav_get_with_no_history_reports_false_false() {
        let version = AtomicU64::new(0);
        let (reply, action) = handle(&get("/nav"), None, &version, false, true, None, None);
        assert_eq!(reply.status, 200);
        assert_eq!(action, Action::None);
        let value: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(value["back"], false);
        assert_eq!(value["forward"], false);
    }

    #[test]
    fn nav_get_reports_the_given_snapshot() {
        let version = AtomicU64::new(0);
        let (reply, _action) = handle(
            &get("/nav"),
            None,
            &version,
            false,
            true,
            None,
            Some((true, false)),
        );
        let value: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(value["back"], true);
        assert_eq!(value["forward"], false);
    }

    #[test]
    fn nav_get_in_browser_mode_is_always_false_false_even_with_a_snapshot() {
        // `allow_open: false` (server.rs/`--browser`) always answers
        // false/false, regardless of what `nav` says — there's no
        // per-window history under `--browser` at all.
        let version = AtomicU64::new(0);
        let (reply, _action) = handle(
            &get("/nav"),
            None,
            &version,
            false,
            false,
            None,
            Some((true, true)),
        );
        let value: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(value["back"], false);
        assert_eq!(value["forward"], false);
    }

    #[test]
    fn nav_put_in_browser_mode_is_501() {
        let version = AtomicU64::new(0);
        let headers = with_request_header();
        let body = br#"{"dir":"back"}"#;
        let (reply, action) = handle(
            &put_nav(body, &headers),
            None,
            &version,
            false,
            false,
            None,
            Some((true, true)),
        );
        assert_eq!(reply.status, 501);
        assert_eq!(action, Action::None);
    }

    #[test]
    fn nav_put_without_the_request_header_is_403() {
        let version = AtomicU64::new(0);
        let body = br#"{"dir":"back"}"#;
        let (reply, action) = handle(
            &put_nav(body, &REQUEST_HEADER_PAIR),
            None,
            &version,
            false,
            true,
            None,
            Some((true, true)),
        );
        assert_eq!(reply.status, 403);
        assert_eq!(action, Action::None);
    }

    #[test]
    fn nav_put_with_no_history_is_409() {
        let version = AtomicU64::new(0);
        let headers = with_request_header();
        let body = br#"{"dir":"back"}"#;
        let (reply, action) = handle(
            &put_nav(body, &headers),
            None,
            &version,
            false,
            true,
            None,
            None,
        );
        assert_eq!(reply.status, 409);
        assert_eq!(action, Action::None);
    }

    #[test]
    fn nav_put_with_invalid_json_is_400() {
        let version = AtomicU64::new(0);
        let headers = with_request_header();
        let (reply, action) = handle(
            &put_nav(b"not json", &headers),
            None,
            &version,
            false,
            true,
            None,
            Some((true, true)),
        );
        assert_eq!(reply.status, 400);
        assert_eq!(action, Action::None);
    }

    #[test]
    fn nav_put_with_an_invalid_direction_is_400() {
        let version = AtomicU64::new(0);
        let headers = with_request_header();
        let body = br#"{"dir":"sideways"}"#;
        let (reply, action) = handle(
            &put_nav(body, &headers),
            None,
            &version,
            false,
            true,
            None,
            Some((true, true)),
        );
        assert_eq!(reply.status, 400);
        assert_eq!(action, Action::None);
    }

    #[test]
    fn nav_put_back_when_unavailable_is_409() {
        let version = AtomicU64::new(0);
        let headers = with_request_header();
        let body = br#"{"dir":"back"}"#;
        let (reply, action) = handle(
            &put_nav(body, &headers),
            None,
            &version,
            false,
            true,
            None,
            Some((false, true)),
        );
        assert_eq!(reply.status, 409);
        assert_eq!(action, Action::None);
    }

    #[test]
    fn nav_put_forward_when_unavailable_is_409() {
        let version = AtomicU64::new(0);
        let headers = with_request_header();
        let body = br#"{"dir":"forward"}"#;
        let (reply, action) = handle(
            &put_nav(body, &headers),
            None,
            &version,
            false,
            true,
            None,
            Some((true, false)),
        );
        assert_eq!(reply.status, 409);
        assert_eq!(action, Action::None);
    }

    #[test]
    fn nav_put_back_succeeds_and_returns_a_navigate_action() {
        let version = AtomicU64::new(0);
        let headers = with_request_header();
        let body = br#"{"dir":"back"}"#;
        let (reply, action) = handle(
            &put_nav(body, &headers),
            None,
            &version,
            false,
            true,
            None,
            Some((true, false)),
        );
        assert_eq!(reply.status, 200);
        assert!(value_reloaded(&reply));
        assert_eq!(action, Action::Navigate(NavDirection::Back));
    }

    #[test]
    fn nav_put_forward_succeeds_and_returns_a_navigate_action() {
        let version = AtomicU64::new(0);
        let headers = with_request_header();
        let body = br#"{"dir":"forward"}"#;
        let (reply, action) = handle(
            &put_nav(body, &headers),
            None,
            &version,
            false,
            true,
            None,
            Some((false, true)),
        );
        assert_eq!(reply.status, 200);
        assert!(value_reloaded(&reply));
        assert_eq!(action, Action::Navigate(NavDirection::Forward));
    }

    // -- root_dir: the window's fixed tree/switch root -------------------

    #[test]
    fn open_can_switch_back_to_a_file_in_the_fixed_window_root_after_descending_into_a_subdirectory(
    ) {
        // Regression test for the file-tree root-dir fix: before it, both
        // `/tree` and `PUT /open` used `asset_parent_dir(file)` — the
        // *current* file's own parent — as their scope. Switching from
        // `a.md` into `sub/c.md` would then silently move that scope to
        // `root/sub`, and switching back to `a.md` (which lives in `root`,
        // not `root/sub`) would 404 — defeating the tree's whole point of
        // moving freely between sibling files. Passing the *same*
        // `root_dir` on every call (as `app.rs`'s `WindowCtx::root_dir`
        // now does — fixed at the window's first file, never touched by a
        // later switch) is what fixes this.
        let dir = tempfile::tempdir().expect("create tempdir");
        let root_dir = dir.path().canonicalize().expect("canonicalize root dir");
        let a_path = dir.path().join("a.md");
        std::fs::write(&a_path, "# A\n").expect("write a.md");
        std::fs::create_dir(dir.path().join("sub")).expect("create sub dir");
        let sub_c_path = dir.path().join("sub").join("c.md");
        std::fs::write(&sub_c_path, "# C\n").expect("write sub/c.md");
        let version = AtomicU64::new(0);
        let headers = with_request_header();

        let (reply1, action1) = handle(
            &put_open(br#"{"path":"sub/c.md"}"#, &headers),
            Some(&a_path),
            &version,
            false,
            true,
            Some(&root_dir),
            None,
        );
        assert_eq!(reply1.status, 200);
        let sub_c_canonical = sub_c_path.canonicalize().expect("canonicalize sub/c.md");
        assert_eq!(action1, Action::OpenFile(sub_c_canonical.clone()));

        // Now "in" sub/c.md — switch back to a.md, against the *same*
        // `root_dir` (never `asset_parent_dir(sub/c.md)`, which would be
        // `root/sub`, under which "a.md" doesn't exist).
        let (reply2, action2) = handle(
            &put_open(br#"{"path":"a.md"}"#, &headers),
            Some(&sub_c_canonical),
            &version,
            false,
            true,
            Some(&root_dir),
            None,
        );
        assert_eq!(reply2.status, 200, "must be able to switch back to a.md");
        assert_eq!(
            action2,
            Action::OpenFile(a_path.canonicalize().expect("canonicalize a.md"))
        );

        // Root stays inside root_dir; escaping it (`../`) is still 404
        // regardless of which file is currently open.
        let (reply3, action3) = handle(
            &put_open(br#"{"path":"../secret.md"}"#, &headers),
            Some(&sub_c_canonical),
            &version,
            false,
            true,
            Some(&root_dir),
            None,
        );
        assert_eq!(reply3.status, 400);
        assert_eq!(action3, Action::None);
    }

    #[test]
    fn open_without_a_fixed_root_dir_cannot_switch_back_out_of_a_subdirectory() {
        // Contrast with the fixed-root test above: `root_dir: None` falls
        // back to `asset_parent_dir(file)` — the pre-fix behavior — which
        // breaks switching back out of a subdirectory. Pinned here so a
        // future change to that fallback is deliberate.
        let dir = tempfile::tempdir().expect("create tempdir");
        std::fs::write(dir.path().join("a.md"), "# A\n").expect("write a.md");
        std::fs::create_dir(dir.path().join("sub")).expect("create sub dir");
        let sub_c_path = dir.path().join("sub").join("c.md");
        std::fs::write(&sub_c_path, "# C\n").expect("write sub/c.md");
        let sub_c_canonical = sub_c_path.canonicalize().expect("canonicalize sub/c.md");
        let version = AtomicU64::new(0);
        let headers = with_request_header();

        let (reply, action) = handle(
            &put_open(br#"{"path":"a.md"}"#, &headers),
            Some(&sub_c_canonical),
            &version,
            false,
            true,
            None,
            None,
        );
        assert_eq!(reply.status, 404);
        assert_eq!(action, Action::None);
    }

    #[test]
    fn tree_root_and_current_reflect_the_fixed_root_not_the_current_files_own_parent() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let root_name = dir
            .path()
            .file_name()
            .expect("tempdir has a name")
            .to_string_lossy()
            .into_owned();
        std::fs::write(dir.path().join("a.md"), "# A\n").expect("write a.md");
        std::fs::create_dir(dir.path().join("sub")).expect("create sub dir");
        let sub_c_path = dir.path().join("sub").join("c.md");
        std::fs::write(&sub_c_path, "# C\n").expect("write sub/c.md");
        let root_dir = dir.path().canonicalize().expect("canonicalize root dir");
        let sub_c_canonical = sub_c_path.canonicalize().expect("canonicalize sub/c.md");
        let version = AtomicU64::new(0);

        // As if the window's fixed root is `dir` (established from a.md)
        // but the *current* file is now `sub/c.md` (after a tree switch) —
        // without `root_dir`, this would use `asset_parent_dir(sub/c.md)`
        // = `dir/sub` instead, breaking both `root` and `current`.
        let (reply, _action) = handle(
            &get("/tree"),
            Some(&sub_c_canonical),
            &version,
            false,
            true,
            Some(&root_dir),
            None,
        );
        assert_eq!(reply.status, 200);
        let value: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(value["root"], root_name);
        assert_eq!(value["current"], "sub/c.md");
        let names: Vec<&str> = value["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"a.md"), "{names:?}");
    }

    #[test]
    fn tree_relative_path_falls_back_to_empty_string_without_leaking_a_path() {
        // `Path::file_name()` is `None` for a path with no proper
        // file-name component (`..`, `/`, ...) — `file_title`'s own
        // fallback would leak the full path there; this must not.
        assert_eq!(
            tree_relative_path(Path::new(".."), Path::new("/somewhere/else")),
            ""
        );
    }

    // -- PUT /review: residual cross-window risk (documented, not fixed
    //    at this layer) ----------------------------------------------------

    #[test]
    fn put_review_matches_only_by_basename_not_by_directory() {
        // Characterizes a known, accepted residual risk around switching
        // files mid-session with the same basename but a different
        // directory (see docs/SECURITY.md): `doc.file` only ever
        // travels the wire as a bare basename (see `handle_get_review`/
        // `handle_put_review`), so `handle()` alone can never distinguish
        // `a/README.md` from `b/README.md` by that basename. What actually
        // prevents a stale, in-flight PUT from an old page reaching this
        // handler with a *newly switched* `file` is `app.rs`'s
        // `open_file` calling `webview.load_url` (discarding the old page)
        // *before* swapping the served file, not a check in this module.
        // Pinned here so a future change to the basename-only comparison
        // is deliberate.
        let dir = tempfile::tempdir().expect("create tempdir");
        let readme = dir.path().join("README.md");
        std::fs::write(&readme, "# Hi\n").expect("write markdown file");
        let version = AtomicU64::new(0);
        let headers = with_request_header();

        let body = br#"{"version":1,"file":"README.md","blocks":[]}"#;
        let reply = handle_reply(&put_review(body, &headers), Some(&readme), &version, false);
        assert_eq!(
            reply.status, 200,
            "handle() alone can't tell same-named files in different directories apart"
        );
    }

    // -- scan_tree_dir budget/visited edge cases --------------------------

    #[test]
    fn tree_keeps_already_collected_entries_when_the_cap_is_hit_inside_a_subdirectory() {
        // Regression test: hitting the entries cap while about to add a
        // *subdirectory's own* row used to discard everything already
        // collected inside it, not just the row that didn't fit.
        let dir = tempfile::tempdir().expect("create tempdir");
        std::fs::create_dir(dir.path().join("sub")).expect("create sub dir");
        for i in 0..5 {
            std::fs::write(dir.path().join("sub").join(format!("f{i}.md")), "# x\n")
                .expect("write file");
        }
        let (entries, truncated) = collect_tree_entries(dir.path(), 2, 1_000);
        assert!(truncated);
        assert_eq!(
            entries.len(),
            2,
            "already-collected entries must survive truncation instead of being discarded"
        );
    }

    #[test]
    fn tree_does_not_truncate_when_entries_exactly_fill_the_budget() {
        let dir = tempfile::tempdir().expect("create tempdir");
        for i in 0..4 {
            std::fs::write(dir.path().join(format!("f{i}.md")), "# x\n").expect("write file");
        }
        let (entries, truncated) = collect_tree_entries(dir.path(), 4, 1_000);
        assert!(
            !truncated,
            "exactly filling the budget must not falsely truncate"
        );
        assert_eq!(entries.len(), 4);
    }

    #[test]
    fn tree_does_not_truncate_when_a_subdirectory_and_its_file_exactly_fill_the_budget() {
        let dir = tempfile::tempdir().expect("create tempdir");
        std::fs::create_dir(dir.path().join("sub")).expect("create sub dir");
        std::fs::write(dir.path().join("sub").join("c.md"), "# C\n").expect("write file");
        // budget=2: exactly enough for "sub" (dir row) + "sub/c.md" (file row).
        let (entries, truncated) = collect_tree_entries(dir.path(), 2, 1_000);
        assert!(!truncated);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind, "dir");
        assert_eq!(entries[1].kind, "file");
    }

    #[test]
    fn tree_stops_after_visiting_too_many_entries_even_if_none_qualify() {
        let dir = tempfile::tempdir().expect("create tempdir");
        std::fs::write(dir.path().join("doc.md"), "# Hi\n").expect("write markdown file");
        for i in 0..10 {
            std::fs::write(dir.path().join(format!("noise{i}.txt")), b"x")
                .expect("write noise file");
        }
        let (entries, truncated) = collect_tree_entries(dir.path(), 1_000, 5);
        assert!(truncated);
        // Whatever *was* found (doc.md, if visited before the cap) is
        // kept; the walk just stops looking at more entries once too many
        // have been examined, regardless of whether they qualified.
        assert!(entries.len() <= 1);
    }

    // Linux only: macOS's filesystems (APFS/HFS+) reject a non-UTF-8 file
    // name outright at creation time (`Illegal byte sequence`), so there's
    // no way to even construct this scenario there — unlike Linux, which
    // treats file names as arbitrary byte sequences and happily creates
    // one, which is exactly the case `scan_tree_dir`'s `to_str()` check
    // guards against.
    #[cfg(target_os = "linux")]
    #[test]
    fn tree_skips_a_non_utf8_file_name() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().expect("create tempdir");
        std::fs::write(dir.path().join("a.md"), "# A\n").expect("write a.md");
        let bad_name = OsStr::from_bytes(b"bad-\xffname.md");
        std::fs::write(dir.path().join(bad_name), "# Bad\n").expect("write non-utf8-named file");

        let (entries, truncated) = collect_tree_entries(dir.path(), 1_000, 1_000);
        assert!(!truncated);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "a.md");
    }
}

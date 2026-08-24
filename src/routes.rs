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
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use std::fs;
use std::path::Path;
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
const EMPTY_TITLE: &str = "mdview";
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
/// - anything else — `404`.
///
/// `GET`/`PUT /review` and `POST /export` all answer `409` when `file` is
/// `None` — there's nothing to review yet.
///
/// A read failure (file deleted, permissions changed, etc.) never leaks the
/// absolute path or OS error text (those go to stderr, same as every other
/// error body in this module). `/` answers `500` but still with a full
/// `page()` — live-reload script included — so the view recovers on its
/// own once the file is readable again; `/body` answers `200` with an
/// error fragment for the same reason (the script treats a non-200 `/body`
/// as "reload the whole page").
pub fn handle(req: &RouteRequest, file: Option<&Path>, version: &AtomicU64) -> Reply {
    let route = req.path.split('?').next().unwrap_or("/");
    let reply = match (req.method, route) {
        ("GET", "/") | ("HEAD", "/") => handle_root(file, version),
        ("GET", "/version") | ("HEAD", "/version") => handle_version(version),
        ("GET", "/body") | ("HEAD", "/body") => handle_body(file),
        ("GET", "/review") => handle_get_review(file),
        ("PUT", "/review") => handle_put_review(req, file),
        ("POST", "/export") => handle_export(req, file),
        _ => Reply::text(404, "404 Not Found"),
    };
    reply
        .with_header("Cache-Control", "no-store")
        .with_header("X-Content-Type-Options", "nosniff")
        .with_header("Content-Security-Policy", "frame-ancestors 'none'")
}

fn handle_root(file: Option<&Path>, version: &AtomicU64) -> Reply {
    // Read the live-reload baseline *before* reading the file: if a save
    // lands in between, the version embedded here is guaranteed to be no
    // newer than the content about to be rendered, so the client's first
    // comparison can't spuriously miss that save. See `render::page`'s docs.
    let baseline = version.load(Ordering::SeqCst);
    match file {
        None => Reply::html(200, page(EMPTY_TITLE, EMPTY_BODY_HTML, Some(baseline))),
        Some(path) => match read_and_render(path) {
            Ok((title, body_html)) => Reply::html(200, page(&title, &body_html, Some(baseline))),
            // Still a full page (live script included) so the view can
            // recover by itself once the file is back — a bare 500 would
            // leave the native window with no way to refresh.
            Err(title) => Reply::html(500, page(&title, &error_fragment(&title), Some(baseline))),
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
        eprintln!(
            "warning: PUT /review body names a different file ({:?}) than is open",
            doc.file
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
        Ok(markdown) => Ok((title, to_html(&markdown))),
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

    fn with_request_header() -> Vec<(String, String)> {
        vec![("X-Mdview-Request".to_string(), "1".to_string())]
    }

    #[test]
    fn root_renders_the_file_and_carries_common_headers() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hello\n").expect("write markdown file");
        let version = AtomicU64::new(3);

        let reply = handle(&get("/"), Some(&file_path), &version);

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
    fn head_root_behaves_like_get() {
        let version = AtomicU64::new(0);
        let reply = handle(
            &RouteRequest {
                method: "HEAD",
                path: "/",
                headers: &[],
                body: &[],
            },
            None,
            &version,
        );
        assert_eq!(reply.status, 200);
    }

    #[test]
    fn version_returns_the_counter_as_plain_text() {
        let version = AtomicU64::new(42);
        let reply = handle(&get("/version"), None, &version);

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

        let reply = handle(&get("/body"), Some(&file_path), &version);

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

        let root = handle(&get("/"), None, &version);
        assert_eq!(root.status, 200);
        assert!(String::from_utf8(root.body)
            .unwrap()
            .contains("Drop a Markdown file here"));

        let body = handle(&get("/body"), None, &version);
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

        let root = handle(&get("/"), Some(&missing_path), &version);
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

        let reply = handle(&get("/body"), Some(&missing_path), &version);
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

        let reply = handle(&get("/body"), Some(&file_path), &version);
        assert_eq!(reply.status, 200);
        let title = header(&reply, "X-Mdview-Title").expect("title header present");
        assert!(title.is_ascii(), "header must be ASCII-safe: {title}");
        assert!(title.starts_with("%E3%83%A1%E3%83%A2"), "{title}");
        assert!(title.ends_with(".md"), "{title}");

        std::fs::remove_file(&file_path).expect("delete file");
        let failed = handle(&get("/body"), Some(&file_path), &version);
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

        let reply = handle(&get("/body"), Some(&file_path), &version);
        let title = header(&reply, "X-Mdview-Title").expect("title header present");
        assert!(title.ends_with("<1>.md"), "{title}");

        std::fs::remove_file(&file_path).expect("delete file");
        let failed = handle(&get("/body"), Some(&file_path), &version);
        let body = String::from_utf8(failed.body).unwrap();
        assert!(body.contains("メモ&lt;1&gt;.md"), "{body}");
    }

    #[test]
    fn every_route_carries_common_security_headers() {
        let version = AtomicU64::new(0);
        for route in ["/", "/version", "/body", "/nope"] {
            let reply = handle(&get(route), None, &version);
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

        let reply = handle(&get("/?x=1"), Some(&file_path), &version);
        assert_eq!(reply.status, 200);

        let version_reply = handle(&get("/version?t=1"), Some(&file_path), &version);
        assert_eq!(version_reply.status, 200);
    }

    #[test]
    fn unknown_path_is_404() {
        let version = AtomicU64::new(0);
        let reply = handle(&get("/nope"), None, &version);
        assert_eq!(reply.status, 404);
    }

    // -- /review, /export -----------------------------------------------

    #[test]
    fn get_review_without_a_sidecar_returns_an_empty_document() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("notes.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        let version = AtomicU64::new(0);

        let reply = handle(&get("/review"), Some(&file_path), &version);
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
        let put_reply = handle(&put_review(&body, &headers), Some(&file_path), &version);
        assert_eq!(put_reply.status, 200);

        let get_reply = handle(&get("/review"), Some(&file_path), &version);
        let value: serde_json::Value = serde_json::from_slice(&get_reply.body).unwrap();
        assert_eq!(value["unanchored"], serde_json::json!([]));
        assert_eq!(value["blocks"][0]["kind"], "item");
    }

    #[test]
    fn get_review_with_no_file_open_is_409() {
        let version = AtomicU64::new(0);
        let reply = handle(&get("/review"), None, &version);
        assert_eq!(reply.status, 409);
        let value: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(value["error"], "no file open");
    }

    #[test]
    fn get_review_when_document_is_unreadable_is_500() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let missing_path = dir.path().join("gone.md");
        let version = AtomicU64::new(0);

        let reply = handle(&get("/review"), Some(&missing_path), &version);
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
        let reply = handle(
            &put_review(body, &REQUEST_HEADER_PAIR),
            Some(&file_path),
            &version,
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

        let reply = handle(
            &put_review(b"not json", &headers),
            Some(&file_path),
            &version,
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
        let reply = handle(&put_review(body, &headers), None, &version);
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
        let reply = handle(&put_review(body, &headers), Some(&file_path), &version);
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
        let reply = handle(&put_review(body, &headers), Some(&file_path), &version);
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
        let reply = handle(&put_review(body, &headers), Some(&file_path), &version);
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

        let put_reply = handle(&put_review(&body, &headers), Some(&file_path), &version);
        assert_eq!(put_reply.status, 200);
        let put_value: serde_json::Value = serde_json::from_slice(&put_reply.body).unwrap();
        assert_eq!(put_value["ok"], true);

        let get_reply = handle(&get("/review"), Some(&file_path), &version);
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

        let put_reply = handle(&put_review(&body, &headers), Some(&file_path), &version);
        assert_eq!(put_reply.status, 200);

        let get_reply = handle(&get("/review"), Some(&file_path), &version);
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

        let reply = handle(
            &post_export(&REQUEST_HEADER_PAIR),
            Some(&file_path),
            &version,
        );
        assert_eq!(reply.status, 403);
    }

    #[test]
    fn export_with_no_file_open_is_409() {
        let version = AtomicU64::new(0);
        let headers = with_request_header();
        let reply = handle(&post_export(&headers), None, &version);
        assert_eq!(reply.status, 409);
    }

    #[test]
    fn export_writes_the_file_and_returns_json() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("notes.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        let version = AtomicU64::new(0);
        let headers = with_request_header();

        let reply = handle(&post_export(&headers), Some(&file_path), &version);
        assert_eq!(reply.status, 200);
        let value: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(value["path"], "notes.review.md");
        assert!(value["markdown"]
            .as_str()
            .unwrap()
            .starts_with("# Review: notes.md"));
        assert!(dir.path().join("notes.review.md").exists());
    }
}

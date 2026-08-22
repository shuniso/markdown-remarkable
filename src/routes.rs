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

use crate::render::{page, to_html};
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

    fn with_header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }
}

/// Routes `path` to a response, given the currently-viewed `file` (`None` if
/// nothing has been opened yet) and the live-reload `version` counter.
///
/// A query string on `path` (`/?x=1`) is ignored for routing purposes, same
/// as the original `server::run` behavior. Every response carries
/// `Cache-Control: no-store`, `X-Content-Type-Options: nosniff`, and
/// `Content-Security-Policy: frame-ancestors 'none'` (an HTTP-level
/// belt-and-suspenders alongside the `<meta>` tag `page()` already emits —
/// applied to every route because `/body` is `text/html` too and has no
/// `<meta>` of its own).
///
/// Routes:
/// - `GET /` — the full page (`render::page`), live-reload baseline
///   embedded. `file: None` renders the empty-state placeholder instead of
///   reading anything.
/// - `GET /version` — the current `version`, as plain text.
/// - `GET /body` — just the rendered body fragment (`text/html`), with a
///   percent-encoded `X-Mdview-Title: <file name>` header when `file` is
///   `Some`. Used by the live-reload script to swap `<main>`'s contents in
///   place instead of a full page reload.
/// - anything else — `404`.
///
/// A read failure (file deleted, permissions changed, etc.) never leaks the
/// absolute path or OS error text (those go to stderr). `/` answers `500`
/// but still with a full `page()` — live-reload script included — so the
/// view recovers on its own once the file is readable again; `/body`
/// answers `200` with an error fragment for the same reason (the script
/// treats a non-200 `/body` as "reload the whole page").
pub fn handle(path: &str, file: Option<&Path>, version: &AtomicU64) -> Reply {
    let route = path.split('?').next().unwrap_or("/");
    let reply = match route {
        "/" => handle_root(file, version),
        "/version" => handle_version(version),
        "/body" => handle_body(file),
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

/// The display title for `path`: its file name, or the full path if it
/// somehow has none. Shared with `app.rs`, which uses the same rule for the
/// native window's title bar.
pub(crate) fn file_title(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
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

    #[test]
    fn root_renders_the_file_and_carries_common_headers() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("doc.md");
        std::fs::write(&file_path, "# Hello\n").expect("write markdown file");
        let version = AtomicU64::new(3);

        let reply = handle("/", Some(&file_path), &version);

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
    fn version_returns_the_counter_as_plain_text() {
        let version = AtomicU64::new(42);
        let reply = handle("/version", None, &version);

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

        let reply = handle("/body", Some(&file_path), &version);

        assert_eq!(reply.status, 200);
        assert_eq!(reply.content_type, "text/html; charset=utf-8");
        assert_eq!(header(&reply, "X-Mdview-Title"), Some("notes.md"));
        let body = String::from_utf8(reply.body).unwrap();
        assert_eq!(body.trim(), "<h1>Hi</h1>");
        // /body is a fragment, not a full page: no <html>/<main> scaffolding.
        assert!(!body.contains("<main"));
    }

    #[test]
    fn no_file_selected_renders_empty_page_on_root_and_body() {
        let version = AtomicU64::new(0);

        let root = handle("/", None, &version);
        assert_eq!(root.status, 200);
        assert!(String::from_utf8(root.body)
            .unwrap()
            .contains("Drop a Markdown file here"));

        let body = handle("/body", None, &version);
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

        let root = handle("/", Some(&missing_path), &version);
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

        let reply = handle("/body", Some(&missing_path), &version);
        assert_eq!(reply.status, 200);
        assert_eq!(header(&reply, "X-Mdview-Title"), Some("gone.md"));
        let body = String::from_utf8(reply.body).unwrap();
        assert!(body.contains("Failed to read gone.md"));
        assert!(!body.contains(missing_path.to_str().unwrap()));
    }

    #[test]
    fn non_ascii_title_is_percent_encoded_in_header_and_escaped_in_body() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("メモ<1>.md");
        std::fs::write(&file_path, "# Hi\n").expect("write markdown file");
        let version = AtomicU64::new(0);

        let reply = handle("/body", Some(&file_path), &version);
        assert_eq!(reply.status, 200);
        let title = header(&reply, "X-Mdview-Title").expect("title header present");
        assert!(title.is_ascii(), "header must be ASCII-safe: {title}");
        assert!(title.starts_with("%E3%83%A1%E3%83%A2"), "{title}");
        assert!(title.ends_with("<1>.md"), "{title}");

        std::fs::remove_file(&file_path).expect("delete file");
        let failed = handle("/body", Some(&file_path), &version);
        let body = String::from_utf8(failed.body).unwrap();
        assert!(body.contains("メモ&lt;1&gt;.md"), "{body}");
    }

    #[test]
    fn every_route_carries_common_security_headers() {
        let version = AtomicU64::new(0);
        for route in ["/", "/version", "/body", "/nope"] {
            let reply = handle(route, None, &version);
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

        let reply = handle("/?x=1", Some(&file_path), &version);
        assert_eq!(reply.status, 200);

        let version_reply = handle("/version?t=1", Some(&file_path), &version);
        assert_eq!(version_reply.status, 200);
    }

    #[test]
    fn unknown_path_is_404() {
        let version = AtomicU64::new(0);
        let reply = handle("/nope", None, &version);
        assert_eq!(reply.status, 404);
    }
}

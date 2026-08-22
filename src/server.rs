//! The local HTTP server that renders the watched Markdown file on demand.
//!
//! Binding and serving are split into two functions — [`bind`] and [`run`]
//! — instead of a single blocking `serve` call, because `main` needs the
//! actual bound address (to open the browser and print where it's
//! listening) before it starts serving requests forever.

use crate::render::{page, to_html};
use anyhow::{anyhow, Context, Result};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tiny_http::{Header, Method, Request, Response, Server};

/// Binds a `tiny_http` server on `127.0.0.1:<port>`. Pass `0` to let the OS
/// assign an available port; the actual bound address is available via
/// `server.server_addr()`.
pub fn bind(port: u16) -> Result<Server> {
    Server::http(("127.0.0.1", port))
        .map_err(|err| anyhow!("failed to bind to 127.0.0.1:{port}: {err}"))
}

/// Serves requests on `server` until the server is closed. Routing (after a
/// `Host` header check — see [`host_rejection_reason`]; anything else is
/// `403`):
///
/// - `GET`/`HEAD /` — re-reads `path` from disk, renders it to HTML, and
///   returns the full page (`text/html; charset=utf-8`, `Cache-Control:
///   no-store`, `X-Content-Type-Options: nosniff`, and a
///   `Content-Security-Policy: frame-ancestors 'none'` header). A read
///   failure (file deleted, permissions changed, etc.) yields `500` with
///   the error message as the body (same security headers) rather than
///   tearing down the server.
/// - `GET`/`HEAD /version` — returns the current value of `version` as
///   plain text (`Cache-Control: no-store`, `X-Content-Type-Options:
///   nosniff`), for the page's live-reload script to poll.
/// - anything else — `404`.
///
/// A query string on the request path (`/?x=1`) is ignored for routing
/// purposes. A failure sending an individual response is logged to stderr
/// and does not stop the server — one broken connection shouldn't take
/// every other tab viewing the file down with it. This function itself only
/// returns once the underlying `tiny_http` request iterator ends, which in
/// practice means the server was shut down out from under it.
pub fn run(server: Server, path: &Path, version: Arc<AtomicU64>) -> Result<()> {
    for request in server.incoming_requests() {
        if let Err(err) = handle_request(request, path, &version) {
            eprintln!("warning: failed to handle request: {err}");
        }
    }
    Err(anyhow!("server stopped unexpectedly"))
}

fn handle_request(request: Request, path: &Path, version: &AtomicU64) -> Result<()> {
    if let Some(reason) = host_rejection_reason(&request) {
        return request
            .respond(Response::from_string(reason).with_status_code(403))
            .context("failed to send 403 response");
    }

    let request_path = request.url().split('?').next().unwrap_or("/");

    match (request.method(), request_path) {
        (Method::Get | Method::Head, "/") => respond_with_page(request, path, version),
        (Method::Get | Method::Head, "/version") => respond_with_version(request, version),
        _ => request
            .respond(Response::from_string("404 Not Found").with_status_code(404))
            .context("failed to send 404 response"),
    }
}

/// Body sent (with `403`) whenever a request is rejected over its `Host`.
const HOST_REJECTION_BODY: &str = "403 Forbidden: Host header must be 127.0.0.1 or localhost";

/// Returns `Some(reason)` if `request` should be rejected based on its
/// `Host` header — e.g. a DNS-rebinding attempt that points some other
/// hostname at this server — or `None` if it's fine to proceed.
///
/// A present `Host` must name a loopback address (`127.0.0.1`, `localhost`,
/// or `[::1]`, each optionally followed by `:<port>`, matched
/// case-insensitively for the name). A *missing* `Host` is only tolerated
/// from HTTP/1.0 requests, where the header was never required by the RFC
/// in the first place; HTTP/1.1 and later both require it and get no
/// benefit of the doubt for omitting it.
fn host_rejection_reason(request: &Request) -> Option<&'static str> {
    let host_header = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Host"))
        .map(|header| header.value.as_str());

    match host_header {
        Some(value) if is_allowed_host(value) => None,
        Some(_) => Some(HOST_REJECTION_BODY),
        None if !request_requires_host_header(request) => None,
        None => Some(HOST_REJECTION_BODY),
    }
}

fn request_requires_host_header(request: &Request) -> bool {
    let version = request.http_version();
    (version.0, version.1) >= (1, 1)
}

fn is_allowed_host(host_header: &str) -> bool {
    let host = host_header.trim();

    if let Some(after_bracket) = host.strip_prefix("[::1]") {
        return after_bracket.is_empty() || is_port_suffix(after_bracket);
    }

    let (name, port_suffix) = match host.rsplit_once(':') {
        Some((name, port)) => (name, Some(port)),
        None => (host, None),
    };
    if let Some(port) = port_suffix {
        if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
    }

    name.eq_ignore_ascii_case("127.0.0.1") || name.eq_ignore_ascii_case("localhost")
}

fn is_port_suffix(s: &str) -> bool {
    s.strip_prefix(':')
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

fn respond_with_page(request: Request, path: &Path, version: &AtomicU64) -> Result<()> {
    // Read the live-reload baseline *before* reading the file: if a save
    // lands in between, the version we embed here is guaranteed to be no
    // newer than the content we're about to render, so the client's first
    // comparison can't spuriously miss that save. See `page`'s docs.
    let baseline_version = version.load(Ordering::SeqCst);

    let title = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());

    match fs::read_to_string(path) {
        Ok(markdown) => {
            let body_html = to_html(&markdown);
            let html = page(&title, &body_html, Some(baseline_version));
            request
                .respond(
                    Response::from_string(html)
                        .with_header(content_type_header())
                        .with_header(no_store_header())
                        .with_header(nosniff_header())
                        .with_header(frame_ancestors_none_header()),
                )
                .context("failed to send / response")
        }
        Err(err) => {
            // The client only gets the file name and no error detail — the
            // full path and the OS error (permissions, exact reason, etc.)
            // are none of a browser's business and go to stderr instead.
            eprintln!("warning: failed to read {}: {err}", path.display());
            let body = format!("Failed to read {title}");
            request
                .respond(
                    Response::from_string(body)
                        .with_status_code(500)
                        .with_header(nosniff_header())
                        .with_header(frame_ancestors_none_header()),
                )
                .context("failed to send 500 response")
        }
    }
}

fn respond_with_version(request: Request, version: &AtomicU64) -> Result<()> {
    let body = version.load(Ordering::SeqCst).to_string();
    request
        .respond(
            Response::from_string(body)
                .with_header(no_store_header())
                .with_header(nosniff_header()),
        )
        .context("failed to send /version response")
}

fn content_type_header() -> Header {
    Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..])
        .expect("static header value is valid")
}

fn no_store_header() -> Header {
    Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..])
        .expect("static header value is valid")
}

/// Tells the browser not to try to sniff/reinterpret the response body as
/// something other than the declared `Content-Type` (e.g. treating a
/// Markdown-derived HTML page as a different content type based on its
/// contents). Sent on both `/` and `/version`.
fn nosniff_header() -> Header {
    Header::from_bytes(&b"X-Content-Type-Options"[..], &b"nosniff"[..])
        .expect("static header value is valid")
}

/// Blocks the rendered page from being framed by another site — belt and
/// suspenders alongside the equivalent `<meta http-equiv="Content-Security-
/// Policy">` tag `page()` already emits (an HTTP header can't be
/// stripped/overridden by the page's own markup the way a `<meta>` tag
/// theoretically could be). Sent only on `/`, since `/version` never
/// returns anything framable.
fn frame_ancestors_none_header() -> Header {
    Header::from_bytes(
        &b"Content-Security-Policy"[..],
        &b"frame-ancestors 'none'"[..],
    )
    .expect("static header value is valid")
}

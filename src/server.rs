//! The local HTTP server that renders the watched Markdown file on demand.
//!
//! Binding and serving are split into two functions — [`bind`] and [`run`]
//! — instead of a single blocking `serve` call, because `main` needs the
//! actual bound address (to open the browser and print where it's
//! listening) before it starts serving requests forever.
//!
//! Actual routing (which path maps to which content) lives in
//! [`crate::routes`], shared with the native app's custom protocol handler.
//! This module keeps only what's specific to being an HTTP server: `Host`
//! header validation (a custom protocol has no such header to spoof) and
//! converting a [`routes::Reply`] into a `tiny_http::Response`.

use crate::routes::{self, Reply};
use anyhow::{anyhow, Context, Result};
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tiny_http::{Header, Method, Request, Response, Server};

/// Binds a `tiny_http` server on `127.0.0.1:<port>`. Pass `0` to let the OS
/// assign an available port; the actual bound address is available via
/// `server.server_addr()`.
pub fn bind(port: u16) -> Result<Server> {
    Server::http(("127.0.0.1", port))
        .map_err(|err| anyhow!("failed to bind to 127.0.0.1:{port}: {err}"))
}

/// Serves requests on `server` until the server is closed.
///
/// After a `Host` header check (see [`host_rejection_reason`]; anything
/// suspicious is `403`), only `GET`/`HEAD` requests are routed at all —
/// everything else is `404`, matching the original behavior before routing
/// moved to [`crate::routes`]. The request path (query string included) is
/// handed to `routes::handle`, and the resulting [`Reply`] is converted to a
/// `tiny_http::Response` and sent.
///
/// A failure sending an individual response is logged to stderr and does
/// not stop the server — one broken connection shouldn't take every other
/// tab viewing the file down with it. This function itself only returns
/// once the underlying `tiny_http` request iterator ends, which in practice
/// means the server was shut down out from under it.
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

    match request.method() {
        Method::Get | Method::Head => {
            let reply = routes::handle(request.url(), Some(path), version);
            send_reply(request, reply)
        }
        _ => request
            .respond(Response::from_string("404 Not Found").with_status_code(404))
            .context("failed to send 404 response"),
    }
}

/// Converts a [`Reply`] into a `tiny_http::Response` and sends it.
fn send_reply(request: Request, reply: Reply) -> Result<()> {
    let mut response = Response::from_data(reply.body).with_status_code(reply.status);
    if let Some(content_type) = header("Content-Type", reply.content_type) {
        response = response.with_header(content_type);
    }
    for (name, value) in &reply.headers {
        if let Some(extra) = header(name, value) {
            response = response.with_header(extra);
        }
    }
    request.respond(response).context("failed to send response")
}

/// Builds a `tiny_http` header, or logs and returns `None` if the value
/// isn't representable (tiny_http only accepts ASCII). A dropped header is
/// always preferable to a panic that takes the whole server down.
fn header(name: &str, value: &str) -> Option<Header> {
    match Header::from_bytes(name.as_bytes(), value.as_bytes()) {
        Ok(header) => Some(header),
        Err(()) => {
            eprintln!("warning: dropping response header {name}: value is not ASCII");
            None
        }
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

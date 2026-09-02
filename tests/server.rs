//! Integration tests for the HTTP server: binds on an OS-assigned port,
//! serves a temporary Markdown file, and checks routing/headers over a raw
//! TCP connection (i.e. without any HTTP client crate, per the spec).

use markdown_remarkable::server;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

/// Everything a test needs to talk to a running instance of the server.
/// `_dir` just keeps the temp directory (and therefore `file_path`) alive
/// for the lifetime of the test; it's never read directly.
struct TestServer {
    addr: SocketAddr,
    file_path: PathBuf,
    version: Arc<AtomicU64>,
    _dir: TempDir,
}

fn start_test_server(markdown: &str) -> TestServer {
    start_test_server_named(markdown, "doc.md")
}

fn start_test_server_named(markdown: &str, file_name: &str) -> TestServer {
    let dir = tempfile::tempdir().expect("create tempdir");
    let file_path = dir.path().join(file_name);
    std::fs::write(&file_path, markdown).expect("write temp markdown file");

    let http_server = server::bind(0).expect("bind test server to an OS-assigned port");
    let addr = http_server
        .server_addr()
        .to_ip()
        .expect("test server is bound to a TCP address");

    let version = Arc::new(AtomicU64::new(0));
    let run_path = file_path.clone();
    let run_version = Arc::clone(&version);
    // `server::run` blocks forever serving requests, so it runs on its own
    // thread. The thread (and the server) are simply left running when the
    // test finishes; the process exiting at the end of the test binary
    // reclaims them.
    thread::spawn(move || {
        let _ = server::run(http_server, &run_path, run_version, false);
    });

    // Give the spawned thread a moment to start accepting connections.
    thread::sleep(Duration::from_millis(100));

    TestServer {
        addr,
        file_path,
        version,
        _dir: dir,
    }
}

/// Sends a bare-bones HTTP request with the given method, path, HTTP
/// version, and (optional) `Host` header, and returns the full raw response
/// (status line, headers, and body) as a string. `host: None` omits the
/// `Host` header entirely, which is only legal pre-HTTP/1.1.
///
/// The response is read as raw bytes and, when the server chose
/// `Transfer-Encoding: chunked` (tiny_http does this for HTTP/1.1 once the
/// body grows past its internal threshold), the chunked framing is decoded
/// before the UTF-8 conversion. Reading the wire bytes straight into a
/// `String` used to work only by luck: a chunk boundary can land in the
/// middle of a multi-byte character, making the *framed* stream invalid
/// UTF-8 even though the body itself is fine.
fn raw_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    http_version: &str,
    host: Option<&str>,
) -> String {
    let mut stream = TcpStream::connect(addr).expect("connect to test server");
    let host_line = match host {
        Some(value) => format!("Host: {value}\r\n"),
        None => String::new(),
    };
    let request =
        format!("{method} {path} HTTP/{http_version}\r\n{host_line}Connection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .expect("write raw HTTP request");

    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .expect("read raw HTTP response");

    let header_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(raw.len());
    let head = String::from_utf8(raw[..header_end].to_vec()).expect("headers are valid UTF-8");
    let body_bytes = if head.to_lowercase().contains("transfer-encoding: chunked") {
        decode_chunked(&raw[header_end..])
    } else {
        raw[header_end..].to_vec()
    };
    let body = String::from_utf8(body_bytes).expect("response body is valid UTF-8");
    format!("{head}{body}")
}

/// Decodes an HTTP/1.1 chunked-encoded body: `<hex size>\r\n<data>\r\n`
/// repeated, terminated by a zero-size chunk (any trailers are ignored).
fn decode_chunked(mut rest: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    while let Some(line_end) = rest.windows(2).position(|w| w == b"\r\n") {
        let size_line = std::str::from_utf8(&rest[..line_end]).expect("chunk size line is ASCII");
        let size = usize::from_str_radix(size_line.trim().split(';').next().unwrap_or("0"), 16)
            .expect("chunk size is hex");
        rest = &rest[line_end + 2..];
        if size == 0 {
            break;
        }
        assert!(rest.len() >= size, "chunk data truncated");
        body.extend_from_slice(&rest[..size]);
        rest = &rest[size..];
        // Skip the CRLF that terminates the chunk data.
        rest = rest.strip_prefix(b"\r\n").unwrap_or(rest);
    }
    body
}

/// HTTP/1.1 `GET` with a well-formed loopback `Host` header (what every
/// other test not specifically about `Host` validation wants).
fn raw_get(addr: SocketAddr, path: &str) -> String {
    raw_request(addr, "GET", path, "1.1", Some("127.0.0.1"))
}

/// Sends a raw HTTP/1.1 request with a body and (optionally) extra headers
/// beyond `Host`/`Content-Length`/`Connection`, and returns the full raw
/// response. Used for `PUT /review`, where headers (`X-Mdview-Request`) and
/// the request body matter and the higher-level `raw_get` helper has no way
/// to supply either.
fn raw_request_with_body(
    addr: SocketAddr,
    method: &str,
    path: &str,
    extra_headers: &[(&str, &str)],
    body: &[u8],
) -> Vec<u8> {
    let mut stream = TcpStream::connect(addr).expect("connect to test server");
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in extra_headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");

    stream
        .write_all(head.as_bytes())
        .expect("write request head");
    stream.write_all(body).expect("write request body");

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .expect("read raw HTTP response");
    response
}

fn response_headers(response: &str) -> String {
    response
        .split("\r\n\r\n")
        .next()
        .unwrap_or("")
        .to_lowercase()
}

fn response_body(response: &str) -> &str {
    // Split on the *first* blank line only: the body itself may legally
    // contain CRLF blank lines (e.g. CRLF-embedded assets), and
    // `split(..).nth(1)` would truncate it at the next one.
    response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or("")
}

fn extract_baseline_version(body: &str) -> u64 {
    let marker = "__mdviewVersion=\"";
    let start = body.find(marker).expect("body contains version marker") + marker.len();
    let rest = &body[start..];
    let end = rest.find('"').expect("version marker value is closed");
    rest[..end]
        .parse()
        .expect("version marker value is numeric")
}

#[test]
fn serves_rendered_markdown_version_and_404() {
    let harness = start_test_server("# Hello\n\nSome *text*.\n");

    let root_response = raw_get(harness.addr, "/");
    assert!(
        root_response.starts_with("HTTP/1.1 200"),
        "expected 200 for GET /, got: {root_response}"
    );
    assert!(root_response.contains("<h1>Hello</h1>"));
    assert!(root_response.contains("<em>text</em>"));

    let version_response = raw_get(harness.addr, "/version");
    assert!(
        version_response.starts_with("HTTP/1.1 200"),
        "expected 200 for GET /version, got: {version_response}"
    );
    response_body(&version_response)
        .trim()
        .parse::<u64>()
        .expect("/version body should be a plain integer");

    let missing_response = raw_get(harness.addr, "/nope");
    assert!(
        missing_response.starts_with("HTTP/1.1 404"),
        "expected 404 for GET /nope, got: {missing_response}"
    );
}

#[test]
fn root_response_has_html_content_type() {
    let harness = start_test_server("# Hi\n");
    let response = raw_get(harness.addr, "/");
    let headers = response_headers(&response);
    assert!(
        headers.contains("content-type: text/html; charset=utf-8"),
        "missing/incorrect Content-Type header: {headers}"
    );
}

#[test]
fn version_and_root_responses_are_not_cached() {
    let harness = start_test_server("# Hi\n");

    let root_headers = response_headers(&raw_get(harness.addr, "/"));
    assert!(
        root_headers.contains("cache-control: no-store"),
        "missing Cache-Control on /: {root_headers}"
    );

    let version_headers = response_headers(&raw_get(harness.addr, "/version"));
    assert!(
        version_headers.contains("cache-control: no-store"),
        "missing Cache-Control on /version: {version_headers}"
    );
}

#[test]
fn root_and_version_responses_set_nosniff_and_root_sets_frame_ancestors() {
    let harness = start_test_server("# Hi\n");

    let root_headers = response_headers(&raw_get(harness.addr, "/"));
    assert!(
        root_headers.contains("x-content-type-options: nosniff"),
        "missing X-Content-Type-Options on /: {root_headers}"
    );
    assert!(
        root_headers.contains("content-security-policy: frame-ancestors 'none'"),
        "missing frame-ancestors CSP header on /: {root_headers}"
    );

    let version_headers = response_headers(&raw_get(harness.addr, "/version"));
    assert!(
        version_headers.contains("x-content-type-options: nosniff"),
        "missing X-Content-Type-Options on /version: {version_headers}"
    );
}

#[test]
fn root_returns_500_after_the_file_is_deleted() {
    let harness = start_test_server("# Hi\n");
    std::fs::remove_file(&harness.file_path).expect("delete temp markdown file");

    let response = raw_get(harness.addr, "/");
    assert!(
        response.starts_with("HTTP/1.1 500"),
        "expected 500 after deleting the file, got: {response}"
    );
    let body = response_body(&response);
    assert!(
        body.contains("doc.md"),
        "500 body should still name the file: {body}"
    );
    assert!(
        !body.contains(harness.file_path.to_str().unwrap()),
        "500 body must not leak the absolute path: {body}"
    );
    assert!(
        !body.to_lowercase().contains("no such file"),
        "500 body must not leak the OS error text: {body}"
    );
}

#[test]
fn query_strings_are_ignored_for_routing() {
    let harness = start_test_server("# Hi\n");

    let root = raw_get(harness.addr, "/?x=1");
    assert!(root.starts_with("HTTP/1.1 200"), "GET /?x=1: {root}");

    let version = raw_get(harness.addr, "/version?t=1");
    assert!(
        version.starts_with("HTTP/1.1 200"),
        "GET /version?t=1: {version}"
    );
}

#[test]
fn head_root_is_ok() {
    let harness = start_test_server("# Hi\n");
    let response = raw_request(harness.addr, "HEAD", "/", "1.1", Some("127.0.0.1"));
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "expected 200 for HEAD /, got: {response}"
    );
}

#[test]
fn unrecognized_host_header_is_rejected() {
    let harness = start_test_server("# Hi\n");
    let response = raw_request(harness.addr, "GET", "/", "1.1", Some("evil.example.com"));
    assert!(
        response.starts_with("HTTP/1.1 403"),
        "expected 403 for a spoofed Host header, got: {response}"
    );
    assert!(
        response_body(&response).contains("Host header must be 127.0.0.1 or localhost"),
        "403 body should explain why: {response}"
    );
}

#[test]
fn host_header_name_matching_is_case_insensitive() {
    let harness = start_test_server("# Hi\n");
    let host = format!("LocalHost:{}", harness.addr.port());
    let response = raw_request(harness.addr, "GET", "/", "1.1", Some(&host));
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "expected 200 for a mixed-case localhost Host header, got: {response}"
    );
}

#[test]
fn missing_host_header_is_rejected_on_http_1_1() {
    let harness = start_test_server("# Hi\n");
    let response = raw_request(harness.addr, "GET", "/", "1.1", None);
    assert!(
        response.starts_with("HTTP/1.1 403"),
        "expected 403 for a missing Host header on HTTP/1.1, got: {response}"
    );
}

#[test]
fn missing_host_header_is_tolerated_on_http_1_0() {
    let harness = start_test_server("# Hi\n");
    let response = raw_request(harness.addr, "GET", "/", "1.0", None);
    assert!(
        response.starts_with("HTTP/1.0 200") || response.starts_with("HTTP/1.1 200"),
        "expected 200 for a missing Host header on HTTP/1.0, got: {response}"
    );
}

#[test]
fn body_route_returns_fragment_with_title_header() {
    let harness = start_test_server("# Hello\n\nSome *text*.\n");

    let response = raw_get(harness.addr, "/body");
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "expected 200 for GET /body, got: {response}"
    );

    let headers = response_headers(&response);
    assert!(
        headers.contains("x-mdview-title: doc.md"),
        "missing X-Mdview-Title header on /body: {headers}"
    );
    assert!(
        headers.contains("content-type: text/html; charset=utf-8"),
        "missing/incorrect Content-Type on /body: {headers}"
    );
    assert!(
        headers.contains("cache-control: no-store"),
        "missing Cache-Control on /body: {headers}"
    );

    let body = response_body(&response);
    assert!(body.contains("<h1>Hello</h1>"));
    assert!(body.contains("<em>text</em>"));
    // /body is a fragment, not a full page.
    assert!(!body.contains("<html"));
    assert!(!body.contains("__mdviewVersion"));
}

#[test]
fn body_route_survives_non_ascii_file_names() {
    let harness = start_test_server_named("# Hello\n", "メモ.md");

    let response = raw_get(harness.addr, "/body");
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "expected 200 for GET /body on a non-ASCII file name, got: {response}"
    );
    let headers = response_headers(&response);
    assert!(
        headers.contains("x-mdview-title: %e3%83%a1%e3%83%a2.md"),
        "title header must be percent-encoded ASCII: {headers}"
    );
    assert!(response_body(&response).contains("<h1>Hello</h1>"));

    // The server must still be alive afterwards (no panic on the header).
    let again = raw_get(harness.addr, "/version");
    assert!(again.starts_with("HTTP/1.1 200"), "{again}");
}

#[test]
fn body_route_returns_200_error_fragment_after_the_file_is_deleted() {
    let harness = start_test_server("# Hi\n");
    std::fs::remove_file(&harness.file_path).expect("delete temp markdown file");

    let response = raw_get(harness.addr, "/body");
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "expected 200 error fragment for /body after deletion, got: {response}"
    );
    let body = response_body(&response);
    assert!(body.contains("Failed to read doc.md"), "{body}");
    assert!(!body.contains(harness.file_path.to_str().unwrap()));

    // The 500 page for `/` must still carry the live script so the view
    // recovers on its own once the file is back.
    let root = raw_get(harness.addr, "/");
    assert!(root.starts_with("HTTP/1.1 500"), "{root}");
    assert!(response_body(&root).contains("/version"), "{root}");
}

#[test]
fn all_routes_carry_frame_ancestors_csp_header() {
    let harness = start_test_server("# Hi\n");
    for path in ["/", "/version", "/body"] {
        let headers = response_headers(&raw_get(harness.addr, path));
        assert!(
            headers.contains("content-security-policy: frame-ancestors 'none'"),
            "missing frame-ancestors on {path}: {headers}"
        );
    }
}

#[test]
fn root_body_embeds_the_version_it_was_rendered_against() {
    let harness = start_test_server("# Hi\n");

    let first = raw_get(harness.addr, "/");
    let first_baseline = extract_baseline_version(response_body(&first));
    let first_version: u64 = response_body(&raw_get(harness.addr, "/version"))
        .trim()
        .parse()
        .expect("/version body is numeric");
    assert_eq!(first_baseline, first_version);

    // Simulate a save landing, and check the newly embedded baseline
    // tracks it too — not just whatever the counter happened to start at.
    harness.version.fetch_add(1, Ordering::SeqCst);

    let second = raw_get(harness.addr, "/");
    let second_baseline = extract_baseline_version(response_body(&second));
    let second_version: u64 = response_body(&raw_get(harness.addr, "/version"))
        .trim()
        .parse()
        .expect("/version body is numeric");
    assert_eq!(second_baseline, second_version);
    assert_eq!(second_version, first_version + 1);
}

#[test]
fn put_review_without_the_request_header_is_403_over_raw_http() {
    let harness = start_test_server("# Hi\n");
    let body = br#"{"version":1,"file":"doc.md","blocks":[]}"#;

    let response = raw_request_with_body(harness.addr, "PUT", "/review", &[], body);
    let response = String::from_utf8_lossy(&response);
    assert!(
        response.starts_with("HTTP/1.1 403"),
        "expected 403 for PUT /review without X-Mdview-Request, got: {response}"
    );
}

#[test]
fn put_review_with_the_request_header_succeeds_over_raw_http() {
    let harness = start_test_server("# Hi\n");
    let body = br#"{"version":1,"file":"doc.md","blocks":[]}"#;

    let response = raw_request_with_body(
        harness.addr,
        "PUT",
        "/review",
        &[("X-Mdview-Request", "1")],
        body,
    );
    let response = String::from_utf8_lossy(&response);
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "expected 200 for PUT /review with X-Mdview-Request, got: {response}"
    );
    assert!(response.contains(r#""ok":true"#) || response.contains(r#""ok": true"#));

    // The write actually landed: a follow-up GET reflects it.
    let get_response = raw_get(harness.addr, "/review");
    assert!(get_response.starts_with("HTTP/1.1 200"), "{get_response}");
}

#[test]
fn head_asset_returns_ok_with_image_content_type_and_csp() {
    let harness = start_test_server("# Hi\n\n![x](img.png)\n");
    std::fs::write(harness._dir.path().join("img.png"), b"not-really-a-png")
        .expect("write image file next to the document");

    let response = raw_request(
        harness.addr,
        "HEAD",
        "/asset?p=img.png",
        "1.1",
        Some("127.0.0.1"),
    );
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "expected 200 for HEAD /asset?p=img.png, got: {response}"
    );
    let headers = response_headers(&response);
    assert!(
        headers.contains("content-type: image/png"),
        "missing/incorrect Content-Type on HEAD /asset: {headers}"
    );
    assert!(
        headers.contains("content-security-policy: default-src 'none'; sandbox"),
        "missing/incorrect CSP on HEAD /asset: {headers}"
    );
}

#[test]
fn get_tree_returns_the_file_tree_as_json() {
    let harness = start_test_server("# Hi\n");
    std::fs::write(harness._dir.path().join("b.md"), "# B\n").expect("write sibling markdown file");

    let response = raw_get(harness.addr, "/tree");
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "expected 200 for GET /tree, got: {response}"
    );
    let headers = response_headers(&response);
    assert!(
        headers.contains("content-type: application/json; charset=utf-8"),
        "missing/incorrect Content-Type on GET /tree: {headers}"
    );
    let body = response_body(&response);
    assert!(body.contains(r#""current":"doc.md""#), "{body}");
    assert!(body.contains(r#""name":"b.md""#), "{body}");
}

#[test]
fn put_open_is_501_in_browser_mode() {
    let harness = start_test_server("# Hi\n");
    std::fs::write(harness._dir.path().join("b.md"), "# B\n").expect("write sibling markdown file");
    let body = br#"{"path":"b.md"}"#;

    let response = raw_request_with_body(
        harness.addr,
        "PUT",
        "/open",
        &[("X-Mdview-Request", "1")],
        body,
    );
    let response = String::from_utf8_lossy(&response);
    assert!(
        response.starts_with("HTTP/1.1 501"),
        "expected 501 for PUT /open in --browser mode, got: {response}"
    );
}

#[test]
fn put_nav_is_501_in_browser_mode() {
    let harness = start_test_server("# Hi\n");
    let body = br#"{"dir":"back"}"#;

    let response = raw_request_with_body(
        harness.addr,
        "PUT",
        "/nav",
        &[("X-Mdview-Request", "1")],
        body,
    );
    let response = String::from_utf8_lossy(&response);
    assert!(
        response.starts_with("HTTP/1.1 501"),
        "expected 501 for PUT /nav in --browser mode, got: {response}"
    );
}

#[test]
fn put_review_body_over_1_mib_is_413_over_raw_http() {
    let harness = start_test_server("# Hi\n");
    // A JSON body comfortably over the 1 MiB limit.
    let oversized_text = "x".repeat(2 * 1024 * 1024);
    let body = format!(
        r#"{{"version":1,"file":"doc.md","blocks":[{{"hash":"0123456789abcdef","excerpt":"x","comments":[{{"id":"c_1","text":"{oversized_text}","created":"2026-08-22T07:00:00Z","updated":"2026-08-22T07:00:00Z"}}]}}]}}"#
    );

    let response = raw_request_with_body(
        harness.addr,
        "PUT",
        "/review",
        &[("X-Mdview-Request", "1")],
        body.as_bytes(),
    );
    let response = String::from_utf8_lossy(&response);
    assert!(
        response.starts_with("HTTP/1.1 413"),
        "expected 413 for an oversized PUT /review body, got: {}",
        &response[..response.len().min(200)]
    );
}

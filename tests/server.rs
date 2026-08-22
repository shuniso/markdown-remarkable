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
    let dir = tempfile::tempdir().expect("create tempdir");
    let file_path = dir.path().join("doc.md");
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
        let _ = server::run(http_server, &run_path, run_version);
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

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("read raw HTTP response");
    response
}

/// HTTP/1.1 `GET` with a well-formed loopback `Host` header (what every
/// other test not specifically about `Host` validation wants).
fn raw_get(addr: SocketAddr, path: &str) -> String {
    raw_request(addr, "GET", path, "1.1", Some("127.0.0.1"))
}

fn response_headers(response: &str) -> String {
    response
        .split("\r\n\r\n")
        .next()
        .unwrap_or("")
        .to_lowercase()
}

fn response_body(response: &str) -> &str {
    response.split("\r\n\r\n").nth(1).unwrap_or("")
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

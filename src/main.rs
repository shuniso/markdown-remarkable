//! `mdview` CLI entry point.
//!
//! This file only parses arguments and wires the library modules together
//! (rendering, the native app, the HTTP server, and file watching); the
//! actual logic lives in `markdown_remarkable::{app, render, server,
//! watch}`.

use anyhow::{Context, Result};
use clap::Parser;
use markdown_remarkable::render::{page, to_html};
use markdown_remarkable::{app, server, watch};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

/// A tiny Markdown viewer: opens a native window (by default) or serves the
/// file to a browser tab (`--browser`), reloading in place whenever the
/// file is saved.
#[derive(Parser, Debug)]
#[command(name = "mdview", version, about)]
struct Cli {
    /// Markdown file to view. If omitted, the native window opens a
    /// file-picker dialog on startup — cancelling it shows an empty "drop a
    /// file here" page instead of exiting.
    file: Option<PathBuf>,

    /// Show the file in your default browser instead of opening a native
    /// window (the original CLI's behavior, unchanged). Requires FILE.
    #[arg(long, requires = "file")]
    browser: bool,

    /// Port to listen on. 0 lets the OS pick a free port. Only applies to
    /// `--browser`.
    #[arg(long, default_value_t = 0, requires = "browser")]
    port: u16,

    /// Don't open the default browser automatically. Only applies to
    /// `--browser`.
    #[arg(long, requires = "browser")]
    no_open: bool,

    /// Render the file to a standalone HTML file and exit, instead of
    /// showing it live. Requires FILE, and is mutually exclusive with
    /// `--browser`/`--port`/`--no-open` (which only apply to the live
    /// view).
    #[arg(
        long,
        value_name = "OUT.html",
        requires = "file",
        conflicts_with_all = ["browser", "port", "no_open"]
    )]
    export: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // If FILE was given, confirm up front that it's readable — this turns
    // "doesn't exist"/"unreadable" into a clean exit-1 error before doing
    // anything else (opening a server, a window, etc.) in every mode, the
    // same as the original CLI did.
    let markdown = cli
        .file
        .as_deref()
        .map(|file| {
            fs::read_to_string(file).with_context(|| format!("failed to read {}", file.display()))
        })
        .transpose()?;

    if let Some(export_path) = cli.export.as_deref() {
        // clap guarantees `file`/`markdown` are `Some` here (`--export`
        // `requires = "file"`).
        let file = cli.file.as_deref().expect("--export requires FILE");
        let markdown = markdown.expect("--export requires FILE");
        return export(file, &markdown, export_path);
    }

    if cli.browser {
        // clap guarantees `file` is `Some` here (`--browser` `requires =
        // "file"`): the browser server has no equivalent of the native
        // app's file-picker dialog / drag&drop, so there's nothing sensible
        // to serve without one.
        let file = cli.file.as_deref().expect("--browser requires FILE");
        return run_browser(file, cli.port, cli.no_open);
    }

    app::run(cli.file)
}

/// The original `--browser` flow: bind a local HTTP server, watch the file,
/// open the default browser, and serve forever. Unchanged from before this
/// file gained a native-window default.
fn run_browser(file: &Path, port: u16, no_open: bool) -> Result<()> {
    let version = Arc::new(AtomicU64::new(0));

    let http_server = server::bind(port).context("failed to start server")?;
    let bound_port = http_server
        .server_addr()
        .to_ip()
        .context("server is not bound to a TCP address")?
        .port();
    let url = format!("http://127.0.0.1:{bound_port}/");
    println!("Serving {} at {url}", file.display());

    // Keep the watcher bound for the lifetime of the program: dropping it
    // would stop the watch. A failure here is non-fatal — we just lose
    // live-reload and keep serving.
    let _watcher = match watch::watch(file, Arc::clone(&version)) {
        Ok(watcher) => Some(watcher),
        Err(err) => {
            eprintln!("warning: live-reload disabled: {err}");
            None
        }
    };

    if !no_open {
        if let Err(err) = open::that(&url) {
            eprintln!("warning: failed to open browser: {err}");
        }
    }

    server::run(http_server, file, version)
}

/// Renders `markdown` (already read from `file`, so `--export` doesn't pay
/// for a second disk read) to a standalone, non-live HTML page and writes it
/// to `out`.
fn export(file: &Path, markdown: &str, out: &Path) -> Result<()> {
    ensure_not_same_file(file, out)?;

    let body_html = to_html(markdown);
    let title = file
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| file.display().to_string());
    let html = page(&title, &body_html, None);

    fs::write(out, html).with_context(|| format!("failed to write {}", out.display()))?;
    println!("Exported {} to {}", file.display(), out.display());
    Ok(())
}

/// Refuses to export over the input file itself. `file` is known to exist
/// (main already read it), so it can be canonicalized directly.
///
/// `out` is handled two ways:
/// - If it already exists, it's canonicalized directly, which also catches
///   `out` being a *symlink* to the input file (canonicalize follows
///   symlinks).
/// - Otherwise (the common case: `--export` usually names a file that
///   doesn't exist yet, so canonicalizing it outright would just fail) its
///   parent directory is canonicalized and rejoined with its file name. A
///   parent directory that doesn't exist is an error here rather than a
///   silent fallback to the current directory — better to say so than to
///   guess.
///
/// Either way, on Unix the resulting path's `(device, inode)` is also
/// compared against the input file's, which catches a *hard* link to the
/// input file — a different path that canonicalizes to itself rather than
/// to the input file, since hard links (unlike symlinks) have no
/// resolvable target for `canonicalize` to follow.
fn ensure_not_same_file(file: &Path, out: &Path) -> Result<()> {
    let input_canonical = file
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", file.display()))?;

    let out_candidate = if out.exists() {
        out.canonicalize()
            .with_context(|| format!("failed to resolve {}", out.display()))?
    } else {
        let out_parent = out
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let out_parent_canonical = out_parent.canonicalize().map_err(|_| {
            anyhow::anyhow!("output directory does not exist: {}", out_parent.display())
        })?;
        let out_file_name = out.file_name().unwrap_or(out.as_os_str());
        out_parent_canonical.join(out_file_name)
    };

    if out_candidate == input_canonical || is_same_file_unix(&out_candidate, &input_canonical) {
        anyhow::bail!(
            "refusing to overwrite the input file: {} and {} are the same file",
            file.display(),
            out.display()
        );
    }
    Ok(())
}

#[cfg(unix)]
fn is_same_file_unix(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (a.metadata(), b.metadata()) {
        (Ok(meta_a), Ok(meta_b)) => meta_a.dev() == meta_b.dev() && meta_a.ino() == meta_b.ino(),
        _ => false,
    }
}

/// Non-Unix platforms have no portable equivalent of `(dev, ino)` exposed
/// via `std`, so a hard link to the input file (a different path than
/// `out_candidate`/`input_canonical` but the same underlying file) can't be
/// detected here the way it is on Unix. The `canonicalize`-based path
/// comparison in `ensure_not_same_file` still catches the same-path and
/// symlink cases regardless of platform; only the hard-link case is
/// Unix-only.
#[cfg(not(unix))]
fn is_same_file_unix(_a: &Path, _b: &Path) -> bool {
    false
}

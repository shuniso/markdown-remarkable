//! `mdview` CLI entry point.
//!
//! This file only parses arguments and wires the library modules together
//! (rendering, the native app, the HTTP server, and file watching); the
//! actual logic lives in `markdown_remarkable::{app, render, server,
//! watch}`.
//!
//! On Windows, `windows_subsystem = "windows"` below builds `mdview.exe`
//! as a GUI subsystem binary — this suppresses the console window that
//! would otherwise flash open when the app is launched from Explorer/a
//! file association (there being no console to attach to in that case).
//! The tradeoff: a GUI-subsystem binary also has no console — and
//! therefore no stdout/stderr for `--browser`/`--export` to print to —
//! when launched from a terminal instead. `attach_parent_console` (below)
//! recovers that: it's the first thing `main` does on Windows, and asks
//! the OS to attach this process to whatever console its parent process
//! already has. Launched from Explorer, there is no parent console, the
//! call harmlessly fails, and the app stays console-free as intended.
//! Launched from a terminal (cmd/PowerShell/Windows Terminal), the parent
//! shell's console is what's already visible on screen, so attaching to
//! it makes `println!`/`eprintln!` show up there exactly as they would on
//! a console-subsystem binary. One caveat inherent to this order: any
//! `println!`/`eprintln!` that ran *before* `attach_parent_console` (there
//! are none today, but a future change could add one) would be silently
//! discarded rather than printed — with no console attached yet, Rust's
//! std streams have nothing to write to and drop the output rather than
//! erroring.
#![cfg_attr(windows, windows_subsystem = "windows")]

use anyhow::{Context, Result};
use clap::Parser;
use markdown_remarkable::render::{page, to_html};
use markdown_remarkable::{app, review, server, watch};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

/// A tiny Markdown viewer: opens a native window per file (by default) or
/// serves a single file to a browser tab (`--browser`), reloading in place
/// whenever a file is saved.
#[derive(Parser, Debug)]
#[command(name = "mdview", version, about)]
struct Cli {
    /// Markdown file(s) to view — each gets its own native window. If
    /// omitted entirely, one native window opens a file-picker dialog on
    /// startup — cancelling it shows an empty "drop a file here" page
    /// instead of exiting.
    #[arg(num_args = 0..)]
    file: Vec<PathBuf>,

    /// Show a file in your default browser instead of opening a native
    /// window (the original CLI's behavior, unchanged). Requires exactly
    /// one FILE — the browser server has no equivalent of the native app's
    /// multiple windows.
    #[arg(long)]
    browser: bool,

    /// Port to listen on. 0 lets the OS pick a free port. Only applies to
    /// `--browser`.
    #[arg(long, default_value_t = 0, requires = "browser")]
    port: u16,

    /// Don't open the default browser automatically. Only applies to
    /// `--browser`.
    #[arg(long, requires = "browser")]
    no_open: bool,

    /// Render a file to a standalone HTML file and exit, instead of
    /// showing it live. Requires exactly one FILE, and is mutually
    /// exclusive with `--browser`/`--port`/`--no-open` (which only apply to
    /// the live view).
    #[arg(
        long,
        value_name = "OUT.html",
        conflicts_with_all = ["browser", "port", "no_open"]
    )]
    export: Option<PathBuf>,

    /// Allow the page to load images from `http(s):` URLs, in addition to
    /// inline `data:` images. Off by default: a remote image target in the
    /// document then renders as a broken `<img>` rather than an outbound
    /// request, since fetching one would leak the viewer's IP (and
    /// potentially which document they're viewing) to whatever host it
    /// points at — the same "tracking pixel" concern email/RSS clients
    /// guard against by default. Applies to the native window, `--browser`,
    /// and `--export` alike.
    #[arg(long)]
    allow_remote_images: bool,
}

fn main() -> Result<()> {
    #[cfg(windows)]
    attach_parent_console();

    let cli = Cli::parse();

    // `--browser`/`--export` each show/render a single file — neither has
    // any equivalent of the native app's multiple windows — so more than
    // one FILE alongside either is a usage error, checked up front before
    // touching the filesystem at all.
    if (cli.browser || cli.export.is_some()) && cli.file.len() != 1 {
        anyhow::bail!("--browser/--export take exactly one FILE");
    }

    if let Some(export_path) = cli.export.as_deref() {
        // The length check above guarantees exactly one FILE here.
        // `--export` is the one mode that actually needs the file's
        // *contents* right away (to render it once and exit) — every other
        // mode below only ever needs to know a file is readable right now;
        // the native window and the browser server each read a file live,
        // per request, via `routes::handle`.
        let file = &cli.file[0];
        let markdown = fs::read_to_string(file)
            .with_context(|| format!("failed to read {}", file.display()))?;
        return export(file, &markdown, export_path, cli.allow_remote_images);
    }

    if cli.browser {
        // The length check above guarantees exactly one FILE here. A
        // single bad file is fatal in this mode (there's no second window
        // to fall back to showing), so this propagates as a hard error.
        let file = &cli.file[0];
        confirm_readable(file)?;
        return run_browser(file, cli.port, cli.no_open, cli.allow_remote_images);
    }

    // Native mode: open a window for every FILE that's readable right now.
    // An unreadable one (typo, deleted, permissions) gets a warning and is
    // skipped rather than failing the whole launch — the other files (and
    // their windows) are still worth showing regardless. Only if every
    // given FILE failed is that a hard error (there would be nothing left
    // to open a window for).
    let had_files = !cli.file.is_empty();
    let readable: Vec<PathBuf> = cli
        .file
        .into_iter()
        .filter(|file| match confirm_readable(file) {
            Ok(()) => true,
            Err(err) => {
                eprintln!("warning: {err:#}");
                false
            }
        })
        .collect();
    if had_files && readable.is_empty() {
        anyhow::bail!("none of the given files could be read");
    }
    app::run(readable, cli.allow_remote_images)
}

/// Attaches this process's stdout/stderr to whatever console its parent
/// process already has (a no-op, harmlessly, if the parent has none — e.g.
/// launched from Explorer/a file association). See the module doc comment
/// above `#![windows_subsystem = "windows"]` for why this exists and why
/// it must run first thing in `main`. The return value is intentionally
/// ignored: failure just means "no parent console to attach to," which is
/// the normal, expected case for a GUI launch.
#[cfg(windows)]
fn attach_parent_console() {
    use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

/// Confirms `file` can be opened for reading right now, without actually
/// reading its contents — this turns "doesn't exist"/"unreadable" into a
/// clean error before the native window or browser server (each of which
/// reads a file live, per request, rather than once up front) ever tries
/// to serve it.
fn confirm_readable(file: &Path) -> Result<()> {
    fs::File::open(file)
        .map(|_file| ())
        .with_context(|| format!("failed to read {}", file.display()))
}

/// The original `--browser` flow: bind a local HTTP server, watch the file,
/// open the default browser, and serve forever. Unchanged from before this
/// file gained a native-window default, aside from threading
/// `allow_remote_images` through to every response `server::run` sends.
fn run_browser(file: &Path, port: u16, no_open: bool, allow_remote_images: bool) -> Result<()> {
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

    server::run(http_server, file, version, allow_remote_images)
}

/// Renders `markdown` (already read from `file`, so `--export` doesn't pay
/// for a second disk read) to a standalone, non-live HTML page and writes it
/// to `out`.
fn export(file: &Path, markdown: &str, out: &Path, allow_remote_images: bool) -> Result<()> {
    ensure_not_same_file(file, out)?;

    let body_html = to_html(markdown);
    let title = file
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| file.display().to_string());
    let html = page(&title, &body_html, None, allow_remote_images);

    // Same tmp-file-plus-rename, symlink-race-safe write `review::save`/
    // `review::export` use for the sidecar/review-export files — `out` is a
    // user-chosen path, so a pre-existing symlink there (accidental or
    // planted) is never followed and truncated by a plain `fs::write`.
    review::atomic_write(out, html.as_bytes())
        .with_context(|| format!("failed to write {}", out.display()))?;
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

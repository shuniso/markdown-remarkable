//! Library crate for `mdview`: a tiny Markdown viewer that renders a file to
//! GitHub-flavored HTML, serves it locally, and live-reloads on save.
//!
//! The `mdview` binary (`src/main.rs`) is a thin wrapper around these
//! modules; the modules themselves are exposed here so integration tests can
//! exercise them directly.

pub mod render;
pub mod server;
pub mod watch;

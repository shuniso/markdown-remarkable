//! Small helpers shared across modules that would otherwise create an
//! awkward dependency (e.g. `review.rs` and `app.rs` both needing the same
//! file-title logic `routes.rs` used to own).

use std::path::Path;

/// The display title for `path`: its file name, or the full path if it
/// somehow has none. Shared by `routes.rs` (the `X-Mdview-Title` header and
/// error fragments), `review.rs` (the sidecar's `file` field and the
/// exported Markdown's heading), and `app.rs` (the native window's title
/// bar).
pub(crate) fn file_title(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

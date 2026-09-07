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

/// Which kind of document a path names, decided purely by its file
/// extension. The single source of truth for "can this app open that
/// file?" — `app.rs` (drag & drop, the Finder "Open With"/`open -a` path,
/// and the file-picker dialog's filter), `routes.rs` (`GET /tree`'s
/// listing and `PUT /open`'s acceptance check), and every rendering call
/// site (`routes.rs`, `review.rs`, `main.rs`) all go through this instead
/// of repeating an extension match of their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// `.md` / `.markdown` — parsed and rendered as Markdown.
    Markdown,
    /// `.txt` — shown verbatim, with no Markdown syntax interpreted at
    /// all (see `render::to_html_plain`).
    PlainText,
}

/// `path`'s [`FileKind`], or `None` if its extension isn't one this app
/// opens. Matched case-insensitively (`NOTES.TXT` is plain text), against
/// the extension only — a file with no extension at all, or a non-UTF-8
/// one, is `None`.
///
/// `None` means "reject at the entry point" (drag & drop, `PUT /open`,
/// `GET /tree`), *not* "refuse to render": a file opened through the CLI
/// can have any extension, and those call sites fall back to
/// [`FileKind::Markdown`] rather than failing — see the spec's
/// `file_kind(path).unwrap_or(FileKind::Markdown)` rule.
pub fn file_kind(path: &Path) -> Option<FileKind> {
    let extension = path.extension().and_then(|ext| ext.to_str())?;
    if extension.eq_ignore_ascii_case("md") || extension.eq_ignore_ascii_case("markdown") {
        Some(FileKind::Markdown)
    } else if extension.eq_ignore_ascii_case("txt") {
        Some(FileKind::PlainText)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_kind_recognizes_markdown_extensions() {
        assert_eq!(file_kind(Path::new("a.md")), Some(FileKind::Markdown));
        assert_eq!(file_kind(Path::new("a.markdown")), Some(FileKind::Markdown));
    }

    #[test]
    fn file_kind_recognizes_a_txt_extension() {
        assert_eq!(file_kind(Path::new("a.txt")), Some(FileKind::PlainText));
    }

    #[test]
    fn file_kind_ignores_extension_case() {
        assert_eq!(file_kind(Path::new("A.MD")), Some(FileKind::Markdown));
        assert_eq!(file_kind(Path::new("a.MarkDown")), Some(FileKind::Markdown));
        assert_eq!(file_kind(Path::new("a.TXT")), Some(FileKind::PlainText));
    }

    #[test]
    fn file_kind_rejects_unknown_or_missing_extensions() {
        assert_eq!(file_kind(Path::new("a.rs")), None);
        assert_eq!(file_kind(Path::new("a.mdx")), None);
        assert_eq!(file_kind(Path::new("README")), None);
        assert_eq!(file_kind(Path::new(".hidden")), None);
    }
}

# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Native Markdown viewer window (macOS, Linux, and Windows, built on
  wry/tao) that live-reloads the open file in place on save; a `--browser`
  mode serves the same view to a local browser tab (bound to
  `127.0.0.1` only) instead of opening a native window.
- Block-level inline review comments: select a paragraph, heading, list,
  code block, table, blockquote, list item, or table row and leave a
  comment, stored in a `<file>.review.json` sidecar next to the document
  rather than in the Markdown itself. The document as a whole can also
  carry file-wide comments.
- One-click export of a review to a concise Markdown summary (line range
  plus a short excerpt per commented block).
- Left-hand file tree listing the `.md`/`.markdown` files under the open
  file's directory, with same-window switching between them. Clicking a
  relative link to another Markdown file in the document body opens it in
  the same window too (or brings an already-open window for that file to
  the front), with per-window back/forward navigation history.
- Local image rendering for images referenced with a relative path under
  the open Markdown file's own directory; remote (`http(s)`) images are
  blocked unless `--allow-remote-images` is passed.
- Leading YAML frontmatter is parsed and stripped from the rendered
  output instead of being shown as text.
- Baseline UX: a resizable and collapsible review pane, zoom in/out/reset,
  window position and size memory across restarts, and a full set of
  keyboard shortcuts for opening, closing, navigating, and commenting.
- `--export <OUT.html>` to render a document to a standalone HTML file
  and exit, without opening a live view.
- macOS app bundle (`scripts/bundle-macos.sh`) with double-click launch,
  `.md`/`.markdown` file association, native-architecture builds, and an
  optional signing identity for `--install`.
- Windows build support, including a GUI-subsystem release binary that
  opens no console window when launched from Explorer or a file
  association, and Windows entries in the CI matrix and release pipeline.
- An optional code-signing hook plus a personal self-signing helper
  script for release builds.

### Changed

- Renamed the binary/`.app` from `mdview` to `markdown-remarkable`, and
  changed the macOS bundle id to `com.shuniso.markdown-remarkable`.
  Changing the bundle identifier moves the `.app`'s WebView data store, so
  UI preferences kept in `localStorage` (zoom, pane widths, collapsed
  state) start from defaults after upgrading; the page origin itself is
  unchanged. This also moves `window.json`'s save location to a
  `markdown-remarkable` directory under the OS's config dir; there's no
  migration from the old `mdview` path. Since there's no tagged release
  yet, this isn't called out as breaking.
- Export: the nested-anchor suffix now uses ASCII parentheses,
  `(in list L10-L20)`, instead of full-width ones.

### Security

- Documented the app's local-only guarantees — no outbound network
  access outside the opt-in `--browser` (loopback-only) and
  `--allow-remote-images` modes, path validation for local images and
  cross-file navigation, and atomic writes for all files the app saves —
  in `docs/SECURITY.md`, and fixed the issues found during that audit.

[Unreleased]: https://github.com/shuniso/markdown-remarkable/commits/main

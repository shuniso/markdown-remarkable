# markdown-remarkable

A small Rust CLI, `mdview`, that renders a Markdown file to GitHub-flavored
HTML and shows it live, reloading automatically every time you save the
file. Nothing more: no multi-file navigation, no syntax highlighting, no
editing. Just "write, save, see it update."

By default it opens its own native window (via `wry`/`tao`), not a browser
tab; pass `--browser` to fall back to the original local-server-plus-browser
behavior.

## Usage

```sh
mdview README.md
```

This opens a native window showing the rendered file. Edit and save the
file — the window updates in place (no full reload, scroll position kept).
Deleting the file is detected the same way as any other save: the window
shows a "failed to read" message instead of the old content, then updates
again on its own once the file exists (and is readable) again.

```sh
mdview a.md b.md c.md
```

Each `FILE` gets its own native window, so you can see several files side
by side. Windows are independent: each has its own watcher, title, and
zoom level, and closing one (⌘W or its close button) only closes that
window — the app itself only quits once every window is closed (or on
⌘Q, which closes all of them at once). New windows cascade slightly down
and to the right of the last one so they don't stack exactly on top of
each other.

```sh
mdview
```

With no `FILE`, a native "open file" dialog appears; pick a `.md`/
`.markdown` file, or cancel to see an empty "drop a Markdown file here"
window. You can also drag & drop one or more `.md`/`.markdown` files onto a
window at any time — the first one switches that window to the dropped file
(watcher and title along with it) if it's still empty, and every other
dropped file opens in its own new window. ⌘O behaves the same way, applied
to whichever window is currently frontmost. Opening a file that's already
open in another window — whether via a drop, ⌘O, a repeated `FILE` on the
command line, or Finder/"Open With"/`open -a` — brings that window to the
front instead of opening a duplicate, across every one of these routes.

```sh
mdview README.md --browser
```

Serves the file over a local HTTP server and opens it in your default
browser instead of a native window — the original CLI's behavior, unchanged.

### Options

| Flag | Default | Description |
|---|---|---|
| `--browser` | off | Show the file in your default browser instead of opening a native window. Requires exactly one `FILE`. |
| `--port <PORT>` | `0` | Port to listen on. `0` lets the OS pick a free port. Only applies to `--browser`. |
| `--no-open` | off | Don't open the browser automatically. Only applies to `--browser`. |
| `--export <OUT.html>` | — | Render to a standalone HTML file and exit, instead of showing it live. Requires exactly one `FILE`, and is mutually exclusive with `--browser`/`--port`/`--no-open`. Refuses to write over the input file itself. |

Set the environment variable `MDVIEW_DEBUG=1` to log every request the
native window's WebView makes (`[mdview] GET /version -> 200` and so on) to
stderr — handy for checking that live updates are actually flowing.

Examples:

```sh
# Native window, drag & drop a file in (or use the open dialog)
mdview

# Browser mode: pick a fixed port and skip opening a browser
mdview notes.md --browser --port 8080 --no-open

# Render to a static HTML file (no live reload, no window, no server)
mdview notes.md --export notes.html
```

## Features

- GitHub-style rendering: tables, strikethrough, task lists, and footnotes
  are all supported.
- Light and dark themes, following the OS/browser's `prefers-color-scheme`.
- Live updates on save: the window (or browser tab) swaps in the freshly
  rendered content in place, preserving scroll position, instead of a full
  reload. Directory-watch based, so it works with editors that save
  "atomically" via a temp file + rename.
- Single static binary — CSS and JS are embedded at compile time, so it
  never fetches anything external.
- Raw HTML embedded in the Markdown source is rendered as literal, inert
  text instead of being executed, and `javascript:`/`data:`-style link and
  image targets are neutralized (rewritten to `#`) rather than left
  clickable/loadable. This narrows the most common script-injection
  vectors in a Markdown file; it isn't a general claim that viewing any
  untrusted file is safe.
- HTML comments (`<!-- ... -->`), block-level or inline, are discarded
  entirely rather than shown as text.
- **Inline review comments**: in the live view (native window or
  `--browser`, not `--export`), every top-level block — a paragraph,
  heading, list, code block, table, blockquote, etc. — gets a right-hand
  review pane. Click a block to select it (highlighted; blocks that already
  have comments get a left border and a count badge), then write a comment
  and save it (a Save button, or Cmd/Ctrl+Enter). The pane shows the
  selected block's source line range (e.g. `L12-L18`) above its excerpt.
  Comments live in
  `<file>.review.json` next to the Markdown file (e.g. `notes.md` ->
  `notes.md.review.json`) and are matched to blocks by a hash of the
  block's trimmed source, so they survive reloads and reordering elsewhere
  in the document. If a commented block's source changes (or is deleted),
  its comments become "unanchored" — shown in a collapsible section at the
  bottom of the pane, where you can re-attach them to the currently
  selected block or delete them. The **Export** button in the pane header
  writes `<stem>.review.md` (e.g. `notes.md` -> `notes.review.md`) — a
  Markdown summary of every commented block, in document order: each one as
  a one-line quote giving its source line range and excerpt (`> L12-L18:
  ## Design notes`, or `> L40: ...` for a one-line block) followed by its
  comments. The block's full source isn't quoted — for handing a review to
  an AI agent, the line range plus a short excerpt is enough to locate the
  block, and repeating the whole thing again would just be noise. A **Copy
  to clipboard** button then appears so you can put the same text on your
  clipboard in a separate click — handy for pasting a review into an AI
  agent or a PR description. The document itself is never edited by any of
  this. The pane can be resized (drag the divider) or collapsed to a
  slim tab with `⌘\` (Windows/Linux: `Ctrl+\`); `⌘J`/`Ctrl+J` does the same
  thing as an alternative for keyboards where typing a backslash is
  awkward (JIS layouts, notably).
- **File-wide comments**: the pane's breadcrumb has a permanent root
  segment, **ファイル** (with a comment-count badge), for comments on the
  document as a whole rather than any specific block — click it (or press
  Esc, or just don't select anything) to write/edit/delete them the same
  way as block comments. They're saved in the same `.review.json` sidecar
  and counted in the header's total, and Export adds a leading
  `> (file): <name>` section for them ahead of any block sections.
- **List items and table rows** (including the header row) can be
  commented on individually, not just the list/table as a whole — click a
  specific `<li>` or `<tr>` (nesting is respected: clicking a deeply nested
  item selects that item, not its enclosing list or block) to select just
  that item/row instead of the whole block. Selecting an item/row shows a
  breadcrumb above its quote (e.g. `ブロック L10-L20 › 項目 L13`, each
  segment clickable) and a "↑ リスト/表全体にコメント" shortcut back to the
  enclosing block; selecting a block that contains items/rows shows a hint
  that finer-grained comments are available. A block/item with commented
  descendants gets a small "内側に N 件" badge. `Alt+←`/`Alt+→` move to the
  selected anchor's enclosing anchor/first nested anchor; `Alt+↑`/`Alt+↓`
  now move within the same nesting level (sibling items, sibling rows, or
  sibling top-level blocks) rather than always cycling through top-level
  blocks. Exporting a comment on an item/row adds a `（in list L10-L20）`/
  `（in table L40-L48）` suffix to its quote line, naming the enclosing
  list/table's line range.

## Limitations

- Only three routes/paths are served (`/`, `/body`, `/version`) — no other
  files. So a relative-path image (e.g. `![alt](./photo.png)`) renders an
  `<img>` tag that points nowhere and won't display; only `http(s)` image
  URLs actually show up. Relative *links* to other files render fine as
  `<a>` tags, they just won't resolve to anything either, for the same
  reason.
- This isn't just a routing limitation: the page's Content-Security-Policy
  (`img-src data: http: https:`) doesn't include `file:` or `'self'`, so
  even a `--export`ed HTML file opened directly in a browser won't display
  a local/relative-path image — only `http(s)` image URLs work there too.
- Each native window still shows one file at a time — dropping, picking, or
  Finder-opening a file while a window that already has one is frontmost
  opens a *new* window for it rather than replacing that window's content;
  only an empty window (nothing open in it yet) gets filled in place. The one
  exception: if the file is already open in some *other* window, that window
  is brought to the front instead of a new one opening on the same file —
  true across every route that can open a file (a drop, ⌘O, a repeated
  `FILE` on the command line, Finder/"Open With"/`open -a`, and startup
  itself). There's no tabs, no window list menu, and no combined export
  across windows.
- Links in a native window never navigate the window itself: `http(s)`
  and `mailto:` links open in your default browser, and relative links to
  other files are ignored (there's no back button to return from them).
- The native windows share a single minimal menu bar on macOS only (Quit,
  Copy, Select All, Close Window, with the usual Cmd shortcuts) — menu
  actions like Zoom/Reload/Open… apply to whichever window is currently
  frontmost. On Linux and Windows there's no menu bar; use each window's
  close button, and
  `--browser` if you need clipboard shortcuts.
- On Linux the "open file" dialog goes through `xdg-desktop-portal` over
  D-Bus; without a portal/D-Bus session the dialog silently yields nothing
  and `mdview` (no `FILE`) just shows the empty drop-target page.
- The `.app` bundle is macOS-only and ad-hoc signed (no Developer ID, no
  notarization), so it's meant for your own machine. There's no installer
  for Linux or Windows.
- Review comments are per-block/per-item/per-row, not per-cell, per-line, or
  per-selection, and there's no reply thread or resolved/unresolved status —
  just a flat list of comments per anchor. Two anchors with identical
  (trimmed) source hash the same and so share comments; this only matters
  for exact duplicate content — for example, a list item and a table row
  with identical text share a marker/badge state with each other, and a
  single-item list's block-level anchor and its sole item anchor hash
  identically too (they cover the same trimmed source string). `--export`ed
  static HTML has no review pane at all (there's no server for it to talk
  to). The `.review.json` sidecar isn't watched for external changes — edit
  it by hand at your own risk, since the running view won't notice until you
  interact with the pane again.
- The pane header's comment count includes unanchored comments, but the
  Export summary's headline count only covers anchored ones — unanchored
  comments are called out separately there as `(+U unanchored)` — so the
  two numbers can legitimately differ.
- Saving a review from a version of mdview that predates file-wide comments
  discards any existing `file_comments`: `PUT /review` replaces the whole
  sidecar in one request, and an older client's payload simply has no such
  field to send back.
- Footnote reference numbers reset per block (and, within a list/table
  block, per list item/table row), instead of counting up sequentially
  across the whole document. This is because `to_html` renders each block's
  content through several separate `pulldown-cmark::html::push_html` calls
  rather than one call over the whole document (needed so hand-written
  `<li>`/`<tr>` anchor tags can be interleaved with pulldown-cmark's own
  output) — each such call starts pulldown-cmark's internal footnote
  counter fresh, at 1.

## Building

Requires a recent stable Rust toolchain.

```sh
cargo build --release
```

The binary is built at `target/release/mdview`.

### macOS app bundle

To get a double-clickable `mdview.app` that also shows up in Finder's
"Open With" menu for `.md`/`.markdown` files:

```sh
scripts/bundle-macos.sh            # builds target/bundle/mdview.app
scripts/bundle-macos.sh --install  # ...and copies it to /Applications
```

Launched from Finder with no file, the app shows an "Open" dialog (cancel
it to get the empty drop-target page; ⌘O brings it back). Double-clicking a
Markdown file — or dropping one on the app icon — opens it directly. The
icon comes from `packaging/macos/mdview.icns` (regenerate with
`scripts/make-icon.py`, which needs Pillow).

The bundle is built for the machine's native architecture (on Apple Silicon
with a Rosetta-installed rustup, run `rustup target add aarch64-apple-darwin`
first) and ad-hoc signed by default, which is enough to run it. macOS 15+
will not let an ad-hoc signed app become the *default* handler for `.md`
though (Finder's "Change All…" silently reverts), so to make mdview the
default, sign with a real identity:

```sh
security find-identity -v -p codesigning   # list available identities
MDVIEW_SIGN_IDENTITY="Apple Development: you@example.com (TEAMID)" \
  scripts/bundle-macos.sh --install
```

### Linux dependencies

The native window uses WebKitGTK; `--browser` mode has no such requirement.
To build (or run the native window on) Linux, install:

```sh
# Debian / Ubuntu
sudo apt-get install -y libwebkit2gtk-4.1-dev libgtk-3-dev libxdo-dev

# Fedora
sudo dnf install -y gtk3-devel webkit2gtk4.1-devel

# Arch
sudo pacman -S webkit2gtk-4.1 gtk3
```

If WebKitGTK isn't available at runtime, the native window fails to start
with an error message pointing at `--browser` as a fallback.

Saving review comments (`PUT /review`) from the native window needs
WebKit2GTK 2.40 or newer, which is what actually delivers the request body
to `mdview`'s custom-protocol handler on Linux (older WebKit2GTK versions
silently drop it). On an older system, the review pane's Save/Export
actions won't work in the native window; use `--browser` instead, which
isn't affected.

### Windows

No extra system dependencies are needed to *build* on Windows — `wry`
links against WebView2 through the `windows` crate, and the WebView2 SDK
isn't required at build time. At *runtime*, the native window needs the
Microsoft Edge WebView2 Runtime, which ships preinstalled on Windows 11; on
older systems that don't have it, install the [Evergreen
Bootstrapper](https://developer.microsoft.com/microsoft-edge/webview2/).

```sh
cargo build --release
```

produces `target\release\mdview.exe`. Prebuilt zips are also published on
the [Releases](../../releases) page.

`mdview.exe` is built with the GUI subsystem, so launching it from
Explorer or a `.md` file association opens no console window. Launched
from a terminal (cmd/PowerShell/Windows Terminal) instead, it attaches to
that terminal's console, so `--browser`/`--export` still print their usual
output there.

As on Linux, there's no menu bar (macOS only, per Limitations above) — use
each window's close button, and the existing keyboard shortcuts (Ctrl-based,
not Cmd) still work. `--export`'s same-path and symlink self-overwrite
checks apply on Windows too; the additional hard-link check is Unix-only.

### Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## License

MIT. See [`LICENSE`](LICENSE).

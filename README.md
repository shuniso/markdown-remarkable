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
mdview
```

With no `FILE`, a native "open file" dialog appears; pick a `.md`/
`.markdown` file, or cancel to see an empty "drop a Markdown file here"
window. You can also drag & drop a `.md`/`.markdown` file onto the window
at any time (before or after one is already open) to switch to it — the
watcher and window title switch along with it.

```sh
mdview README.md --browser
```

Serves the file over a local HTTP server and opens it in your default
browser instead of a native window — the original CLI's behavior, unchanged.

### Options

| Flag | Default | Description |
|---|---|---|
| `--browser` | off | Show the file in your default browser instead of opening a native window. Requires `FILE`. |
| `--port <PORT>` | `0` | Port to listen on. `0` lets the OS pick a free port. Only applies to `--browser`. |
| `--no-open` | off | Don't open the browser automatically. Only applies to `--browser`. |
| `--export <OUT.html>` | — | Render to a standalone HTML file and exit, instead of showing it live. Requires `FILE`, and is mutually exclusive with `--browser`/`--port`/`--no-open`. Refuses to write over the input file itself. |

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
- The native window supports one file at a time — opening or dropping a new
  one replaces the current one, it doesn't open a second window.
- Links in the native window never navigate the window itself: `http(s)`
  and `mailto:` links open in your default browser, and relative links to
  other files are ignored (there's no back button to return from them).
- The native window has a minimal menu bar on macOS only (Quit, Copy,
  Select All, Close Window, with the usual Cmd shortcuts). On Linux and
  Windows there's no menu bar; use the window's close button, and
  `--browser` if you need clipboard shortcuts.
- On Linux the "open file" dialog goes through `xdg-desktop-portal` over
  D-Bus; without a portal/D-Bus session the dialog silently yields nothing
  and `mdview` (no `FILE`) just shows the empty drop-target page.
- No `.app` bundle, file association, or installer — this is a plain CLI
  binary that happens to open a window.

## Building

Requires a recent stable Rust toolchain.

```sh
cargo build --release
```

The binary is built at `target/release/mdview`.

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

### Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## License

MIT. See [`LICENSE`](LICENSE).

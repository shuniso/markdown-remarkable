# markdown-remarkable

A small Rust CLI, `mdview`, that renders a Markdown file to GitHub-flavored
HTML, serves it on `localhost`, and reloads your browser automatically every
time you save the file. Nothing more: no multi-file navigation, no syntax
highlighting, no editing. Just "write, save, see it update."

## Usage

```sh
mdview README.md
```

This starts a local server, prints the URL it's listening on, and opens it
in your default browser. Edit and save the file — the browser reloads on
its own. Deleting the file is detected the same way as any other save: the
browser reloads automatically and shows a `500` error page instead of the
old content, then reloads again on its own once the file exists (and is
readable) again.

### Options

| Flag | Default | Description |
|---|---|---|
| `--port <PORT>` | `0` | Port to listen on. `0` lets the OS pick a free port. |
| `--no-open` | off | Don't open the browser automatically. |
| `--export <OUT.html>` | — | Render to a standalone HTML file and exit, instead of starting a server. Mutually exclusive with `--port`/`--no-open` (both only apply to the server), and refuses to write over the input file itself. |

Examples:

```sh
# Pick a fixed port and skip opening a browser
mdview notes.md --port 8080 --no-open

# Render to a static HTML file (no live reload, no server)
mdview notes.md --export notes.html
```

## Features

- GitHub-style rendering: tables, strikethrough, task lists, and footnotes
  are all supported.
- Light and dark themes, following the browser/OS's `prefers-color-scheme`.
- Live reload on save (directory-watch based, so it works with editors that
  save "atomically" via a temp file + rename).
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

- The server only handles two routes, `/` (the rendered page) and
  `/version` (for live-reload polling) — it doesn't serve any other files.
  So a relative-path image (e.g. `![alt](./photo.png)`) renders an `<img>`
  tag that points nowhere and won't display; only `http(s)` image URLs
  actually show up. Relative *links* to other files render fine as `<a>`
  tags, they just won't resolve to anything either, for the same reason.
- This isn't just a server limitation: the page's Content-Security-Policy
  (`img-src data: http: https:`) doesn't include `file:` or `'self'`, so
  even a `--export`ed HTML file opened directly in a browser won't display
  a local/relative-path image — only `http(s)` image URLs work there too.

## Building

Requires a recent stable Rust toolchain.

```sh
cargo build --release
```

The binary is built at `target/release/mdview`.

### Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## License

MIT. See [`LICENSE`](LICENSE).

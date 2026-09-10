# Security

This document summarizes the results of a security audit performed on `markdown-remarkable`.

## What is guaranteed

- Everything runs entirely locally. The app itself never sends anything to an external network.
  - No HTTP client crate (`reqwest`/`hyper`/`ureq`, etc.) is among the dependencies.
  - The only mode that opens a socket at all is `--browser`, and it only ever binds to `127.0.0.1:<port>` (loopback). The native window and `--export` never open a socket.
  - There is no telemetry, update checking, crash reporting, or anything of that kind.

### Files read

- Only these 5 kinds of files are ever read:
  - The Markdown or plain-text file that's currently open (`.md`/`.markdown`/`.txt`
    — see `util::file_kind`).
  - Its sidecar review file, `<file>.review.json`, next to it.
  - `window.json`, which stores window position/size (under the OS's own config directory).
  - Image files under the open file's parent directory (`GET /asset?p=...`, live view only
    — never happens in `--export`). An extension allowlist
    (png/jpg/jpeg/gif/webp/bmp/svg/avif/ico) applies, `..`/absolute paths/symlink escapes
    are rejected (checked by confirming the `canonicalize`d result has the parent
    directory's own `canonicalize`d result as a prefix), and there's a 20 MiB cap.
    - **Hard links are not detected**: a symlink resolves to its target through
      `canonicalize`, so the escape check catches it, but a hard link is a separate
      path pointing at the same inode, which `canonicalize` cannot tell apart. If a
      file with an allowlisted extension inside the document's parent directory is
      actually a hard link to a file outside it, that file can be read. Be cautious
      when extracting an untrusted archive and opening a Markdown file from inside it
      (though the attack surface is limited in practice, since a hard link can only be
      created within the same filesystem, so an attacker planting one arbitrarily is a
      fairly constrained scenario).
    - **No protection against a TOCTOU race between `canonicalize` and the actual
      read**: there is no defense against a race where a path that pointed inside the
      target directory at check time gets swapped out — after `canonicalize` but
      before `fs::metadata`/`fs::read` runs — (e.g. the target is deleted and replaced
      by a same-named symlink pointing outside the directory). This is only a
      meaningful threat in an environment where another process on the same machine
      can manipulate the filesystem at that exact moment (the same exposure `--browser`
      already carries).
  - Other `.md`/`.markdown`/`.txt` files under `root_dir` (the parent directory of the
    **first file that window ever opened**, canonicalized, and fixed for as long as
    the window keeps that root — never recomputed as a *side effect* of a later file
    switch, so `GET /tree`'s listing and `PUT /open`'s switch target both stay
    confined to that same directory across ordinary navigation. It can still move,
    but only through one explicit, user-driven action — see "Re-establishing
    `root_dir`" below, which is the *only* other way it's ever written). There are two ways to
    reach one: clicking an entry in the left-hand file tree, and clicking a relative
    link in the document body (`.md`/`.markdown` only, `assets/viewer.js`). The latter
    means the input feeding `path` in `PUT /open` now ranges from "the list this app
    itself rendered as a tree" all the way to "the currently open document's own body
    (i.e. arbitrary text written by that file's author)" — but the validation applied
    to `path` (below) is exactly identical on either path; the document's own input
    gets no special treatment. Validation:
    - Only `Component::Normal` components (reusing the same function used for
      images), and only an extension `util::file_kind` recognizes
      (`.md`/`.markdown`/`.txt`).
    - Confirms the target path itself is not a symlink via `symlink_metadata` (before
      `canonicalize` — this rejects even a symlink whose target would otherwise land
      inside the allowed range. This is stricter than the escape check below, to keep
      it consistent with `GET /tree` never listing symlinks at all).
    - Confirms the `canonicalize`d result has `root_dir`'s own `canonicalize`d result
      as a prefix (the same escape check used for images).
    - Confirms the `canonicalize`d target is `fs::metadata(..).is_file()` (this keeps
      a directory or a FIFO/device node named with a `.md` extension from getting
      grabbed and blocking the UI thread inside `read_to_string`).
    `GET /tree`'s exclusion list (hidden folders, `node_modules`, etc.) and depth limit
    exist purely for display purposes and have no bearing on what's actually permitted
    here. Once switched to, that file becomes "the Markdown or plain-text file
    that's currently open" and is treated exactly like the first bullet point
    from here on (no size cap, and the same lack of hard-link detection/TOCTOU
    protection as the other bullets applies).
  - `GET /tree` (also for the left-hand file tree) never reads file **contents**, but
    it does walk the directory tree under `root_dir` up to depth 4 and up to 2000
    entries (past which it returns `truncated: true`), listing directory and file
    names that include `.md`/`.markdown`/`.txt`. It also caps the raw number of
    **visited** entries (before pruning) at 20000, so that scanning a huge directory
    tree containing no Markdown or plain-text file at all can't tie up the UI
    thread for a long time even though none of it ever shows up in the response.
    Hidden directories (starting with `.`), `node_modules`, and `target` are
    skipped, and symlinks are detected via
    `DirEntry::file_type` (equivalent to `symlink_metadata`) and never followed.
    Non-UTF-8 file/directory names are excluded from the listing (a `to_string_lossy`
    rendering with embedded U+FFFD characters can't be opened via `PUT /open` anyway,
    so showing it would serve no purpose).

### Files written

- Only these 4 kinds of files are ever written:
  - The review sidecar `<file>.review.json` (`PUT /review`).
  - The review export (`POST /export` / the panel's Export button): `<stem>.review.md`
    for a Markdown file, or `<name>.txt.review.md` (the full file name) for a
    plain-text file — see `review::export_path`.
  - `window.json` (debounced and saved after the window is moved/resized).
  - `--export`'s output file, `OUT.html`.
  - Every one of these is written by creating a tmp file with `OpenOptions::create_new`
    and then renaming it into place. Even if a symlink with the same name as the
    existing file has been planted, its target is never followed and overwritten
    (`create_new` fails outright if anything already exists there).

### State-changing routes

- `PUT /nav` (the document header's Back/Forward buttons, `⌘[`/`⌘]`) is a state-change route
  that only ever moves the native window's own internal "back/forward history for
  that window" (a sequence of `.md`/`.markdown`/`.txt` paths under `root_dir` — never
  written to disk, and gone once the window closes) one step. The request carries no
  path at all (the body is just `{"dir":"back"|"forward"}`) — where it moves to is
  always decided by the history the server itself already holds. Paths land in that
  history from two sources: (a) the file that window **first opened** (whatever the
  user explicitly specified via a CLI argument, ⌘O, drag-and-drop, or Finder's
  "Open" — since this single input is what defines `root_dir` itself, it never goes
  through `PUT /open`'s validation, but by definition it's still within `root_dir`,
  since `root_dir` is defined as that very file's parent directory), and (b) every
  subsequent switch (via the tree, a relative link, or a direct `PUT /open` call),
  which does go through `PUT /open`'s validation and lands as a canonical path under
  `root_dir`. Either way, `PUT /nav` only **replays** that history entry and never
  re-validates it — if the path was swapped out for a symlink after being visited, a
  replay will read through to that symlink's target, which is a remaining TOCTOU. This
  is accepted as the same class of threat as the existing "live reload of the
  currently open file" (`watch` doesn't re-validate on reload either). Like the other
  state-changing routes (`PUT /review`, `POST /export`, `PUT /open`), it requires the
  `X-Mdview-Request` header (missing it returns `403`), and it always answers `501`
  under `--browser` (only meaningful in the native window — `--browser` has no notion
  of per-window history at all).
- `PUT /pick-root` (the file tree's folder button, or ⌘⇧O) re-establishes the current
  window's `root_dir` — the one deliberate exception to "fixed for the lifetime of the
  window" described above. It requires the `X-Mdview-Request` header (missing it
  returns `403`) and always answers `501` under `--browser` (which has no window/root
  concept to reassign), same as `PUT /open`/`PUT /nav`. The important property: **this
  route never reads its own request body at all** — there is no `path`/`root`/anything
  else it looks for in there, so a client cannot choose the new root by sending one
  over HTTP/the custom protocol, no matter what the body contains (a request with a
  JSON body naming an arbitrary path behaves identically to one with no body at all).
  A successful request only ever gets a `202` back and a promise that the native app
  will, on its own event loop, open the OS's native "choose a folder" dialog; the path
  that actually becomes the new `root_dir` is whatever the user picks in *that*
  dialog, never anything that arrived as request input. This makes re-establishing
  `root_dir` exactly as trustworthy as establishing it the first time (the window's
  very first file, via a CLI argument, ⌘O, drag-and-drop, or Finder's "Open" —
  see above): both are values a human chose through an OS-owned picker, never a string
  parsed out of a network-facing request. Once the dialog resolves: if the window's
  currently open file is still inside the new root, it's left completely alone (no
  reload — scroll position/selection/review highlighting all survive) and only the
  tree pane (and the doc header's back/forward buttons/path label) refresh; otherwise
  the *document* pane falls back to its empty "no file open" state — but, unlike a
  truly new window (which has no `root_dir` at all yet, so `GET /tree` answers `409`
  and shows nothing), this window already has `root_dir: Some(<new root>)` at that
  point, so its file tree keeps showing the new root's contents rather than going
  blank too. Either way, that window's `PUT /nav` history is
  cleared — every entry in it names a path under the *old* root, and (per the history
  bullet above) `PUT /nav` never re-validates a history entry against the current
  `root_dir`, so keeping old entries around after the boundary itself moved would
  reopen exactly the class of risk re-validation exists to avoid.

## Exceptions (deliberately outside the guarded surface)

1. **`http(s):`/`mailto:` links are handed off outside the app**: clicking one launches
   the OS's default browser/mail client. This remains true even once the native
   window grew a document header (Back/Forward, `⌘[`/`⌘]`) — not because there's "no back
   button", but because the window's WebView is deliberately scoped to displaying
   only the currently open document (plus relative link targets under `root_dir`),
   and is never meant to become a route for loading arbitrary web content — that's a
   design boundary, not an accident of missing features. What happens after the
   handoff is outside `markdown-remarkable`'s control. Relative links (`.md`/`.markdown`
   only) are not part of this handoff — they go through the same validation as
   `PUT /open`, within the same `root_dir`, staying inside the app the whole time (see
   the reading-surface section above).
2. **External images are blocked by default**. The Content-Security-Policy's `img-src`
   defaults to `data: 'self'` only, so `http(s)` images referenced in a document are
   never requested and show up as a broken `<img>` (`'self'` exists to allow the
   same-origin `/asset` route — the local images above — not to permit requests to
   external hosts). Only passing `--allow-remote-images` makes `http(s)` images load,
   which exposes the viewer's IP address to whatever host the image URL points at
   (an opt-in, for the same reason mail/RSS clients guard against "tracking pixels").
3. **While `--browser` is running, any other process on the same machine can read and
   write the document and its comments over `127.0.0.1:<port>`**. There is no
   authentication at all (the `X-Mdview-Request` header is a CSRF mitigation that
   relies on the browser's own CORS preflight — it is not an authentication
   mechanism). If your use case doesn't assume other users/processes on the same
   machine might access it, avoid `--browser`, or only use it in a trusted
   environment. The native window (the default mode) has no such exposure.
4. **Communication between the WebView engine itself (WebView2 / WKWebView /
   webkit2gtk) and the OS is outside `markdown-remarkable`'s control**. Windows'
   WebView2 Runtime in particular may perform its own communication — such as its own
   auto-update — that's invisible to, and uncontrollable by, `markdown-remarkable`.

## Logging

- Logs only ever go to stderr. Nothing is ever written to a log file.
- Warning messages may include a file's full path (e.g. `warning: failed to read
  <path>: ...` on a read failure). Be mindful of wherever stderr's output ends up.
- Setting `MDVIEW_DEBUG=1` prints one line per request to stderr in the native
  window only; `--browser` mode does not log requests. Each line has the method,
  path, status, and (for `PUT`/`POST` only) the body's byte count. The body's
  actual contents are never printed.
- Client-supplied values such as `doc.file` are never logged as-is (only their
  length). One exception: when a `PUT /review` body fails to parse, the serde
  error message is logged and may quote a fragment of the body; control
  characters in it are escaped.

## Environment variables

`markdown-remarkable` reads only these 4 environment variables:

- `HOME` — used to resolve where `window.json` is stored on macOS/Linux.
- `XDG_CONFIG_HOME` — used to resolve where `window.json` is stored on Linux
  (falls back to `$HOME/.config` if unset).
- `APPDATA` — used to resolve where `window.json` is stored on Windows.
- `MDVIEW_DEBUG` — enables the request logging described above (any value works; only
  whether it's set at all is checked).

## Reporting a vulnerability

Please report security vulnerabilities using GitHub's private vulnerability reporting
(open the repository's Security tab and choose "Report a vulnerability") rather than a
public issue. If that isn't available to you, open an issue asking to be contacted
without including any vulnerability details in it, and we'll follow up privately.

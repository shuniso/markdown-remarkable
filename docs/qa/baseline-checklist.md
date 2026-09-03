# QA checklist: baseline UX

Manual verification items for the app's baseline UX (pane resize/collapse, zoom,
keyboard shortcuts, window state, light/dark).
Run through this at least once on both the native window (`markdown-remarkable FILE`)
and `--browser` mode, and in both light and dark OS themes.

Anything automatable (`.splitter` being embedded, the `viewer.js`/`review.js` script
insertion order, `window_state`'s pure functions, routes/render responses) has already
been moved into `cargo test`; only what can only be verified on a real device/browser
is listed here.

## 1. Pane resize and collapse

- [ ] Dragging the splitter between `.doc` and `.review` changes the right pane's width.
- [ ] The splitter shows `cursor: col-resize` (equivalent to a 6px width at the
      default zoom level — it's `0.375rem`, so it gets thicker along with everything
      else as you zoom).
- [ ] Dragging doesn't break off even if the cursor leaves the splitter and passes
      over `.doc`/`.review` (uses `setPointerCapture`).
- [ ] Releasing the button after the pointer has left the window entirely during a
      drag still ends the drag cleanly instead of leaving it "stuck" (a later click
      shouldn't move the width on its own).
- [ ] Body/pane text isn't selectable while dragging (`user-select: none`).
- [ ] Dragging the width all the way left never goes below 240px.
- [ ] Dragging the width all the way right never exceeds 60% of the window width.
- [ ] The pane header's "⟩" button collapses it.
- [ ] While collapsed, a vertical tab appears at the right edge of the screen
      (equivalent to a 28px width at the default zoom level — it's `1.75rem`) reading
      "Review · N" (N being the comment count).
- [ ] Block markers (comment-count badges) and click-to-select still work while
      collapsed.
- [ ] Clicking a block while collapsed automatically expands the pane.
- [ ] `⌘\` (`Ctrl+\` on Windows/Linux) toggles open/closed.
- [ ] `⌘J` (`Ctrl+J` on Windows/Linux) toggles it the same way (an alternate binding
      for layouts, like JIS, where backslash is awkward to type).
- [ ] On a layout with an AltGr key (e.g. a German layout), typing ordinary characters
      that involve AltGr doesn't misfire the collapse/reload shortcuts.
- [ ] Narrowing the pane, collapsing it, widening the window, then expanding it again:
      the width doesn't stay at its old wide value — it fits within 60% of the current
      window width.
- [ ] Reloading (`⌘R`) after changing the width/collapsed state restores the same
      state.
- [ ] With `localStorage` disabled (e.g. a private window), nothing errors and it
      falls back to the defaults (320px, expanded).
- [ ] Narrowing the window below 720px automatically fits a width that had been set
      wider (no horizontal scrolling or broken layout shows up in the console).

## 2. Zoom

- [ ] `⌘+` / `⌘=` (`Ctrl` on Windows/Linux) enlarges the body and review pane text.
- [ ] `⌘-` shrinks it.
- [ ] `⌘0` resets to 100%.
- [ ] It never goes below 0.5x or above 2.0x.
- [ ] The zoom level from before a reload is restored after reloading (`localStorage`
      key `mdview.zoom`).
- [ ] The zoom level survives a live update (saving the file → body replacement).
- [ ] Native window (macOS): the View menu's Zoom In / Zoom Out / Actual Size / Reload
      all work.
- [ ] On the macOS native window, mashing both the menu action and its keyboard
      shortcut doesn't double up (one action still only moves it one step).
- [ ] Zoom via the keyboard shortcuts also works in `--browser` mode (Safari/Chrome,
      etc. — there's no menu, so only the key shortcuts apply).
- [ ] On a layout with an AltGr key, typing ordinary characters that involve AltGr
      doesn't misfire zoom (the zoom shortcuts are ignored while `event.altKey` is
      set).

## 3. Basic interactions

- [ ] With focus in the comment textarea, pressing Esc removes focus (the selection
      isn't cleared yet).
- [ ] Pressing Esc again in that state clears the block selection.
- [ ] `Alt+↓` / `Alt+↑` selects the next/previous anchor at the same level (block to
      block while a block is selected, sibling item to sibling item while an item is
      selected, sibling row to sibling row within the same thead/tbody while a row is
      selected). Works even while a textarea has focus.
- [ ] Selecting a block via the keyboard automatically scrolls it into view if it's
      off-screen.
- [ ] Pressing "Reanchor to selected block" in the Unanchored list automatically
      scrolls to the target block if it's off-screen.
- [ ] Pressing `⌘R` / `Ctrl+R` on the JS side (including while a textarea has focus)
      reloads the page.
- [ ] Saving a comment shows `Saving…` then `Saved` in the pane header, and `Saved`
      disappears after about 2 seconds.
- [ ] Making the save fail (e.g. by killing the server) shows `Save failed — retry` in
      the header, and it stays there.
- [ ] Clicking `Save failed — retry` retries the save (switching to `Saved` on
      success).

## 4. Windows (macOS native)

- [ ] Trying to resize the window smaller than `480×320` never shrinks it below that.
- [ ] After moving/resizing the window and waiting a moment, confirm with `cat` that
      `~/Library/Application Support/markdown-remarkable/window.json` has been updated.
- [ ] After closing the app (red traffic-light button / `⌘W` / `⌘Q`) and then running
      `cat`, the position/size at the moment it closed is what's saved.
- [ ] Quitting via `⌘Q` (or Quit from the Dock) also updates `window.json` with the
      latest position/size (since `⌘Q` doesn't always go through the window's
      `CloseRequested`, confirm the path that also saves on `LoopDestroyed`).
- [ ] Restarting with a saved position/size reopens at that position/size (repeating
      launch → quit → relaunch several times shouldn't drift the position/size by the
      title bar's height each time).
- [ ] If the OS nudges/resizes the window for its own reasons right after launch
      (within 1 second), that alone shouldn't rewrite `window.json` (the debounce
      doesn't start until 1 second after launch, by design).
- [ ] Launching after an external display used at save time has been disconnected
      doesn't leave the window off-screen — it opens somewhere reasonable (only the
      size is restored; the position is ignored).
- [ ] Launching with a corrupted `window.json` doesn't crash with an error — it opens
      at the default size (a warning printed to stderr is fine).
- [ ] Launching with `MDVIEW_DEBUG=1` prints `[markdown-remarkable:js] storage: ok` (or
      `unavailable ...`) to stderr right after startup (`assets/viewer.js`'s
      localStorage-availability probe, via `window.ipc`).

## 5. Empty/failure states

- [ ] Launching the native window without specifying a file, then cancelling the file
      picker, shows only "Open a file to see its review here" in the right pane, with
      no comment form.
- [ ] Opening a file by dragging and dropping it from that state switches the right
      pane over to the normal review UI.
- [ ] Making the load fail (e.g. by deleting the file being shown externally) shows
      the existing error banner and disables saving.
- [ ] Clicking "Retry" in that state retries `GET /review` (restoring normally if the
      file is put back first).

## 6. Light/dark

- [ ] Toggling OS dark mode doesn't break any of the elements above (splitter,
      collapse tab, save indicator, banner, placeholder) — including contrast and
      legibility.

## 7. Selecting list items/table rows and export (nested anchors)

Manual verification items for selecting and commenting on list items/table rows (see
README's "Review comments and export" section). Verify with a Markdown
file containing a nested list (about 3 levels deep) and a table with a header row.

- [ ] Clicking a list's `<li>` selects only that item (not the enclosing block as a
      whole). Even for a nested item, the exact item clicked gets selected.
- [ ] Clicking a table row's `<tr>` (including the header row) selects only that row.
- [ ] Selecting an item/row shows a breadcrumb in the right pane (e.g. `Block L10-L20
      › Item L13`), and clicking any segment selects that level.
- [ ] While an item is selected, an "↑ Comment on whole list" button appears; while a
      row is selected, "↑ Comment on whole table" appears — clicking it selects the
      parent block.
- [ ] While a block containing items/rows is selected, the "Click to select an item"
      hint appears.
- [ ] Commenting on an item/row shows a left bar plus a count badge on the
      corresponding `<li>`/`<tr>` (a row's badge appears on its last cell, by design).
- [ ] A block/item whose nested items/rows have comments shows a faint "N inside"
      badge.
- [ ] `Alt+←` moves from the selected item/row to its parent (item → its parent item
      or block; row → block).
- [ ] `Alt+→` moves from the selected block/item to its first nested anchor (its first
      item/row).
- [ ] Commenting on an item/row and exporting adds a `(in list L10-L20)` /
      `(in table L40-L48)` note to that line in the exported Markdown (for a nested
      item, this is the whole list's own line range, not just its immediate parent
      item's).
- [ ] Item/row selection and comment markers are correctly restored across a live
      reload (saving the file → body replacement).

## 8. File-wide comments

Manual verification items for file-wide comments (see README's "Review comments and
export" section).

- [ ] Right after opening a file (the initial state, with nothing selected), the right
      pane shows a "Comments on the whole file" heading and an input field (the
      "Click a block" hint stays small, below the input field).
- [ ] The breadcrumb has a permanent "File" segment at its root, and shows as
      "File › Block L…-L… › Item L…" while a block/item/row is selected.
- [ ] Clicking "File" in the breadcrumb returns to file-wide mode (clearing the
      selection).
- [ ] Pressing Esc while a block is selected returns to file-wide mode.
- [ ] Entering a file-wide comment and saving it (the Save button, or Cmd/Ctrl+Enter)
      adds it to the list and issues `PUT /review`.
- [ ] File-wide comments can be edited and deleted (the same interaction as block
      comments).
- [ ] The header's "N comments" count includes file-wide comments.
- [ ] The breadcrumb's "File" segment shows a count badge that tracks the number of
      file-wide comments.
- [ ] File-wide comments never get a marker (left bar, badge, etc.) on the document
      body itself (there's nothing to attach one to).
- [ ] Whichever was active — a selected block, or file-wide mode — is preserved across
      a live reload (saving the file → body replacement).
- [ ] On export, if there are any file-wide comments, a `> (file): <file name>`
      section is added at the top of the exported Markdown, and the count line reads
      `N comments (K on the file, M on B blocks)` (without any file-wide comments it
      stays `M comments on B blocks` as before).
- [ ] Pressing "Edit" on a comment in file-wide mode, then pressing Esc twice with the
      textarea focused: the first Esc only removes focus, and the second cancels the
      edit and returns to the normal comment list (the textarea's contents don't stick
      around).
- [ ] With some unanchored comments present, compare the header's "N comments" (which
      includes the unanchored ones) against the exported Markdown's own count line at
      the top (anchored comments only, plus `(+U unanchored)`), and confirm that
      `header's N = the exported count line's anchored portion + U`.

## 9. Multiple windows (macOS native)

Manual verification items for opening each file in its own window (see README's "File
tree and multi-window navigation" section).

- [ ] Running `markdown-remarkable a.md b.md` opens two separate windows, one per file
      (titled `a.md — markdown-remarkable` and `b.md — markdown-remarkable`
      respectively).
- [ ] Running with no arguments opens a single empty window with a file picker (if
      cancelled, it stays as the empty "Drop a Markdown file here" state).
- [ ] With one window already open, double-clicking a different file in Finder opens
      one more new window (the existing window's contents don't change).
- [ ] Opening from Finder a file that's already open in a different window doesn't
      open a new window — it brings the existing window to the front instead.
- [ ] Pressing ⌘O in a window that has a file open opens a new window (the original
      window's contents don't change).
- [ ] Pressing ⌘O in an empty window (no file selected) doesn't open a new window — the
      file opens in that same window.
- [ ] Dragging and dropping a `.md` file onto a window that already has a file open
      opens a new window (the drop target window's own contents don't change).
- [ ] Dragging and dropping a `.md` file onto an empty window opens it in that same
      window.
- [ ] With multiple windows open, closing just one (⌘W or the red traffic-light
      button) leaves the other windows and the app itself running.
- [ ] Closing the very last window quits the whole app.
- [ ] Pressing ⌘Q with multiple windows open closes all of them and quits the app.
- [ ] A newly opened window appears cascaded — offset down and to the right from the
      previous window's position.
- [ ] Using the macOS View menu (Zoom In/Out/Actual Size/Reload) only affects whichever
      window is currently frontmost among the open windows (background windows' zoom
      level/reload state don't change).
- [ ] Switching windows and then using the View menu affects whichever window is now
      frontmost after the switch.
- [ ] After moving/resizing any window, check
      `~/Library/Application Support/markdown-remarkable/window.json` — it holds the position/size
      of whichever window was moved most recently.
- [ ] Quitting with ⌘Q while multiple windows are open saves the position/size of
      whichever window was frontmost into `window.json`.
- [ ] Dragging and dropping, or choosing via ⌘O, a file that's already open into a
      different window doesn't open a new window — it just brings that existing
      window to the front (running `markdown-remarkable a.md a.md` at launch also
      results in a single window, the same way).
- [ ] After opening and cascading several windows, closing one in the middle and then
      opening a new file still opens it somewhere that doesn't fully overlap any
      remaining window (the cascade offset doesn't roll back just because a window in
      the middle was closed).

## 10. File navigation (relative links, back/forward, native window only)

- [ ] Clicking a relative `.md` link in the document body switches the same window to
      the linked file.
- [ ] After switching, the document header's ◀ becomes enabled. Pressing ◀ returns to
      the original file, at which point ▶ becomes enabled.
- [ ] After navigating back and forth a few times, ◀/▶'s enabled/disabled state
      updates correctly each time (disabled at either end of the history).
- [ ] `⌘[`/`⌘]` (`Ctrl+[`/`Ctrl+]` on Windows/Linux) also move back/forward the same
      way, staying in sync with the buttons' enabled state.
- [ ] With focus in the comment textarea, pressing `⌘[` doesn't affect the textarea's
      contents or focus (the shortcut is a no-op there).
- [ ] A link containing `../` that crosses a subdirectory boundary under `root_dir`
      (e.g. `../b.md` from `sub/a.md`) switches to the correct file.
- [ ] Clicking a link containing `../` that would climb outside `root_dir` (e.g.
      `../outside.md` from the top-level file, if one exists) does nothing (it never
      mistakenly switches to a same-named file in a different directory).
- [ ] A relative link to a file with non-ASCII characters or spaces in its name (e.g.
      `café note.md`) opens correctly.
- [ ] Clicking a relative `.md` link while holding a modifier key (any of ⌘/Ctrl/Shift/
      Alt) does nothing (it never falls through to a 404 page).
- [ ] Clicking a relative link to something other than `.md`/`.markdown` (e.g.
      `notes.txt`) does nothing (it never falls through to a 404 page).
- [ ] Clicking a link with a `#fragment` (e.g. `other.md#section`) switches to
      `other.md` with the fragment simply ignored (no auto-scroll to a heading).
- [ ] The document header stays fixed at the top of the screen while scrolling a long
      document (sticky).
- [ ] Zooming up to 200% with `⌘+`/`⌘=` doesn't overflow the document header's own
      buttons/path label outside the header.
- [ ] In window A, open X.md → navigate to Y.md via a relative link in the body (A's
      history is now [X, Y]) → open X.md via ⌘O or Finder (a different window B
      displays the same X.md content) → press ◀ in A. Expected: B comes to the front
      and A itself stays on Y.md without switching (one file per window, to avoid a
      review sidecar conflict — see the Limitations section of README.md). A's ◀ stays enabled,
      ▶ stays disabled. Pressing ◀ in A again afterward just repeats the same result
      without breaking (the history cursor never ends up in an inconsistent state).
- [ ] After opening a file via the left-hand file tree, pressing ◀ in the document
      header returns to the original file (a switch via the tree gets added to the
      history the same way a relative link does).
- [ ] In an empty window with no file open, the document header's ◀/▶ are both
      disabled and the path field is empty.
- [ ] In `--browser` mode, the document header's ◀/▶ buttons don't appear at all.
- [ ] Clicking a relative `.md` link in `--browser` mode doesn't switch anything
      (left to the browser's own default behavior — a 404 is fine, and the browser's
      own back button recovers from it).
- [ ] Clicking an `http(s)` link in `--browser` mode opens a new tab, leaving the
      original tab as-is.
- [ ] Clicking `[x](/other.md)` (root-relative) or `[x](//host/other.md)`
      (protocol-relative) in the native window does nothing (it never falls through to
      an in-app 404). In `--browser` mode this is left to the default behavior, and
      landing on a 404 in the same tab is fine.
- [ ] Middle-clicking a relative `.md` link doesn't navigate or open a new
      window/tab (a right-click's context menu still appears as usual).
- [ ] Clicking a relative link containing `%2f` (`/`) or `%5c` (`\`) does nothing (it
      never resolves across a segment boundary into a different directory).
- [ ] Regression check for existing features: clicking a `mailto:` link launches the
      mail client. Clicking a footnote reference `[^1]` scrolls to that footnote's
      definition on the same page (unaffected by the relative-link interception).
- [ ] Deleting the file that ◀ would go back to beforehand: pressing ◀ lands on an
      error page, but opening a different file from the file tree recovers from
      there.

(function () {
  "use strict";

  // Block-level (and, for lists/tables, nested item/row-level) review
  // comments: a right-hand pane that lets you attach comments to Markdown
  // blocks, list items, and table rows (identified by the `data-hash` the
  // server stamps on each `.blk`/`.anchor` element — see render.rs),
  // persist them via GET/PUT /review, and export them as a Markdown review
  // summary via POST /export.

  var REVIEW_URL = "/review";
  var EXPORT_URL = "/export";
  var REQUEST_HEADERS = { "X-Mdview-Request": "1" };

  // Pane resize/collapse (section 1 of the baseline UX design). Persisted
  // separately from anything in `state` below — width/collapsed are UI
  // chrome, not review data, and must survive independently of whether
  // GET /review has ever succeeded.
  var PANE_WIDTH_KEY = "mdview.review.width";
  var PANE_COLLAPSED_KEY = "mdview.review.collapsed";
  var PANE_DEFAULT_WIDTH = 320;
  var PANE_MIN_WIDTH = 240;
  var SAVE_STATUS_CLEAR_MS = 2000;

  var state = {
    doc: { version: 1, file: "", blocks: [], file_comments: [] },
    unanchored: [],
    selectedHash: null,
    editingId: null,
    loading: true,
    // Only true once GET /review has succeeded at least once. Every
    // mutation (comment CRUD, reanchor) and saveReview() itself refuse to
    // run while this is false, so a load failure can never be followed by
    // a PUT that clobbers a sidecar this page never actually saw — see
    // saveReview() and render().
    loaded: false,
    // True only for the specific "no file open yet" case (GET /review ->
    // 409) — distinct from a genuine load failure: the aside shows a plain
    // "open a file" placeholder instead of the failure banner + retry
    // button. See renderAside() and onBodyReplaced().
    noFileOpen: false,
    error: null,
    toast: null,
    unanchoredOpen: false,
    // PUT /review's own status, independent of `error` (which is only ever
    // about GET /review): null | "saving" | "saved" | "failed". Drives the
    // header's Saving…/Saved/"Save failed — retry" indicator.
    saveStatus: null,
  };

  var saveStatusTimer = null;

  var paneState = {
    width: PANE_DEFAULT_WIDTH,
    collapsed: false,
    dragging: false,
  };

  var asideEl = null;
  var layoutEl = null;
  var splitterEl = null;
  var collapseTabEl = null;

  // -- DOM helpers, all textContent-based so nothing here ever builds
  //    HTML strings out of comment text/excerpts (both are user input). --

  function el(tag, className, text) {
    var node = document.createElement(tag);
    if (className) {
      node.className = className;
    }
    if (text !== undefined && text !== null) {
      node.textContent = text;
    }
    return node;
  }

  function clearChildren(node) {
    while (node.firstChild) {
      node.removeChild(node.firstChild);
    }
  }

  function button(className, text, onClick) {
    var btn = el("button", className, text);
    btn.type = "button";
    btn.addEventListener("click", onClick);
    return btn;
  }

  // -- localStorage, wrapped in try/catch: a disabled or full localStorage
  //    must never throw its way into a broken pane — just fall back to
  //    the defaults. --------------------------------------------------

  function safeStorageGet(key) {
    try {
      return window.localStorage.getItem(key);
    } catch (err) {
      return null;
    }
  }

  function safeStorageSet(key, value) {
    try {
      window.localStorage.setItem(key, value);
    } catch (err) {
      // ignore — persistence is best-effort.
    }
  }

  // -- pane resize/collapse -------------------------------------------

  function maxPaneWidth() {
    return window.innerWidth * 0.6;
  }

  // Keeps the effective minimum from ever exceeding the effective maximum
  // (a narrow window can push `maxPaneWidth()` below PANE_MIN_WIDTH) so the
  // clamp itself can never produce an inverted [min, max) range.
  function clampPaneWidth(width) {
    var max = maxPaneWidth();
    var min = Math.min(PANE_MIN_WIDTH, max);
    return Math.min(Math.max(width, min), max);
  }

  function applyPaneWidth(width) {
    paneState.width = width;
    if (asideEl) {
      asideEl.style.width = width + "px";
    }
  }

  function persistPaneWidth() {
    safeStorageSet(PANE_WIDTH_KEY, String(paneState.width));
  }

  function loadStoredPaneWidth() {
    var raw = safeStorageGet(PANE_WIDTH_KEY);
    var parsed = raw === null ? NaN : parseFloat(raw);
    // The default also goes through clampPaneWidth: a narrow window at
    // first load can already put PANE_DEFAULT_WIDTH (320) above the 60%
    // cap, same as any stored value would be.
    return clampPaneWidth(isNaN(parsed) ? PANE_DEFAULT_WIDTH : parsed);
  }

  function loadStoredPaneCollapsed() {
    return safeStorageGet(PANE_COLLAPSED_KEY) === "1";
  }

  function updateCollapseTabLabel() {
    if (collapseTabEl) {
      collapseTabEl.textContent = "Review · " + totalCommentCount();
    }
  }

  function applyPaneCollapsed(collapsed) {
    paneState.collapsed = collapsed;
    if (!collapsed) {
      // The window may well have been resized narrower while the pane sat
      // collapsed (onPaneWindowResize() skips reclamping collapsed panes
      // entirely — see there), so a stored width that was valid when it
      // collapsed could now exceed the current 60% cap. Reclamp on the
      // way back out rather than on every resize tick nothing is even
      // showing yet.
      applyPaneWidth(clampPaneWidth(paneState.width));
      persistPaneWidth();
    }
    if (layoutEl) {
      layoutEl.classList.toggle("review-collapsed", collapsed);
    }
    if (collapseTabEl) {
      collapseTabEl.style.display = collapsed ? "flex" : "none";
    }
    updateCollapseTabLabel();
    safeStorageSet(PANE_COLLAPSED_KEY, collapsed ? "1" : "0");
  }

  // Pointer Events + setPointerCapture (rather than mouse events plus
  // document-level mousemove/mouseup listeners) so the splitter keeps
  // receiving move/up events for the drag it started even once the cursor
  // leaves the element — or the window entirely — instead of the drag
  // getting stuck "on" because a mouseup landed somewhere this page never
  // saw it.
  function onSplitterPointerDown(event) {
    event.preventDefault();
    paneState.dragging = true;
    if (splitterEl) {
      splitterEl.classList.add("dragging");
      if (typeof splitterEl.setPointerCapture === "function") {
        try {
          splitterEl.setPointerCapture(event.pointerId);
        } catch (err) {
          // Capture failed (or unsupported) — dragging still tracks via
          // whatever move/up events do reach the element normally.
        }
      }
    }
    if (layoutEl) {
      layoutEl.classList.add("no-select");
    }
  }

  function onSplitterPointerMove(event) {
    if (!paneState.dragging) {
      return;
    }
    // Belt-and-suspenders on top of pointer capture: `buttons` reflects
    // what's actually held *right now*, so a drag that never got its
    // pointerup delivered (a capture that silently lapsed, a dialog
    // stealing the event, etc.) still self-heals on the very next move
    // instead of leaving the pane permanently "dragging".
    if (event.buttons === 0) {
      endSplitterDrag(event);
      return;
    }
    // The review pane sits at the right edge of the layout, so its width
    // is just the distance from the cursor to the window's right edge.
    applyPaneWidth(clampPaneWidth(window.innerWidth - event.clientX));
  }

  function endSplitterDrag(event) {
    if (!paneState.dragging) {
      return;
    }
    paneState.dragging = false;
    if (splitterEl) {
      splitterEl.classList.remove("dragging");
      if (
        event &&
        typeof splitterEl.releasePointerCapture === "function" &&
        typeof splitterEl.hasPointerCapture === "function" &&
        splitterEl.hasPointerCapture(event.pointerId)
      ) {
        splitterEl.releasePointerCapture(event.pointerId);
      }
    }
    if (layoutEl) {
      layoutEl.classList.remove("no-select");
    }
    persistPaneWidth();
  }

  function onPaneWindowResize() {
    if (paneState.collapsed) {
      return;
    }
    var clamped = clampPaneWidth(paneState.width);
    if (clamped !== paneState.width) {
      applyPaneWidth(clamped);
      persistPaneWidth();
    }
  }

  function initPane() {
    layoutEl = document.querySelector(".layout");
    splitterEl = document.getElementById("splitter");

    collapseTabEl = button("review-collapse-tab", "", function () {
      applyPaneCollapsed(false);
    });
    collapseTabEl.style.display = "none";
    document.body.appendChild(collapseTabEl);

    applyPaneWidth(loadStoredPaneWidth());
    applyPaneCollapsed(loadStoredPaneCollapsed());

    if (splitterEl) {
      splitterEl.addEventListener("pointerdown", onSplitterPointerDown);
      splitterEl.addEventListener("pointermove", onSplitterPointerMove);
      splitterEl.addEventListener("pointerup", endSplitterDrag);
      splitterEl.addEventListener("pointercancel", endSplitterDrag);
    }
    window.addEventListener("resize", onPaneWindowResize);
  }

  // -- block selection (shared by click, keyboard nav, and reanchor) ----

  // Selects `hash`, always re-rendering. `opts.ensureVisible` scrolls the
  // block into view if needed (keyboard nav, and re-anchoring from the
  // unanchored list, where the block clicked to trigger the action isn't
  // necessarily the one now selected).
  // `opts.expandIfCollapsed` re-opens a collapsed review pane (click
  // selection only — see applyMarkers()).
  function selectBlock(hash, opts) {
    state.selectedHash = hash;
    state.editingId = null;
    render();
    if (opts && opts.ensureVisible) {
      scrollBlockIntoView(hash);
    }
    if (opts && opts.expandIfCollapsed && paneState.collapsed) {
      applyPaneCollapsed(false);
    }
  }

  function scrollBlockIntoView(hash) {
    var blockEl = findAnchorElement(hash);
    if (blockEl && typeof blockEl.scrollIntoView === "function") {
      blockEl.scrollIntoView({ block: "nearest" });
    }
  }

  // The elements considered "siblings" of `el` for Alt+↑/↓ navigation: the
  // other `.blk`/`.anchor` elements sharing `el`'s immediate DOM parent.
  // For a top-level `.blk` div that's every other block (parent is
  // `main.doc`); for a list item, the other `<li>` in the *same* `<ul>`/
  // `<ol>` (a nested item's own parent is the inner list, not the outer
  // item, so this naturally excludes items at a different nesting depth);
  // for a table row, the other `<tr>` in the same `<thead>`/`<tbody>`.
  function siblingAnchors(el) {
    if (!el || !el.parentElement) {
      return [];
    }
    var result = [];
    var children = el.parentElement.children;
    for (var i = 0; i < children.length; i++) {
      var child = children[i];
      if (child.classList && (child.classList.contains("anchor") || child.classList.contains("blk"))) {
        result.push(child);
      }
    }
    return result;
  }

  function selectAdjacentAnchor(direction) {
    var current = state.selectedHash ? findAnchorElement(state.selectedHash) : null;
    var siblings = current ? siblingAnchors(current) : blockElements();
    if (!siblings.length) {
      return;
    }
    var hashes = [];
    for (var i = 0; i < siblings.length; i++) {
      hashes.push(siblings[i].getAttribute("data-hash"));
    }
    var currentIdx = current ? hashes.indexOf(state.selectedHash) : -1;
    var nextIdx = currentIdx === -1 ? (direction > 0 ? 0 : hashes.length - 1) : currentIdx + direction;
    if (nextIdx < 0 || nextIdx >= hashes.length) {
      return;
    }
    selectBlock(hashes[nextIdx], { ensureVisible: true });
  }

  // Alt+← selects the currently-selected anchor's nearest enclosing
  // anchor (an item's parent item, or a top-level item/row's parent
  // block). Alt+→ selects its first nested anchor, if it has one (an
  // item's first sub-item; a block's first item/row).
  function selectParentAnchor() {
    if (!state.selectedHash) {
      return;
    }
    var current = findAnchorElement(state.selectedHash);
    var ancestor = current && current.parentElement ? current.parentElement.closest(".anchor, .blk") : null;
    if (!ancestor) {
      return;
    }
    selectBlock(ancestor.getAttribute("data-hash"), { ensureVisible: true, expandIfCollapsed: true });
  }

  function selectFirstChildAnchor() {
    if (!state.selectedHash) {
      return;
    }
    var current = findAnchorElement(state.selectedHash);
    var child = current ? current.querySelector(".anchor") : null;
    if (!child) {
      return;
    }
    selectBlock(child.getAttribute("data-hash"), { ensureVisible: true, expandIfCollapsed: true });
  }

  // -- global keyboard shortcuts (section 3 of the baseline UX design) --

  // True while a TEXTAREA or INPUT holds focus — see the Alt+←/→ handling
  // in onGlobalKeydown().
  function isTextInputFocused() {
    var active = document.activeElement;
    return !!active && (active.tagName === "TEXTAREA" || active.tagName === "INPUT");
  }

  function handleEscape() {
    var active = document.activeElement;
    if (active && active.tagName === "TEXTAREA") {
      active.blur();
      return;
    }
    if (state.selectedHash || state.editingId) {
      state.selectedHash = null;
      state.editingId = null;
      render();
    }
  }

  function onGlobalKeydown(event) {
    // `withModifier && !event.altKey` (rather than just `withModifier`)
    // for every shortcut below except Alt+↑/↓ itself: on layouts where
    // AltGr is used to type punctuation, AltGr commonly arrives as
    // Ctrl+Alt (or Meta+Alt) together, which would otherwise misfire the
    // collapse/reload shortcuts on an ordinary keypress that never meant
    // to invoke either.
    var withModifier = (event.metaKey || event.ctrlKey) && !event.altKey;

    // Backslash's own key/position varies enough by layout (and is a dead
    // key or absent outright on some) that both the produced character
    // and the physical key code are checked; ⌘J is also accepted as a
    // JIS-keyboard-friendly alternative (documented alongside `⌘\` in
    // docs/qa/baseline-checklist.md).
    if (
      withModifier &&
      (event.key === "\\" ||
        event.code === "Backslash" ||
        event.key === "j" ||
        event.key === "J")
    ) {
      event.preventDefault();
      applyPaneCollapsed(!paneState.collapsed);
      return;
    }
    if (withModifier && (event.key === "r" || event.key === "R")) {
      event.preventDefault();
      location.reload();
      return;
    }
    if (event.altKey && (event.key === "ArrowDown" || event.key === "ArrowUp")) {
      event.preventDefault();
      selectAdjacentAnchor(event.key === "ArrowDown" ? 1 : -1);
      return;
    }
    // Alt+←/→ are left to pass through untouched while a TEXTAREA/INPUT is
    // focused (no preventDefault, no anchor navigation) — unlike Alt+↑/↓
    // above, which always navigate regardless of focus. A focused text
    // field is where a user is actively typing a comment, and Alt+←/→ has
    // the potential to be a text-editing shortcut there depending on
    // platform/layout; stealing it away would fight that instead.
    if (event.altKey && event.key === "ArrowLeft") {
      if (isTextInputFocused()) {
        return;
      }
      event.preventDefault();
      selectParentAnchor();
      return;
    }
    if (event.altKey && event.key === "ArrowRight") {
      if (isTextInputFocused()) {
        return;
      }
      event.preventDefault();
      selectFirstChildAnchor();
      return;
    }
    if (event.key === "Escape") {
      handleEscape();
    }
  }

  function docRoot() {
    return document.querySelector("main.doc") || document.querySelector("main");
  }

  // Top-level blocks only (`.blk`) — used where "nothing nested" is the
  // right default: the Alt+↑/↓ fallback sibling set when nothing is
  // selected, and the inner-count badge's own exclusion of rows (which
  // never have anchor descendants).
  function blockElements() {
    var root = docRoot();
    return root ? root.querySelectorAll(".blk") : [];
  }

  // Every anchor in the document, at any granularity: top-level blocks
  // *and* nested list items/table rows. Used everywhere selection,
  // marking, and unanchored-detection need to consider every anchor kind,
  // not just blocks.
  function anchorElements() {
    var root = docRoot();
    return root ? root.querySelectorAll(".blk, .anchor") : [];
  }

  function findAnchorElement(hash) {
    var elements = anchorElements();
    for (var i = 0; i < elements.length; i++) {
      if (elements[i].getAttribute("data-hash") === hash) {
        return elements[i];
      }
    }
    return null;
  }

  // The excerpt for an anchor that isn't in state.doc yet (e.g.
  // re-anchoring onto a block/item/row that has never been commented on,
  // or writing a first comment on one). Prefers the server-computed
  // `data-excerpt` attribute (render.rs stamps every `.blk`/`.anchor` with
  // one, built from the anchor's own sanitized content) and only falls
  // back to a rough client-side derivation if that's missing for some
  // reason. Not required to match render.rs's excerpt byte-for-byte in the
  // fallback case — excerpt and index are purely auxiliary display aids,
  // not identity.
  function excerptForBlockElement(blockEl) {
    var fromServer = blockEl.dataset
      ? blockEl.dataset.excerpt
      : blockEl.getAttribute("data-excerpt");
    if (fromServer) {
      return fromServer;
    }
    return excerptFromElement(blockEl);
  }

  function excerptFromElement(blockEl) {
    var text = (blockEl.textContent || "").trim();
    var firstLine = text.split("\n")[0] || "";
    return firstLine.length > 80 ? firstLine.slice(0, 80) : firstLine;
  }

  // "L12-L18" (multi-line span) or "L40" (single-line span), sourced from
  // the `data-line-start`/`data-line-end` render.rs stamps on every
  // `.blk`/`.anchor` element. Returns null (badge omitted) if either
  // attribute is missing — e.g. `blockEl` is null because the selected
  // anchor is no longer present in the live DOM.
  function lineRangeLabel(blockEl) {
    if (!blockEl) {
      return null;
    }
    var start = blockEl.dataset
      ? blockEl.dataset.lineStart
      : blockEl.getAttribute("data-line-start");
    var end = blockEl.dataset
      ? blockEl.dataset.lineEnd
      : blockEl.getAttribute("data-line-end");
    if (!start || !end) {
      return null;
    }
    return start === end ? "L" + start : "L" + start + "-L" + end;
  }

  // -- state helpers --------------------------------------------------

  function findBlock(hash) {
    for (var i = 0; i < state.doc.blocks.length; i++) {
      if (state.doc.blocks[i].hash === hash) {
        return state.doc.blocks[i];
      }
    }
    return null;
  }

  function ensureBlock(hash, excerpt, kind) {
    var found = findBlock(hash);
    if (found) {
      return found;
    }
    var created = { hash: hash, excerpt: excerpt || "", kind: kind || "block", comments: [] };
    state.doc.blocks.push(created);
    return created;
  }

  function dropIfEmpty(hash) {
    var block = findBlock(hash);
    if (block && block.comments.length === 0) {
      state.doc.blocks = state.doc.blocks.filter(function (b) {
        return b.hash !== hash;
      });
    }
  }

  // hash -> comment count, rebuilt once per render() (see render()) rather
  // than walking state.doc.blocks on every commentCount() call — applyMarkers()
  // and countCommentedDescendants() together call commentCount() once per
  // anchor element (and, for descendants, again per anchor below it), so a
  // linear findBlock() scan per call made marker application quadratic in
  // the number of commented blocks on a large document.
  var commentCountMap = null;

  function buildCommentCountMap() {
    var map = new Map();
    for (var i = 0; i < state.doc.blocks.length; i++) {
      var block = state.doc.blocks[i];
      map.set(block.hash, block.comments.length);
    }
    return map;
  }

  function commentCount(hash) {
    if (!commentCountMap) {
      // Defensive fallback for any call site outside the normal render()
      // flow (none exist today, but this must never silently read stale
      // counts if one's added later).
      commentCountMap = buildCommentCountMap();
    }
    return commentCountMap.get(hash) || 0;
  }

  function totalCommentCount() {
    var total = state.doc.file_comments.length;
    for (var i = 0; i < state.doc.blocks.length; i++) {
      total += state.doc.blocks[i].comments.length;
    }
    return total;
  }

  function nowIso() {
    return new Date().toISOString();
  }

  // Comments only ever leave the browser inside a full `PUT /review` body
  // — there's no server-side "create a comment" endpoint to hand back a
  // generated id — so ids are generated here. The server only checks the
  // shape (`^c_[0-9a-f]{16}$`, see review::validate), so this only needs
  // to be unique within this document's sidecar, not globally.
  function newLocalCommentId() {
    var bytes = new Uint8Array(8);
    if (window.crypto && typeof window.crypto.getRandomValues === "function") {
      window.crypto.getRandomValues(bytes);
    } else {
      for (var i = 0; i < bytes.length; i++) {
        bytes[i] = Math.floor(Math.random() * 256);
      }
    }
    var hex = "";
    for (var j = 0; j < bytes.length; j++) {
      hex += (bytes[j] < 16 ? "0" : "") + bytes[j].toString(16);
    }
    return "c_" + hex;
  }

  // Recomputes which of the currently-known review blocks have no matching
  // `.blk`/`.anchor[data-hash]` in the live document — every anchor kind,
  // not just top-level blocks, or a comment on a list item/table row would
  // always show up as unanchored (its hash never matches any `.blk`). Run
  // after every DOM swap (initial load, and every live-reload body
  // replacement) rather than trusting the server's `unanchored` field past
  // the very first load, since the file can change underneath us at any
  // time.
  function recomputeUnanchored() {
    var elements = anchorElements();
    var present = {};
    for (var i = 0; i < elements.length; i++) {
      present[elements[i].getAttribute("data-hash")] = true;
    }
    var result = [];
    for (var j = 0; j < state.doc.blocks.length; j++) {
      var block = state.doc.blocks[j];
      if (!present[block.hash] && block.comments.length > 0) {
        result.push(block.hash);
      }
    }
    state.unanchored = result;
  }

  // -- network ----------------------------------------------------------

  function loadReview() {
    state.loading = true;
    state.noFileOpen = false;
    render();
    fetch(REVIEW_URL, { method: "GET", cache: "no-store", headers: REQUEST_HEADERS })
      .then(function (response) {
        return response.json().then(function (payload) {
          return { status: response.status, ok: response.ok, payload: payload };
        });
      })
      .then(function (result) {
        state.loading = false;
        if (result.status === 409) {
          // No file open yet — not a failure, just an empty state: no
          // banner, no form, just the placeholder renderAside() shows for
          // state.noFileOpen.
          state.loaded = false;
          state.noFileOpen = true;
          state.error = null;
          render();
          return;
        }
        if (!result.ok) {
          state.loaded = false;
          state.error =
            "Couldn't load the review, so saving is disabled: " +
            errorMessage(result.payload, "unknown error");
          render();
          return;
        }
        state.doc = {
          version: result.payload.version,
          file: result.payload.file,
          blocks: result.payload.blocks || [],
          file_comments: result.payload.file_comments || [],
        };
        state.loaded = true;
        state.error = null;
        recomputeUnanchored();
        render();
      })
      .catch(function () {
        state.loading = false;
        state.loaded = false;
        state.error =
          "Couldn't load the review, so saving is disabled: network error";
        render();
      });
  }

  // Sets the header's save-status indicator (see renderAside()) and
  // re-renders. "saved" auto-clears itself after SAVE_STATUS_CLEAR_MS;
  // "saving"/"failed" persist until the next setSaveStatus() call (a new
  // save, or a retry click on the "Save failed — retry" indicator itself).
  function setSaveStatus(status) {
    state.saveStatus = status;
    if (saveStatusTimer) {
      clearTimeout(saveStatusTimer);
      saveStatusTimer = null;
    }
    if (status === "saved") {
      saveStatusTimer = setTimeout(function () {
        state.saveStatus = null;
        saveStatusTimer = null;
        render();
      }, SAVE_STATUS_CLEAR_MS);
    }
    render();
  }

  function saveReview() {
    // Never PUT before a successful GET /review: without one, state.doc
    // may still be the placeholder (empty `file`, no blocks), and saving
    // that would either fail the server's file-name check or, worse,
    // silently overwrite real data with an incomplete document.
    if (!state.loaded) {
      return;
    }
    var payload = {
      version: state.doc.version,
      file: state.doc.file,
      blocks: state.doc.blocks,
      file_comments: state.doc.file_comments,
    };
    setSaveStatus("saving");
    fetch(REVIEW_URL, {
      method: "PUT",
      cache: "no-store",
      headers: Object.assign(
        { "Content-Type": "application/json" },
        REQUEST_HEADERS
      ),
      body: JSON.stringify(payload),
    })
      .then(function (response) {
        if (!response.ok) {
          throw new Error("save failed: " + response.status);
        }
        setSaveStatus("saved");
      })
      .catch(function () {
        setSaveStatus("failed");
      });
  }

  function errorMessage(payload, fallback) {
    return (payload && typeof payload.error === "string" && payload.error) || fallback;
  }

  // Export success leaves a toast with a "Copy to clipboard" button rather
  // than copying immediately: `document.execCommand("copy")` requires a
  // user gesture to work reliably across browsers/WebViews, and the fetch
  // callback here runs outside of one (it's asynchronous, so by the time
  // it resolves the click that started the export is no longer considered
  // "active" by the browser). Waiting for an explicit, synchronous click
  // on the toast's own button keeps the copy inside a real gesture.
  function handleExport(exportBtn) {
    exportBtn.disabled = true;
    fetch(EXPORT_URL, { method: "POST", cache: "no-store", headers: REQUEST_HEADERS })
      .then(function (response) {
        return response.json().then(function (payload) {
          return { ok: response.ok, payload: payload };
        });
      })
      .then(function (result) {
        exportBtn.disabled = false;
        if (!result.ok) {
          state.error = errorMessage(result.payload, "export failed");
          render();
          return;
        }
        state.toast = {
          message: "Exported " + result.payload.path,
          markdown: result.payload.markdown || "",
          copied: false,
        };
        render();
      })
      .catch(function () {
        exportBtn.disabled = false;
        state.error = "Export failed";
        render();
      });
  }

  function copyToClipboard(text) {
    try {
      var textarea = document.createElement("textarea");
      textarea.value = text;
      textarea.style.position = "fixed";
      textarea.style.top = "0";
      textarea.style.left = "0";
      textarea.style.opacity = "0";
      document.body.appendChild(textarea);
      textarea.focus();
      textarea.select();
      var ok = document.execCommand("copy");
      document.body.removeChild(textarea);
      return ok;
    } catch (err) {
      return false;
    }
  }

  function buildToast() {
    var toast = el("div", "review-toast");
    toast.appendChild(document.createTextNode(state.toast.message));

    var copyBtn = button(
      "review-copy",
      state.toast.copied ? "Copied" : "Copy to clipboard",
      function () {
        // Runs synchronously inside this click handler — the requirement
        // for execCommand("copy") to work — using the markdown text the
        // export response already handed back, not a fresh fetch.
        var ok = copyToClipboard(state.toast.markdown);
        state.toast.copied = ok;
        if (!ok) {
          state.toast.message =
            "Exported (copy failed — file was still saved)";
        }
        render();
      }
    );
    toast.appendChild(copyBtn);

    toast.appendChild(
      button("review-toast-close", "Close", function () {
        state.toast = null;
        render();
      })
    );
    return toast;
  }

  // -- mutations (always followed by a save + re-render) -----------------

  function addComment(hash, excerpt, text, kind) {
    var block = ensureBlock(hash, excerpt, kind);
    var now = nowIso();
    block.comments.push({
      id: newLocalCommentId(),
      text: text,
      created: now,
      updated: now,
    });
    saveReview();
    render();
  }

  function editComment(hash, id, text) {
    var block = findBlock(hash);
    if (!block) {
      return;
    }
    for (var i = 0; i < block.comments.length; i++) {
      if (block.comments[i].id === id) {
        block.comments[i].text = text;
        block.comments[i].updated = nowIso();
        break;
      }
    }
    saveReview();
    render();
  }

  function deleteComment(hash, id) {
    var block = findBlock(hash);
    if (!block) {
      return;
    }
    block.comments = block.comments.filter(function (c) {
      return c.id !== id;
    });
    dropIfEmpty(hash);
    recomputeUnanchored();
    saveReview();
    render();
  }

  function reanchor(oldHash, newHash, newExcerpt, newKind) {
    var block = findBlock(oldHash);
    if (!block || oldHash === newHash) {
      return;
    }
    // Merge into an existing block at the target hash, if there is one,
    // rather than creating a second entry for the same hash.
    var target = findBlock(newHash);
    if (target) {
      target.comments = target.comments.concat(block.comments);
      state.doc.blocks = state.doc.blocks.filter(function (b) {
        return b.hash !== oldHash;
      });
    } else {
      block.hash = newHash;
      block.excerpt = newExcerpt;
      block.kind = newKind || "block";
    }
    recomputeUnanchored();
    saveReview();
    render();
  }

  function deleteUnanchored(hash) {
    state.doc.blocks = state.doc.blocks.filter(function (b) {
      return b.hash !== hash;
    });
    recomputeUnanchored();
    saveReview();
    render();
  }

  // -- file-wide comments (not anchored to any block/item/row — "file
  //    mode", entered whenever nothing is selected; see buildFileCommentsView()
  //    and buildFileBreadcrumbSegment()) ----------------------------------

  function addFileComment(text) {
    var now = nowIso();
    state.doc.file_comments.push({
      id: newLocalCommentId(),
      text: text,
      created: now,
      updated: now,
    });
    saveReview();
    render();
  }

  function editFileComment(id, text) {
    for (var i = 0; i < state.doc.file_comments.length; i++) {
      if (state.doc.file_comments[i].id === id) {
        state.doc.file_comments[i].text = text;
        state.doc.file_comments[i].updated = nowIso();
        break;
      }
    }
    saveReview();
    render();
  }

  function deleteFileComment(id) {
    state.doc.file_comments = state.doc.file_comments.filter(function (c) {
      return c.id !== id;
    });
    saveReview();
    render();
  }

  // -- rendering ----------------------------------------------------------

  // Sum of comment counts on every anchor nested (at any depth) inside
  // `el` — an item's own sub-items, or a block's items/rows. Rows never
  // have anchor descendants (a table cell can't contain a nested anchor),
  // so this is always 0 for a `tr.anchor`. Descendant hashes are
  // deduplicated (two identical-text items/rows share a hash, and so share
  // comments — counting the same hash twice would overstate the badge), and
  // a descendant sharing `el`'s own hash is excluded (its comments are
  // already `el`'s own comment count, not an "inner" count).
  function countCommentedDescendants(el) {
    var descendants = el.querySelectorAll(".anchor");
    var selfHash = el.getAttribute("data-hash");
    var seen = new Set();
    var total = 0;
    for (var i = 0; i < descendants.length; i++) {
      var hash = descendants[i].getAttribute("data-hash");
      if (hash === selfHash || seen.has(hash)) {
        continue;
      }
      seen.add(hash);
      total += commentCount(hash);
    }
    return total;
  }

  // Marks every `.blk`/`.anchor` element with its current has-comments/
  // selected/inner-count state. Click *selection* itself is handled by a
  // single delegated `document` listener (see `onDocumentClick`) rather
  // than a per-element `onclick` here, so a click inside a nested anchor
  // (e.g. an `<li>` inside its enclosing `.blk`) resolves to the innermost
  // match via `closest(".anchor, .blk")` instead of always hitting the
  // outer block.
  function applyMarkers() {
    var elements = anchorElements();
    for (var i = 0; i < elements.length; i++) {
      var anchorEl = elements[i];
      var hash = anchorEl.getAttribute("data-hash");
      var count = commentCount(hash);
      if (count > 0) {
        anchorEl.classList.add("has-comments");
        anchorEl.setAttribute("data-count", String(count));
      } else {
        anchorEl.classList.remove("has-comments");
        anchorEl.removeAttribute("data-count");
      }
      // `::after`/`::before` generated content isn't rendered on `<tr>`
      // in any major browser (a table-row-specific quirk), so a row's
      // own count badge is instead rendered from its *last cell's*
      // `data-count` — see style.css's `tr.anchor.has-comments > :last-child`.
      if (anchorEl.tagName === "TR") {
        var lastCell = anchorEl.lastElementChild;
        if (lastCell) {
          if (count > 0) {
            lastCell.setAttribute("data-count", String(count));
          } else {
            lastCell.removeAttribute("data-count");
          }
        }
      }
      anchorEl.classList.toggle("selected", hash === state.selectedHash);

      var innerCount = countCommentedDescendants(anchorEl);
      if (innerCount > 0) {
        anchorEl.setAttribute("data-inner-count", String(innerCount));
      } else {
        anchorEl.removeAttribute("data-inner-count");
      }
    }
  }

  // The single delegated click handler for the whole document pane
  // (registered once in init(), not per-element): `closest(".anchor,
  // .blk")` from the actual click target always resolves to the innermost
  // anchor under the cursor (an `<li>`/`<tr>` before its enclosing
  // `.blk`), so nested items/rows are directly clickable without any
  // `stopPropagation` bookkeeping.
  function onDocumentClick(event) {
    var root = docRoot();
    if (!root) {
      return;
    }
    var target = event.target.closest(".anchor, .blk");
    if (!target || !root.contains(target)) {
      return;
    }
    selectBlock(target.getAttribute("data-hash"), { expandIfCollapsed: true });
  }

  function render() {
    commentCountMap = buildCommentCountMap();
    applyMarkers();
    renderAside();
    updateCollapseTabLabel();
  }

  function renderAside() {
    if (!asideEl) {
      return;
    }
    clearChildren(asideEl);

    if (state.noFileOpen) {
      // No file open: only this placeholder, no header, no form.
      // loadReview() re-fetches once a file is open (see
      // onBodyReplaced()), which replaces this with the real pane.
      asideEl.appendChild(
        el(
          "p",
          "review-placeholder",
          "Open a file to see its review here"
        )
      );
      return;
    }

    var header = el("div", "review-header");
    header.appendChild(
      el("span", "review-count", totalCommentCount() + " comments")
    );
    if (state.saveStatus === "saving") {
      header.appendChild(el("span", "review-save-status saving", "Saving…"));
    } else if (state.saveStatus === "saved") {
      header.appendChild(el("span", "review-save-status saved", "Saved"));
    } else if (state.saveStatus === "failed") {
      header.appendChild(
        button("review-save-status failed", "Save failed — retry", function () {
          saveReview();
        })
      );
    }
    header.appendChild(
      button("review-collapse-btn", "⟩", function () {
        applyPaneCollapsed(true);
      })
    );
    header.appendChild(
      button("review-export", "Export", function (event) {
        handleExport(event.currentTarget);
      })
    );
    asideEl.appendChild(header);

    if (state.error) {
      asideEl.appendChild(el("div", "review-banner", state.error));
    }
    if (state.toast) {
      asideEl.appendChild(buildToast());
    }

    var body = el("div", "review-body");
    if (state.loading) {
      body.appendChild(el("p", "review-placeholder", "Loading…"));
    } else if (!state.loaded) {
      // Load failed: state.error already carries the reason as a banner.
      // Don't build the comment form / edit / delete / reanchor UI at all
      // while it's unknown whether state.doc reflects what's actually on
      // disk — see saveReview()'s early return for the same guard.
      body.appendChild(
        el("p", "review-placeholder", "Review is unavailable")
      );
      body.appendChild(
        button("review-retry", "Retry", function () {
          loadReview();
        })
      );
    } else if (!state.selectedHash) {
      // Nothing selected — "file mode" (also entered by clicking the
      // breadcrumb's root "File" segment, or pressing Esc): comments on
      // the document as a whole rather than any particular block/item/row.
      body.appendChild(buildFileCommentsView());
    } else {
      body.appendChild(buildSelectedBlockView());
    }
    asideEl.appendChild(body);

    if (state.loaded) {
      asideEl.appendChild(buildUnanchoredSection());
    }
  }

  // The anchor kind an element identifies itself as (`data-kind`,
  // `"block"`/`"item"`/`"row"`) — falls back to `"block"` if the attribute
  // is somehow absent, matching `ensureBlock`'s own default.
  function anchorKindOf(anchorEl) {
    var kind = anchorEl.dataset ? anchorEl.dataset.kind : anchorEl.getAttribute("data-kind");
    return kind || "block";
  }

  function anchorKindLabel(anchorEl) {
    var kind = anchorKindOf(anchorEl);
    if (kind === "item") {
      return "Item";
    }
    if (kind === "row") {
      return "Row";
    }
    return "Block";
  }

  // `el`'s own chain of enclosing anchors, top (a top-level block) to
  // bottom (`el` itself) — derived purely from the live DOM via
  // `closest(".anchor, .blk")`, since that's the only place the
  // block/item/row nesting relationship is actually recorded client-side
  // (GET /review's sidecar payload is a flat list of commented anchors,
  // not a tree).
  function ancestorChain(anchorEl) {
    var chain = [anchorEl];
    var current = anchorEl;
    while (current && current.parentElement) {
      var next = current.parentElement.closest(".anchor, .blk");
      if (!next) {
        break;
      }
      chain.unshift(next);
      current = next;
    }
    return chain;
  }

  // "File › Block L10-L20 › Item L13": the root "File" segment
  // (see buildFileBreadcrumbSegment()) is always present — clicking it
  // enters file mode (no selected anchor) — followed by one clickable
  // segment per level of `chain` (the currently-selected level rendered as
  // plain, non-clickable text instead of a button). `chain` is empty in
  // file mode, so the breadcrumb is then just the root segment alone.
  function buildBreadcrumb(chain) {
    var nav = el("div", "review-breadcrumb");
    nav.appendChild(buildFileBreadcrumbSegment());
    for (var i = 0; i < chain.length; i++) {
      nav.appendChild(el("span", "review-breadcrumb-sep", "›"));
      var node = chain[i];
      var nodeHash = node.getAttribute("data-hash");
      var label = anchorKindLabel(node) + " " + (lineRangeLabel(node) || "");
      if (nodeHash === state.selectedHash) {
        nav.appendChild(el("span", "review-breadcrumb-current", label));
      } else {
        nav.appendChild(
          button(
            "review-breadcrumb-link",
            label,
            (function (targetHash) {
              return function () {
                selectBlock(targetHash, { ensureVisible: true });
              };
            })(nodeHash)
          )
        );
      }
    }
    return nav;
  }

  // The breadcrumb's permanent root segment: "File", with a count badge
  // when there's at least one file-wide comment. Plain (non-clickable) text
  // while already in file mode (nothing selected); a clickable link back to
  // file mode otherwise.
  function buildFileBreadcrumbSegment() {
    var count = state.doc.file_comments.length;
    var node = state.selectedHash
      ? button("review-breadcrumb-link", "File", function () {
          selectBlock(null);
        })
      : el("span", "review-breadcrumb-current", "File");
    if (count > 0) {
      node.appendChild(el("span", "review-breadcrumb-badge", String(count)));
    }
    return node;
  }

  // While an item/row is selected: "↑ Comment on whole list"/"↑ Comment on
  // whole table", jumping to the enclosing block. While a block containing at
  // least one item/row is selected: a plain (non-actionable) hint that
  // there's finer-grained anchors to click into.
  function buildHint(anchorEl) {
    var kind = anchorKindOf(anchorEl);
    if (kind === "item" || kind === "row") {
      var blockEl = anchorEl.closest(".blk");
      if (!blockEl) {
        return null;
      }
      var whole = kind === "item" ? "list" : "table";
      return button("review-hint-up", "↑ Comment on whole " + whole, function () {
        selectBlock(blockEl.getAttribute("data-hash"), { ensureVisible: true });
      });
    }
    if (anchorEl.querySelector(".anchor")) {
      return el("p", "review-hint", "Click to select an item");
    }
    return null;
  }

  function buildSelectedBlockView() {
    var wrap = el("div", "review-selected");
    var hash = state.selectedHash;
    var block = findBlock(hash);
    var anchorEl = findAnchorElement(hash);
    var excerpt = block ? block.excerpt : anchorEl ? excerptForBlockElement(anchorEl) : "";
    var kind = anchorEl ? anchorKindOf(anchorEl) : block && block.kind ? block.kind : "block";

    // The breadcrumb (root "File" segment plus the selected anchor's own
    // chain) is shown even if `anchorEl` no longer exists live — e.g. the
    // selected anchor was removed by a live-reload — so there's always a
    // way back to file mode. `ancestorChain` itself needs a live element,
    // so it's only called when one exists; the chain is just empty (root
    // segment only) otherwise.
    wrap.appendChild(buildBreadcrumb(anchorEl ? ancestorChain(anchorEl) : []));
    if (anchorEl) {
      var hint = buildHint(anchorEl);
      if (hint) {
        wrap.appendChild(hint);
      }
    }

    var quoteWrap = el("div", "review-quote-wrap");
    var lineLabel = lineRangeLabel(anchorEl);
    if (lineLabel) {
      quoteWrap.appendChild(el("span", "review-line-badge", lineLabel));
    }
    quoteWrap.appendChild(el("blockquote", "review-quote", excerpt));
    wrap.appendChild(quoteWrap);

    var list = el("div", "review-comments");
    var comments = block ? block.comments : [];
    for (var i = 0; i < comments.length; i++) {
      list.appendChild(buildCommentItem(hash, comments[i]));
    }
    wrap.appendChild(list);

    wrap.appendChild(buildCommentForm(hash, excerpt, kind));
    return wrap;
  }

  // Shared by buildCommentItem() (block/item/row comments) and
  // buildFileCommentItem() (file-wide comments): the two differ only in
  // where a saved edit/delete actually lands, which the caller supplies as
  // `onSave(id, text)`/`onDelete(id)`.
  function buildCommentItemView(comment, onSave, onDelete) {
    var item = el("div", "review-comment");

    if (state.editingId === comment.id) {
      var textarea = el("textarea", "review-textarea");
      textarea.value = comment.text;
      item.appendChild(textarea);

      var actions = el("div", "review-comment-actions");
      var save = function () {
        var value = textarea.value.trim();
        if (value) {
          state.editingId = null;
          onSave(comment.id, value);
        }
      };
      actions.appendChild(button("review-save", "Save", save));
      actions.appendChild(
        button("review-cancel", "Cancel", function () {
          state.editingId = null;
          render();
        })
      );
      item.appendChild(actions);

      textarea.addEventListener("keydown", function (event) {
        if ((event.metaKey || event.ctrlKey) && event.key === "Enter") {
          save();
        }
      });
      return item;
    }

    item.appendChild(el("p", "review-comment-text", comment.text));
    var actions2 = el("div", "review-comment-actions");
    actions2.appendChild(
      button("review-comment-edit", "Edit", function () {
        state.editingId = comment.id;
        render();
      })
    );
    actions2.appendChild(
      button("review-comment-delete", "Delete", function () {
        onDelete(comment.id);
      })
    );
    item.appendChild(actions2);
    return item;
  }

  function buildCommentItem(hash, comment) {
    return buildCommentItemView(
      comment,
      function (id, text) {
        editComment(hash, id, text);
      },
      function (id) {
        deleteComment(hash, id);
      }
    );
  }

  function buildFileCommentItem(comment) {
    return buildCommentItemView(comment, editFileComment, deleteFileComment);
  }

  // Shared by buildCommentForm() (block/item/row comments) and
  // buildFileCommentForm() (file-wide comments): the two differ only in
  // where a new comment actually gets added, which the caller supplies as
  // `onSubmit(text)`.
  function buildCommentFormView(onSubmit) {
    var form = el("div", "review-form");
    var textarea = el("textarea", "review-textarea");
    textarea.placeholder = "Enter a comment… (Cmd/Ctrl+Enter to save)";
    form.appendChild(textarea);

    var submit = function () {
      var value = textarea.value.trim();
      if (!value) {
        return;
      }
      textarea.value = "";
      onSubmit(value);
    };
    form.appendChild(button("review-save", "Save", submit));

    textarea.addEventListener("keydown", function (event) {
      if ((event.metaKey || event.ctrlKey) && event.key === "Enter") {
        submit();
      }
    });
    return form;
  }

  function buildCommentForm(hash, excerpt, kind) {
    return buildCommentFormView(function (text) {
      addComment(hash, excerpt, text, kind);
    });
  }

  function buildFileCommentForm() {
    return buildCommentFormView(function (text) {
      addFileComment(text);
    });
  }

  // "File mode": entered whenever nothing is selected (initial state, Esc,
  // or clicking the breadcrumb's root "File" segment). Comments on the
  // document as a whole rather than any particular block/item/row — no
  // excerpt/quote to show, since there's no anchor. The "Click a block"
  // hint that used to be the entire placeholder here now sits below the
  // input as a small reminder that per-block comments are also available.
  function buildFileCommentsView() {
    var wrap = el("div", "review-selected");
    wrap.appendChild(buildBreadcrumb([]));
    wrap.appendChild(
      el("h3", "review-file-heading", "Comments on the whole file")
    );

    var list = el("div", "review-comments");
    var comments = state.doc.file_comments;
    for (var i = 0; i < comments.length; i++) {
      list.appendChild(buildFileCommentItem(comments[i]));
    }
    wrap.appendChild(list);

    wrap.appendChild(buildFileCommentForm());
    wrap.appendChild(el("p", "review-hint review-file-hint", "Click a block"));
    return wrap;
  }

  function buildUnanchoredSection() {
    var section = el("div", "review-unanchored");
    if (state.unanchored.length === 0) {
      return section;
    }

    section.appendChild(
      button(
        "review-unanchored-toggle",
        "Unanchored (" + state.unanchored.length + ")",
        function () {
          state.unanchoredOpen = !state.unanchoredOpen;
          render();
        }
      )
    );

    if (state.unanchoredOpen) {
      var list = el("div", "review-unanchored-list");
      for (var i = 0; i < state.unanchored.length; i++) {
        var block = findBlock(state.unanchored[i]);
        if (block) {
          list.appendChild(buildUnanchoredItem(block));
        }
      }
      section.appendChild(list);
    }
    return section;
  }

  function buildUnanchoredItem(block) {
    var item = el("div", "review-unanchored-item");
    item.appendChild(el("p", "review-unanchored-excerpt", block.excerpt));
    for (var i = 0; i < block.comments.length; i++) {
      item.appendChild(
        el("p", "review-unanchored-comment", block.comments[i].text)
      );
    }

    var actions = el("div", "review-unanchored-actions");
    var reanchorBtn = button(
      "review-reanchor",
      "Reanchor to selected block",
      function () {
        if (!state.selectedHash) {
          return;
        }
        var targetHash = state.selectedHash;
        var targetEl = findAnchorElement(targetHash);
        var excerpt = targetEl
          ? excerptForBlockElement(targetEl)
          : block.excerpt;
        var kind = targetEl ? anchorKindOf(targetEl) : "block";
        reanchor(block.hash, targetHash, excerpt, kind);
        scrollBlockIntoView(targetHash);
      }
    );
    reanchorBtn.disabled = !state.selectedHash;
    actions.appendChild(reanchorBtn);
    actions.appendChild(
      button("review-unanchored-delete", "Delete", function () {
        deleteUnanchored(block.hash);
      })
    );
    item.appendChild(actions);
    return item;
  }

  // -- entry points ---------------------------------------------------

  function onBodyReplaced() {
    if (state.noFileOpen) {
      // A file may have just been opened — re-fetch to find out, rather
      // than sitting on the stale "no file open" placeholder.
      loadReview();
      return;
    }
    // The DOM under <main> was just swapped out from under us (live
    // reload) — re-derive what's anchored/unanchored against the new
    // content and reapply markers/handlers/selection.
    recomputeUnanchored();
    render();
  }

  window.__mdviewReview = { onBodyReplaced: onBodyReplaced };

  function init() {
    asideEl = document.getElementById("review");
    if (!asideEl) {
      return;
    }
    initPane();
    document.addEventListener("keydown", onGlobalKeydown);
    document.addEventListener("click", onDocumentClick);
    render();
    loadReview();
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }
})();

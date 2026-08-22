(function () {
  "use strict";

  // Block-level review comments: a right-hand pane that lets you attach
  // comments to top-level Markdown blocks (identified by the `data-hash`
  // the server stamps on each `.blk` div — see render.rs), persist them via
  // GET/PUT /review, and export them as a Markdown review summary via
  // POST /export. See docs/superpowers/specs/2026-08-22-inline-review-comments-design.md.

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
    doc: { version: 1, file: "", blocks: [] },
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

  // -- localStorage, wrapped in try/catch per the design doc: a disabled or
  //    full localStorage must never throw its way into a broken pane —
  //    just fall back to the defaults. --------------------------------

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
  // unanchored list — see the design doc's section 3 — where the block
  // clicked to trigger the action isn't necessarily the one now selected).
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
    var blockEl = findBlockElement(hash);
    if (blockEl && typeof blockEl.scrollIntoView === "function") {
      blockEl.scrollIntoView({ block: "nearest" });
    }
  }

  function selectAdjacentBlock(direction) {
    var elements = blockElements();
    if (!elements.length) {
      return;
    }
    var hashes = [];
    for (var i = 0; i < elements.length; i++) {
      hashes.push(elements[i].getAttribute("data-hash"));
    }
    var currentIdx = -1;
    for (var j = 0; j < hashes.length; j++) {
      if (hashes[j] === state.selectedHash) {
        currentIdx = j;
        break;
      }
    }
    var nextIdx = currentIdx === -1 ? (direction > 0 ? 0 : hashes.length - 1) : currentIdx + direction;
    if (nextIdx < 0 || nextIdx >= hashes.length) {
      return;
    }
    selectBlock(hashes[nextIdx], { ensureVisible: true });
  }

  // -- global keyboard shortcuts (section 3 of the baseline UX design) --

  function handleEscape() {
    var active = document.activeElement;
    if (active && active.tagName === "TEXTAREA") {
      active.blur();
      return;
    }
    if (state.selectedHash) {
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
      selectAdjacentBlock(event.key === "ArrowDown" ? 1 : -1);
      return;
    }
    if (event.key === "Escape") {
      handleEscape();
    }
  }

  function docRoot() {
    return document.querySelector("main.doc") || document.querySelector("main");
  }

  function blockElements() {
    var root = docRoot();
    return root ? root.querySelectorAll(".blk") : [];
  }

  function findBlockElement(hash) {
    var elements = blockElements();
    for (var i = 0; i < elements.length; i++) {
      if (elements[i].getAttribute("data-hash") === hash) {
        return elements[i];
      }
    }
    return null;
  }

  // The excerpt for a block that isn't in state.doc yet (e.g. re-anchoring
  // onto a block that has never been commented on, or writing a first
  // comment on one). Prefers the server-computed `data-excerpt` attribute
  // (render.rs stamps every `.blk` with one, built from the block's own
  // sanitized content) and only falls back to a rough client-side
  // derivation if that's missing for some reason. Not required to match
  // render.rs's excerpt byte-for-byte in the fallback case — it's purely a
  // display aid (see the design doc: "excerpt と index は補助情報").
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

  // "L12-L18" (multi-line block) or "L40" (single-line block), sourced from
  // the `data-line-start`/`data-line-end` render.rs stamps on every `.blk`
  // div. Returns null (badge omitted) if either attribute is missing —
  // e.g. `blockEl` is null because the selected block is no longer present
  // in the live DOM.
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

  function ensureBlock(hash, excerpt) {
    var found = findBlock(hash);
    if (found) {
      return found;
    }
    var created = { hash: hash, excerpt: excerpt || "", comments: [] };
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

  function commentCount(hash) {
    var block = findBlock(hash);
    return block ? block.comments.length : 0;
  }

  function totalCommentCount() {
    var total = 0;
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
  // `.blk[data-hash]` in the live document. Run after every DOM swap
  // (initial load, and every live-reload body replacement) rather than
  // trusting the server's `unanchored` field past the very first load,
  // since the file can change underneath us at any time.
  function recomputeUnanchored() {
    var elements = blockElements();
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
          // No file open yet — not a failure, just an empty state. See the
          // design doc's section 5: no banner, no form, just the
          // placeholder renderAside() shows for state.noFileOpen.
          state.loaded = false;
          state.noFileOpen = true;
          state.error = null;
          render();
          return;
        }
        if (!result.ok) {
          state.loaded = false;
          state.error =
            "レビューの読み込みに失敗したため保存は無効です: " +
            errorMessage(result.payload, "unknown error");
          render();
          return;
        }
        state.doc = {
          version: result.payload.version,
          file: result.payload.file,
          blocks: result.payload.blocks || [],
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
          "レビューの読み込みに失敗したため保存は無効です: network error";
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
        state.error = "エクスポートに失敗しました";
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
            "Exported (コピー失敗。ファイルは保存済み)";
        }
        render();
      }
    );
    toast.appendChild(copyBtn);

    toast.appendChild(
      button("review-toast-close", "閉じる", function () {
        state.toast = null;
        render();
      })
    );
    return toast;
  }

  // -- mutations (always followed by a save + re-render) -----------------

  function addComment(hash, excerpt, text) {
    var block = ensureBlock(hash, excerpt);
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

  function reanchor(oldHash, newHash, newExcerpt) {
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

  // -- rendering ----------------------------------------------------------

  function applyMarkers() {
    var elements = blockElements();
    for (var i = 0; i < elements.length; i++) {
      var blockEl = elements[i];
      var hash = blockEl.getAttribute("data-hash");
      var count = commentCount(hash);
      if (count > 0) {
        blockEl.classList.add("has-comments");
        blockEl.setAttribute("data-count", String(count));
      } else {
        blockEl.classList.remove("has-comments");
        blockEl.removeAttribute("data-count");
      }
      blockEl.classList.toggle("selected", hash === state.selectedHash);
      blockEl.onclick = selectBlockHandler(hash);
    }
  }

  function selectBlockHandler(hash) {
    return function () {
      selectBlock(hash, { expandIfCollapsed: true });
    };
  }

  function render() {
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
      // Section 5 of the design doc: only this placeholder, no header, no
      // form. loadReview() re-fetches once a file is open (see
      // onBodyReplaced()), which replaces this with the real pane.
      asideEl.appendChild(
        el(
          "p",
          "review-placeholder",
          "ファイルを開くとここにレビューが表示されます"
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
        el("p", "review-placeholder", "レビュー機能は利用できません")
      );
      body.appendChild(
        button("review-retry", "再読み込み", function () {
          loadReview();
        })
      );
    } else if (!state.selectedHash) {
      body.appendChild(el("p", "review-placeholder", "ブロックをクリック"));
    } else {
      body.appendChild(buildSelectedBlockView());
    }
    asideEl.appendChild(body);

    if (state.loaded) {
      asideEl.appendChild(buildUnanchoredSection());
    }
  }

  function buildSelectedBlockView() {
    var wrap = el("div", "review-selected");
    var hash = state.selectedHash;
    var block = findBlock(hash);
    var blockEl = findBlockElement(hash);
    var excerpt = block ? block.excerpt : blockEl ? excerptForBlockElement(blockEl) : "";

    var quoteWrap = el("div", "review-quote-wrap");
    var lineLabel = lineRangeLabel(blockEl);
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

    wrap.appendChild(buildCommentForm(hash, excerpt));
    return wrap;
  }

  function buildCommentItem(hash, comment) {
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
          editComment(hash, comment.id, value);
        }
      };
      actions.appendChild(button("review-save", "保存", save));
      actions.appendChild(
        button("review-cancel", "キャンセル", function () {
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
      button("review-comment-edit", "編集", function () {
        state.editingId = comment.id;
        render();
      })
    );
    actions2.appendChild(
      button("review-comment-delete", "削除", function () {
        deleteComment(hash, comment.id);
      })
    );
    item.appendChild(actions2);
    return item;
  }

  function buildCommentForm(hash, excerpt) {
    var form = el("div", "review-form");
    var textarea = el("textarea", "review-textarea");
    textarea.placeholder = "コメントを入力… (Cmd/Ctrl+Enter で保存)";
    form.appendChild(textarea);

    var submit = function () {
      var value = textarea.value.trim();
      if (!value) {
        return;
      }
      textarea.value = "";
      addComment(hash, excerpt, value);
    };
    form.appendChild(button("review-save", "保存", submit));

    textarea.addEventListener("keydown", function (event) {
      if ((event.metaKey || event.ctrlKey) && event.key === "Enter") {
        submit();
      }
    });
    return form;
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
      "選択中ブロックへ付け直す",
      function () {
        if (!state.selectedHash) {
          return;
        }
        var targetHash = state.selectedHash;
        var targetEl = findBlockElement(targetHash);
        var excerpt = targetEl
          ? excerptForBlockElement(targetEl)
          : block.excerpt;
        reanchor(block.hash, targetHash, excerpt);
        scrollBlockIntoView(targetHash);
      }
    );
    reanchorBtn.disabled = !state.selectedHash;
    actions.appendChild(reanchorBtn);
    actions.appendChild(
      button("review-unanchored-delete", "削除", function () {
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
      // than sitting on the stale "no file open" placeholder. See section
      // 5 of the design doc.
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
    render();
    loadReview();
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }
})();

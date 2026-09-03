(function () {
  "use strict";

  // Left-hand file tree: lists every Markdown file under the open
  // document's parent directory (`GET /tree`), lets folders be collapsed,
  // highlights the currently-open file, and switches the current window's
  // file in place on click (`PUT /open`).
  // Pane resize/collapse follows the same `mdview.tree.*`
  // localStorage/pointer-capture pattern assets/review.js uses for the
  // right-hand review pane — see its comments for the reasoning behind
  // each piece; this file only re-explains what differs (the pane sits at
  // the *left* edge, so its width tracks the cursor directly rather than
  // `innerWidth - clientX`).

  var TREE_URL = "/tree";
  var OPEN_URL = "/open";
  var REQUEST_HEADERS = { "X-Mdview-Request": "1" };

  // Whole-pane width/collapse — mirrors assets/review.js's
  // mdview.review.width/mdview.review.collapsed for the right pane.
  var PANE_WIDTH_KEY = "mdview.tree.width";
  var PANE_COLLAPSED_KEY = "mdview.tree.collapsed";
  var PANE_DEFAULT_WIDTH = 240;
  var PANE_MIN_WIDTH = 160;

  // Per-folder expand/collapse state — independent of the whole-pane
  // collapse above: a *set* of relative folder paths (as reported by GET
  // /tree's "path" field) that are currently collapsed, so any number of
  // folders can remember their own state.
  var COLLAPSED_FOLDERS_KEY = "mdview.tree.collapsedFolders";

  var state = {
    root: "",
    current: null,
    entries: [],
    truncated: false,
    loading: true,
    // True only for the specific "no file open yet" case (GET /tree ->
    // 409) — mirrors assets/review.js's state.noFileOpen.
    noFileOpen: false,
    error: null,
  };

  var collapsedFolders = null; // lazily loaded — see loadStoredCollapsedFolders().

  var paneState = {
    width: PANE_DEFAULT_WIDTH,
    collapsed: false,
    dragging: false,
  };

  var treeEl = null;
  var layoutEl = null;
  var splitterEl = null;
  var collapseTabEl = null;

  // -- DOM helpers, all textContent-based so nothing here ever builds HTML
  //    strings out of a file/folder name (user-controlled: it's whatever
  //    is on disk). ------------------------------------------------------

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
  //    must never throw its way into a broken pane — just fall back to the
  //    defaults. -----------------------------------------------------------

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

  // -- collapsed-folder state -------------------------------------------

  function loadStoredCollapsedFolders() {
    var raw = safeStorageGet(COLLAPSED_FOLDERS_KEY);
    if (!raw) {
      return new Set();
    }
    try {
      var parsed = JSON.parse(raw);
      return Array.isArray(parsed) ? new Set(parsed) : new Set();
    } catch (err) {
      return new Set();
    }
  }

  function persistCollapsedFolders() {
    safeStorageSet(
      COLLAPSED_FOLDERS_KEY,
      JSON.stringify(Array.from(collapsedFolders))
    );
  }

  function ensureCollapsedFolders() {
    if (!collapsedFolders) {
      collapsedFolders = loadStoredCollapsedFolders();
    }
    return collapsedFolders;
  }

  function toggleFolder(path) {
    var folders = ensureCollapsedFolders();
    if (folders.has(path)) {
      folders.delete(path);
    } else {
      folders.add(path);
    }
    persistCollapsedFolders();
    applyCollapsedFolders();
  }

  // Every folder path that's an ancestor of `path` (its own directory, that
  // directory's directory, and so on up to the root) — e.g.
  // "a/b/c.md" -> ["a", "a/b"]. Used to decide whether a row should be
  // hidden because *some* enclosing folder (not necessarily its immediate
  // parent) is collapsed.
  function ancestorFolders(path) {
    var parts = path.split("/");
    var acc = [];
    var result = [];
    for (var i = 0; i < parts.length - 1; i++) {
      acc.push(parts[i]);
      result.push(acc.join("/"));
    }
    return result;
  }

  function applyCollapsedFolders() {
    if (!treeEl) {
      return;
    }
    var folders = ensureCollapsedFolders();
    var rows = treeEl.querySelectorAll(".tree-item");
    for (var i = 0; i < rows.length; i++) {
      var row = rows[i];
      var path = row.dataset.path;
      var hidden = ancestorFolders(path).some(function (folder) {
        return folders.has(folder);
      });
      row.style.display = hidden ? "none" : "";
      if (row.classList.contains("tree-dir")) {
        row.classList.toggle("collapsed", folders.has(path));
      }
    }
  }

  // -- whole-pane resize/collapse ----------------------------------------

  function maxPaneWidth() {
    return window.innerWidth * 0.4;
  }

  function clampPaneWidth(width) {
    var max = maxPaneWidth();
    var min = Math.min(PANE_MIN_WIDTH, max);
    return Math.min(Math.max(width, min), max);
  }

  function applyPaneWidth(width) {
    paneState.width = width;
    if (treeEl) {
      treeEl.style.width = width + "px";
    }
  }

  function persistPaneWidth() {
    safeStorageSet(PANE_WIDTH_KEY, String(paneState.width));
  }

  function loadStoredPaneWidth() {
    var raw = safeStorageGet(PANE_WIDTH_KEY);
    var parsed = raw === null ? NaN : parseFloat(raw);
    return clampPaneWidth(isNaN(parsed) ? PANE_DEFAULT_WIDTH : parsed);
  }

  function loadStoredPaneCollapsed() {
    return safeStorageGet(PANE_COLLAPSED_KEY) === "1";
  }

  function updateCollapseTabLabel() {
    if (collapseTabEl) {
      collapseTabEl.textContent = "Files";
    }
  }

  function applyPaneCollapsed(collapsed) {
    paneState.collapsed = collapsed;
    if (!collapsed) {
      // The window may have been resized narrower while the pane sat
      // collapsed, so a stored width that was valid when it collapsed
      // could now exceed the current 40% cap — reclamp on the way out.
      applyPaneWidth(clampPaneWidth(paneState.width));
      persistPaneWidth();
    }
    if (layoutEl) {
      layoutEl.classList.toggle("tree-collapsed", collapsed);
    }
    if (collapseTabEl) {
      collapseTabEl.style.display = collapsed ? "flex" : "none";
    }
    updateCollapseTabLabel();
    safeStorageSet(PANE_COLLAPSED_KEY, collapsed ? "1" : "0");
  }

  // Pointer Events + setPointerCapture, same reasoning as
  // assets/review.js's splitter handling — see its comments.
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
    if (event.buttons === 0) {
      endSplitterDrag(event);
      return;
    }
    // The tree pane sits at the *left* edge of the layout (unlike the
    // review pane, which sits at the right), so its width is simply the
    // cursor's distance from the window's left edge.
    applyPaneWidth(clampPaneWidth(event.clientX));
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
    splitterEl = document.getElementById("tree-splitter");

    collapseTabEl = button("tree-collapse-tab", "", function () {
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

  // -- keyboard shortcut: ⌘⇧E / Ctrl+Shift+E toggles the tree pane -------

  function onGlobalKeydown(event) {
    var withModifier = (event.metaKey || event.ctrlKey) && !event.altKey;
    if (withModifier && event.shiftKey && (event.key === "e" || event.key === "E")) {
      event.preventDefault();
      applyPaneCollapsed(!paneState.collapsed);
    }
  }

  // -- network ------------------------------------------------------------

  function errorMessage(payload, fallback) {
    return (payload && typeof payload.error === "string" && payload.error) || fallback;
  }

  function loadTree() {
    state.loading = true;
    state.noFileOpen = false;
    render();
    fetch(TREE_URL, { method: "GET", cache: "no-store", headers: REQUEST_HEADERS })
      .then(function (response) {
        return response.json().then(function (payload) {
          return { status: response.status, ok: response.ok, payload: payload };
        });
      })
      .then(function (result) {
        state.loading = false;
        if (result.status === 409) {
          // No file open yet — not a failure, just an empty state (mirrors
          // assets/review.js's own GET /review 409 handling).
          state.noFileOpen = true;
          state.error = null;
          render();
          return;
        }
        if (!result.ok) {
          state.noFileOpen = false;
          state.error = errorMessage(result.payload, "unknown error");
          render();
          return;
        }
        state.noFileOpen = false;
        state.error = null;
        state.root = typeof result.payload.root === "string" ? result.payload.root : "";
        state.current =
          typeof result.payload.current === "string" ? result.payload.current : null;
        state.entries = Array.isArray(result.payload.entries) ? result.payload.entries : [];
        state.truncated = result.payload.truncated === true;
        render();
      })
      .catch(function () {
        state.loading = false;
        state.error = "Couldn't load the file tree";
        render();
      });
  }

  function openFile(path) {
    state.error = null;
    render();
    fetch(OPEN_URL, {
      method: "PUT",
      cache: "no-store",
      headers: Object.assign({ "Content-Type": "application/json" }, REQUEST_HEADERS),
      body: JSON.stringify({ path: path }),
    })
      .then(function (response) {
        if (response.ok) {
          return response
            .json()
            .then(function (payload) {
              // The native app always ends up reloading *some* window in
              // response to a successful PUT /open (this one, the common
              // case — or, if the target is already open elsewhere, that
              // other window instead, while this one is left untouched;
              // see app.rs's UserEvent::SwitchFile) — "reloaded: true"
              // covers both, so this never needs to reload itself. The
              // `!== true` fallback (rather than trusting the flag
              // outright) exists for robustness — e.g. a malformed/older
              // response — even though --browser mode never reaches here
              // at all (PUT /open is always 501 there).
              if (payload.reloaded !== true) {
                location.reload();
              }
            })
            .catch(function () {
              // 200 with an unparseable body: fall back to reloading
              // ourselves rather than leaving the view stale.
              location.reload();
            });
        }
        return response
          .json()
          .then(function (payload) {
            state.error = errorMessage(payload, "Couldn't open file");
            render();
          })
          .catch(function () {
            state.error = "Couldn't open file";
            render();
          });
      })
      .catch(function () {
        state.error = "Couldn't open file";
        render();
      });
  }

  // -- rendering ------------------------------------------------------------

  function buildRow(entry) {
    var depth = entry.path.split("/").length - 1;
    var row = el("div", "tree-item");
    row.classList.add(entry.kind === "dir" ? "tree-dir" : "tree-file");
    // Path goes in dataset, never an attribute built from string
    // concatenation — see the module docs.
    row.dataset.path = entry.path;
    row.dataset.kind = entry.kind;
    row.style.paddingLeft = 0.4 + depth * 1.1 + "rem";

    if (entry.kind === "dir") {
      row.appendChild(el("span", "tree-toggle", "▾"));
    } else {
      row.appendChild(el("span", "tree-spacer", ""));
    }
    row.appendChild(el("span", "tree-name", entry.name));
    return row;
  }

  function highlightCurrent() {
    if (!treeEl) {
      return;
    }
    var rows = treeEl.querySelectorAll(".tree-item");
    for (var i = 0; i < rows.length; i++) {
      rows[i].classList.toggle("tree-current", rows[i].dataset.path === state.current);
    }
  }

  function buildHeader() {
    var header = el("div", "tree-header");
    header.appendChild(el("span", "tree-root-name", state.root));
    header.appendChild(
      button("tree-collapse-btn", "⟨", function () {
        applyPaneCollapsed(true);
      })
    );
    return header;
  }

  function render() {
    if (!treeEl) {
      return;
    }
    clearChildren(treeEl);
    updateCollapseTabLabel();

    if (state.noFileOpen) {
      // Mirrors assets/review.js's own "no file open" placeholder: no
      // header, just the placeholder. loadTree() re-fetches once a file is
      // open (see onBodyReplaced()).
      treeEl.appendChild(
        el("p", "tree-placeholder", "Open a file to see its tree here")
      );
      return;
    }

    treeEl.appendChild(buildHeader());

    if (state.error) {
      treeEl.appendChild(el("div", "tree-error", state.error));
    }

    if (state.loading && state.entries.length === 0) {
      treeEl.appendChild(el("p", "tree-placeholder", "Loading…"));
      return;
    }

    if (state.entries.length === 0) {
      treeEl.appendChild(el("p", "tree-placeholder", "No Markdown files found"));
      return;
    }

    var list = el("div", "tree-list");
    for (var i = 0; i < state.entries.length; i++) {
      list.appendChild(buildRow(state.entries[i]));
    }
    treeEl.appendChild(list);
    applyCollapsedFolders();
    highlightCurrent();

    if (state.truncated) {
      treeEl.appendChild(
        el("p", "tree-truncated", "Some files/folders are not shown")
      );
    }
  }

  function onTreeClick(event) {
    var row = event.target.closest(".tree-item");
    if (!row || !treeEl || !treeEl.contains(row)) {
      return;
    }
    var path = row.dataset.path;
    if (row.classList.contains("tree-dir")) {
      toggleFolder(path);
    } else {
      openFile(path);
    }
  }

  // -- entry points ---------------------------------------------------

  function onBodyReplaced() {
    // The DOM under <main> was just swapped out from under us (live
    // reload) — the file/folder set on disk may have changed, so refetch
    // rather than trusting whatever the tree already shows.
    loadTree();
  }

  window.__mdviewTree = {
    onBodyReplaced: onBodyReplaced,
    // Exposed so assets/viewer.js's relative-link click handler can reuse
    // this exact PUT /open logic instead of duplicating it — see its
    // module docs.
    openFile: openFile,
    // The currently-open file's root-relative path (`GET /tree`'s
    // `"current"`), or `null` before the first successful `GET /tree`
    // resolves (or if none is open). Also consumed by viewer.js, to
    // resolve a relative link's target directory and to populate the doc
    // header's path label.
    getCurrent: function () {
      return state.current;
    },
  };

  function init() {
    treeEl = document.getElementById("tree");
    if (!treeEl) {
      return;
    }
    initPane();
    treeEl.addEventListener("click", onTreeClick);
    document.addEventListener("keydown", onGlobalKeydown);
    render();
    loadTree();
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }
})();

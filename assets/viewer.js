(function () {
  "use strict";

  // Whole-document zoom. `document.documentElement.style.fontSize` is set
  // to a percentage of the browser default (100% == 1.0), which the
  // rem-based sizes in assets/style.css (`.markdown-body`, `.review`) scale
  // against automatically — see render.rs's `page()` for where this script
  // is embedded (right before live.js).
  //
  // Persisted via `storage` (below) so it survives a reload; naturally
  // survives a live-reload body swap too, since it's set on <html>, which
  // live.js's `refreshBody()` never touches (only `<main>`'s innerHTML is
  // replaced).

  var ZOOM_STORAGE_KEY = "mdview.zoom";
  var ZOOM_MIN = 0.5;
  var ZOOM_MAX = 2.0;
  var ZOOM_STEP = 0.1;
  var ZOOM_DEFAULT = 1.0;

  // Posts `msg` to the native host over wry's IPC bridge, if one exists
  // (it does in the native window — see `with_ipc_handler` in app.rs,
  // which echoes it to stderr as `[markdown-remarkable:js] <msg>` under
  // `MDVIEW_DEBUG=1`; it doesn't in `--browser` mode, where `window.ipc`
  // is simply absent and this is a silent no-op).
  function post(msg) {
    if (window.ipc && typeof window.ipc.postMessage === "function") {
      window.ipc.postMessage(msg);
    }
  }

  // Persistence with a fallback chain: localStorage, then sessionStorage,
  // then an in-memory object, so a WebView/sandbox that disables
  // persistent storage (seen in some native-embedder configurations)
  // degrades to "zoom doesn't survive a restart" instead of throwing on
  // every read/write. Which tier actually got used is reported once via
  // `post()` above so it's visible in `MDVIEW_DEBUG=1` output — see
  // docs/qa/baseline-checklist.md.
  //
  // Every tier is routed through this wrapper, including the common case
  // where localStorage works fine: one code path for get/set rather than
  // a "does the fallback path even work" question nobody exercises until
  // the day localStorage actually is unavailable.
  var storage = (function () {
    function probe(area) {
      try {
        area.setItem("mdview.probe", "1");
        area.removeItem("mdview.probe");
        return true;
      } catch (err) {
        return { name: (err && err.name) || "Error" };
      }
    }

    function backedBy(area) {
      return {
        get: function (key) {
          try {
            return area.getItem(key);
          } catch (err) {
            return null;
          }
        },
        set: function (key, value) {
          try {
            area.setItem(key, value);
          } catch (err) {
            // Best-effort — nothing further to fall back to from here
            // without losing the tier this call already committed to.
          }
        },
      };
    }

    function memoryBacked() {
      var memory = {};
      return {
        get: function (key) {
          return Object.prototype.hasOwnProperty.call(memory, key)
            ? memory[key]
            : null;
        },
        set: function (key, value) {
          memory[key] = value;
        },
      };
    }

    var localResult = probe(window.localStorage);
    if (localResult === true) {
      post("storage: ok");
      return backedBy(window.localStorage);
    }
    post("storage: unavailable " + localResult.name);

    var sessionResult = probe(window.sessionStorage);
    if (sessionResult === true) {
      return backedBy(window.sessionStorage);
    }

    return memoryBacked();
  })();

  // Rounds to one decimal place (avoiding float drift across repeated +/-
  // presses, e.g. 0.1 + 0.2 in JS) and clamps to [ZOOM_MIN, ZOOM_MAX].
  function normalizeZoom(value) {
    var rounded = Math.round(value * 10) / 10;
    return Math.min(ZOOM_MAX, Math.max(ZOOM_MIN, rounded));
  }

  function loadStoredZoom() {
    var raw = storage.get(ZOOM_STORAGE_KEY);
    var parsed = raw === null ? NaN : parseFloat(raw);
    return isNaN(parsed) ? ZOOM_DEFAULT : normalizeZoom(parsed);
  }

  var zoom = loadStoredZoom();

  function applyZoom() {
    document.documentElement.style.fontSize = 100 * zoom + "%";
  }

  function setZoom(value) {
    zoom = normalizeZoom(value);
    applyZoom();
    storage.set(ZOOM_STORAGE_KEY, String(zoom));
  }

  // `dir` is "in" / "out" / "reset" — the same vocabulary app.rs's
  // `evaluate_script("window.__mdviewViewer.zoom('in')")` calls use for the
  // macOS View menu (see install_menu/UserEvent::Zoom in app.rs).
  function zoom_(dir) {
    if (dir === "in") {
      setZoom(zoom + ZOOM_STEP);
    } else if (dir === "out") {
      setZoom(zoom - ZOOM_STEP);
    } else if (dir === "reset") {
      setZoom(ZOOM_DEFAULT);
    }
  }

  function reload() {
    location.reload();
  }

  applyZoom();

  window.__mdviewViewer = { zoom: zoom_, reload: reload };

  // -- relative-link navigation, http(s) links in --browser mode, and
  //    back/forward history (doc header + ⌘[/⌘]) ---------------------------
  //
  // The document is served through a single fixed URL (the custom
  // `mdview://` protocol) rather than one that reflects the open file's
  // actual location on disk, so a bare `<a href="other.md">` can't be left
  // to the WebView's own relative-URL resolution — it would resolve against
  // that fixed serving URL, not against the Markdown file that actually
  // contains the link. This delegated click handler intercepts a relative
  // `.md`/`.markdown` link, resolves it against the *currently open file's*
  // directory (not the page's own URL) itself, and switches the window to
  // it the same way the file tree does (`PUT /open` — see assets/tree.js's
  // `openFile`, reused here rather than duplicated).
  //
  // Native window only — see `isBrowserMode()`'s uses below. `--browser`
  // has no per-window history to switch through (`PUT /open`/`PUT /nav` are
  // both always `501` there — see routes::handle's docs), so this whole
  // feature is a no-op left to the WebView's default click behavior in that
  // mode, same as before it existed.

  var REQUEST_HEADERS = { "X-Mdview-Request": "1" };
  var NAV_URL = "/nav";
  var TREE_URL = "/tree";

  function isBrowserMode() {
    return document.body && document.body.getAttribute("data-mode") === "browser";
  }

  // `true` for an `href` naming an `http(s)` target — the only scheme
  // [`onDocumentClick`] treats specially (see its docs); everything else
  // (mailto:, a same-page fragment, a relative reference) is left to the
  // browser's/WebView's own default click behavior or the relative-link
  // branch below.
  function isHttpHref(href) {
    return /^https?:\/\//i.test(href);
  }

  // `true` if `href` carries no URL scheme at all and isn't a bare
  // same-page fragment (`#...`) — covers both a plain relative reference
  // (`"other.md"`) *and* a root-relative one (`"/other.md"`,
  // `"//host/other.md"`); [`onDocumentClick`] tells those two apart itself
  // afterward (see `isRootRelativeHref`) since they're handled differently,
  // but both need to be routed away from the WebView's own default click
  // behavior in the native window either way. Mirrors render.rs's
  // `is_safe_link_target`'s own scheme detection (a `/`, `?`, or `#` before
  // the first `:` means it isn't introducing a scheme), simplified since
  // only already-sanitized targets ever reach the DOM in the first place
  // (`to_html`'s `sanitize_link_target` already neutralized anything
  // unsafe to `#`).
  function isSchemelessReference(href) {
    if (href.charAt(0) === "#") {
      return false;
    }
    var colonIdx = href.indexOf(":");
    if (colonIdx === -1) {
      return true;
    }
    return /[/?#]/.test(href.slice(0, colonIdx));
  }

  // `true` for a root-relative reference — a single leading `/`
  // (`"/other.md"`) or a protocol-relative one (`"//host/other.md"`, itself
  // already rejected server-side by render.rs's `is_safe_link_target` and
  // neutralized to `#` — this only exists so a stray one can't slip past
  // as if it were plain-relative if that ever changes). This app only ever
  // resolves a link against the *currently open file's own directory*
  // (`resolveRelativePath`), never against any fixed root — so a
  // root-relative href is never something `onDocumentClick` can turn into
  // a `PUT /open` target; it's routed to the same inert treatment as a
  // relative link to a non-Markdown file instead.
  function isRootRelativeHref(href) {
    return href.charAt(0) === "/";
  }

  // The lowercased extension of `path` (no leading `.`), or `null` if it
  // has none. Deliberately read off the still percent-encoded href (an
  // extension is never itself percent-encoded in practice, and this only
  // has to decide *whether* to treat the click as a markdown link at all —
  // the actual path used to resolve/switch is decoded separately, in
  // `resolveRelativePath`).
  function extensionOf(path) {
    var match = /\.([A-Za-z0-9]+)$/.exec(path);
    return match ? match[1].toLowerCase() : null;
  }

  function isMarkdownExtension(path) {
    var ext = extensionOf(path);
    return ext === "md" || ext === "markdown";
  }

  // The directory portion of a root-relative path (as reported by `GET
  // /tree`'s `"current"` field, already plain text — a JSON string, not
  // percent-encoded) — `"sub/c.md"` -> `"sub"`, `"doc.md"` -> `""` (the
  // root itself).
  function dirOf(path) {
    var idx = path.lastIndexOf("/");
    return idx === -1 ? "" : path.slice(0, idx);
  }

  // `decodeURIComponent(segment)`, or `segment` itself if decoding throws
  // (a malformed `%` escape) — best-effort, same as every other decode
  // fallback in this codebase.
  function decodeSegment(segment) {
    try {
      return decodeURIComponent(segment);
    } catch (err) {
      return segment;
    }
  }

  // Resolves `href` (a plain relative reference, e.g. `"../b.md"` or
  // `"sub/c.md"`, still exactly as it appears in the rendered `<a href>`)
  // against `dir` (the *current file's own directory*, root-relative — see
  // `dirOf`) into a root-relative path with every `.`/`..` segment
  // collapsed away, or `null` if it isn't resolvable at all.
  //
  // Each `/`-split segment of `href` is `decodeURIComponent`d before being
  // compared/pushed — `render::to_html`'s `escape_href` (pulldown-cmark)
  // percent-encodes spaces and non-ASCII bytes into a rendered `<a href>`
  // (ASCII letters/digits are left alone), so a link to e.g.
  // `café note.md` arrives here as `caf%C3%A9%20note.md` and has to be
  // decoded back before it means anything as a path —
  // `routes::handle_open`'s doc comment on `path` documents this same
  // contract from the server side. Decoding can turn a segment that wasn't
  // literally `".."`/`"."` in the source `href` into one that now is
  // (`%2e%2e` -> `".."`) — that's fine, the normalization below still runs
  // *after* decoding, so it's caught exactly like a literal `..` would be.
  //
  // `routes::handle_open`'s `is_plain_relative_path` rejects a path
  // carrying *any* `.`/`..` component, even mid-path, so this has to fully
  // normalize rather than just prefix-join — sending `"sub/../other.md"`
  // straight through would always come back `400`. Unlike a plain prefix
  // join, though, a `..` that would climb *above* `dir` itself (the
  // already-resolved stack going empty) makes this return `null` outright
  // instead of silently dropping just that one `..` — dropping it would
  // silently land on a same-named file in a *different* directory instead
  // of refusing the link, which is worse than doing nothing.
  function resolveRelativePath(dir, href) {
    var stack = dir ? dir.split("/") : [];
    var segments = href.split("/");
    for (var i = 0; i < segments.length; i++) {
      var segment = decodeSegment(segments[i]);
      // A decoded segment carrying its own `/` (`%2f`) or `\` (`%5c`) would
      // otherwise smuggle an extra path-separator-like character across a
      // segment boundary this function already finished splitting on —
      // rejected outright rather than silently joined into the next
      // pushed/popped segment as if it were plain text.
      if (segment.indexOf("/") !== -1 || segment.indexOf("\\") !== -1) {
        return null;
      }
      if (segment === "" || segment === ".") {
        continue;
      }
      if (segment === "..") {
        if (stack.length === 0) {
          return null;
        }
        stack.pop();
        continue;
      }
      stack.push(segment);
    }
    return stack.join("/");
  }

  // The currently-open file's root-relative path (`GET /tree`'s
  // `"current"`), preferring assets/tree.js's already-fetched value
  // (`window.__mdviewTree.getCurrent()` — available once its own `GET
  // /tree` resolves) and falling back to a direct `GET /tree` of our own
  // only when that isn't available yet (e.g. this runs before tree.js's
  // fetch has completed). `callback` is always called exactly once, with
  // the path or `null` (no file open, or the request failed).
  function currentRelativePath(callback) {
    if (window.__mdviewTree && typeof window.__mdviewTree.getCurrent === "function") {
      var known = window.__mdviewTree.getCurrent();
      if (known) {
        callback(known);
        return;
      }
    }
    fetch(TREE_URL, { method: "GET", cache: "no-store", headers: REQUEST_HEADERS })
      .then(function (response) {
        return response.ok ? response.json() : null;
      })
      .then(function (payload) {
        callback(payload && typeof payload.current === "string" ? payload.current : null);
      })
      .catch(function () {
        callback(null);
      });
  }

  function openRelativeFile(targetPath) {
    if (window.__mdviewTree && typeof window.__mdviewTree.openFile === "function") {
      window.__mdviewTree.openFile(targetPath);
    } else {
      // Shouldn't happen (tree.js always defines this before any click
      // could reach here), but fail silently rather than throw if it ever
      // does.
      console.warn("markdown-remarkable: file tree is not available; cannot open " + targetPath);
    }
  }

  // Handles both "click" (the primary/left button, on every engine) and
  // "auxclick" (the middle/right buttons — modern WebKit/Chromium-based
  // engines, which is every engine this app ships on, fire "auxclick" for
  // those rather than "click"; registered as a second listener for the
  // same function below). `event.button` tells the two apart: `0` for a
  // primary click, `1` for a middle click. A right click (`2`) is ignored
  // outright — it opens a context menu, not a navigation.
  function onDocumentClick(event) {
    if (event.defaultPrevented || (event.button !== 0 && event.button !== 1)) {
      return;
    }
    var anchor = event.target.closest ? event.target.closest("a[href]") : null;
    if (!anchor || !anchor.closest("main.doc")) {
      return;
    }
    var href = anchor.getAttribute("href");
    if (!href) {
      return;
    }
    var isMiddleClick = event.button === 1;
    var hasModifier = event.metaKey || event.ctrlKey || event.shiftKey || event.altKey;

    if (isHttpHref(href)) {
      if (hasModifier || isMiddleClick) {
        // Left entirely to the browser's/WebView's own modified/middle-
        // click behavior (e.g. a real browser's "open in a new tab")
        // rather than our own window.open() substitute below.
        return;
      }
      if (isBrowserMode()) {
        // In --browser mode, clicking a link navigates the same tab away
        // from the app entirely (there's no separate native window for
        // navigation_policy to intervene in) — open it in a new tab
        // instead, same as the native window's own http(s) handling.
        event.preventDefault();
        // No return-value fallback here: per the HTML spec, window.open()
        // with "noopener" in its features always returns null regardless
        // of whether the new tab actually opened, so there's no way to
        // tell success from failure (e.g. a popup blocker) from the return
        // value alone — a `!opened` fallback would run unconditionally and
        // double-navigate (a new tab *and* this one), losing this tab's
        // still-open document (and any unsaved review comment draft) to
        // the link's URL in the process.
        window.open(href, "_blank", "noopener");
      }
      // Native window: left as the default click action — wry's
      // navigation_policy (app.rs) intercepts it and hands it to the OS
      // browser.
      return;
    }

    if (!isSchemelessReference(href)) {
      // A fragment (#...), mailto:, or anything else — left to the
      // browser's default behavior.
      return;
    }
    var withoutHash = href.split("#")[0];
    var withoutQuery = withoutHash.split("?")[0];

    if (isRootRelativeHref(href) || !isMarkdownExtension(withoutQuery)) {
      // A root-relative reference (`isRootRelativeHref` — this app never
      // resolves a link against anything but the currently open file's own
      // directory, so there's no sensible target to compute for one) or a
      // relative link to something other than a Markdown file. Left
      // completely inert in the native window: the WebView's own default
      // click would resolve it against the app's fixed `mdview://` origin
      // (an "internal" URL, which `navigation_policy` — app.rs — always
      // lets through) and land on a bare 404 page inside the app itself,
      // which this app has no way to recover from short of reopening the
      // file. `--browser` mode is left alone — a normal HTTP 404 in a
      // normal browser tab, same as before this feature existed, and
      // still recoverable with the browser's own back button.
      if (!isBrowserMode()) {
        event.preventDefault();
      }
      return;
    }

    if (isBrowserMode()) {
      // No per-window history/PUT /open under --browser (both are always
      // `501` there) — left entirely to the browser's own default click
      // behavior, same 404-then-back-button situation as the non-markdown
      // case above (`--browser` mode never reaches any of the branches
      // below).
      return;
    }
    if (hasModifier || isMiddleClick) {
      // Same 404-avoidance as the root-relative/non-markdown case above —
      // this app has no tabs/window list for a modified or middle click to
      // usefully open into.
      event.preventDefault();
      return;
    }

    event.preventDefault();
    // Stop this click from also reaching assets/review.js's own `.doc`
    // click handler (block/item selection) — the link is being taken over
    // for navigation, not selection, and letting both fire would select
    // the enclosing block *and* switch the window's file in the same
    // click.
    event.stopPropagation();
    currentRelativePath(function (current) {
      if (!current) {
        // No file open (shouldn't happen — a link only ever renders inside
        // an open document) or the lookup failed; nothing to resolve
        // against.
        return;
      }
      var target = resolveRelativePath(dirOf(current), withoutQuery);
      if (!target) {
        // `resolveRelativePath` returned `null` — either the link's `..`s
        // climb above the currently open file's own root-relative
        // directory, or a decoded segment carried a stray `/`/`\`.
        // Already prevented above; nothing further to do (see its docs on
        // why this refuses rather than silently landing on some other,
        // same-named file).
        return;
      }
      openRelativeFile(target);
    });
  }

  document.addEventListener("click", onDocumentClick, true);
  document.addEventListener("auxclick", onDocumentClick, true);

  // -- doc header: back/forward buttons + current path -------------------

  // Populated by initDocHeader() — kept at module scope (rather than local
  // to that function) so navigate() can refresh their disabled state after
  // every PUT /nav response, not just at page load.
  var docBackBtn = null;
  var docForwardBtn = null;

  function setNavButtonDisabled(button, disabled) {
    if (button) {
      button.disabled = disabled;
    }
  }

  function refreshNavState() {
    fetch(NAV_URL, { method: "GET", cache: "no-store", headers: REQUEST_HEADERS })
      .then(function (response) {
        return response.ok ? response.json() : null;
      })
      .then(function (payload) {
        setNavButtonDisabled(docBackBtn, !(payload && payload.back === true));
        setNavButtonDisabled(docForwardBtn, !(payload && payload.forward === true));
      })
      .catch(function () {
        setNavButtonDisabled(docBackBtn, true);
        setNavButtonDisabled(docForwardBtn, true);
      });
  }

  function navigate(dir) {
    if (isBrowserMode()) {
      // No per-window history under --browser (`PUT /nav` is always `501`
      // there) — nothing to do. The doc header's buttons are hidden by CSS
      // in this mode already; this guards the ⌘[/⌘] shortcut too.
      return;
    }
    fetch(NAV_URL, {
      method: "PUT",
      cache: "no-store",
      headers: Object.assign({ "Content-Type": "application/json" }, REQUEST_HEADERS),
      body: JSON.stringify({ dir: dir }),
    })
      .then(function (response) {
        if (!response.ok) {
          return;
        }
        return response
          .json()
          .then(function (payload) {
            // Same "reloaded" contract as PUT /open (assets/tree.js) — the
            // native app always ends up reloading some window on success,
            // so this only needs to reload itself as a fallback.
            if (payload.reloaded !== true) {
              location.reload();
            }
          })
          .catch(function () {
            location.reload();
          });
      })
      .catch(function () {
        // Ignore — refreshNavState() below still runs and resyncs the
        // buttons regardless.
      })
      .then(function () {
        // Always resync, success or failure: a request that lands but
        // doesn't change *this* window's own display (the history's target
        // was already open in a different window, which gets focused
        // instead — see app.rs's `UserEvent::Navigate`) still changes what
        // `GET /nav` would now report, and a location.reload() above (the
        // common case) makes this a harmless no-op racing a page teardown.
        refreshNavState();
      });
  }

  function initDocHeader() {
    docBackBtn = document.getElementById("doc-back");
    docForwardBtn = document.getElementById("doc-forward");
    var pathEl = document.getElementById("doc-header-path");
    if (!docBackBtn && !docForwardBtn && !pathEl) {
      // --export's standalone page has no doc header at all.
      return;
    }
    if (docBackBtn) {
      docBackBtn.addEventListener("click", function () {
        navigate("back");
      });
    }
    if (docForwardBtn) {
      docForwardBtn.addEventListener("click", function () {
        navigate("forward");
      });
    }
    refreshNavState();
    if (pathEl) {
      currentRelativePath(function (current) {
        pathEl.textContent = current || "";
      });
    }
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", initDocHeader);
  } else {
    initDocHeader();
  }

  // True while a TEXTAREA or INPUT holds focus — same check
  // assets/review.js's own global shortcuts use (`isTextInputFocused`
  // there), applied here so ⌘[/⌘] doesn't fire while typing a review
  // comment or a search field.
  function isTextInputFocused() {
    var active = document.activeElement;
    return !!active && (active.tagName === "TEXTAREA" || active.tagName === "INPUT");
  }

  // Keyboard shortcuts, for every mode/platform that has no native menu to
  // intercept them first: Windows, Linux, and `--browser` mode on every OS
  // (including macOS — the muda menu only exists in the native window). On
  // macOS's native window, the menu's accelerators (Cmd+=/-/0, see app.rs)
  // consume the keydown before the WebView ever sees it, so this listener
  // simply never fires there for those keys — nothing to double-handle.
  // ⌘[/⌘] (back/forward) has no menu accelerator at all, on any platform,
  // so unlike zoom this fires everywhere, macOS's native window included.
  document.addEventListener("keydown", function (event) {
    // `event.altKey` excluded: on layouts where AltGr is used to type
    // punctuation (many European keyboards), AltGr commonly arrives as
    // Ctrl+Alt together, which would otherwise misfire these shortcuts on
    // an ordinary keypress that never meant to invoke any of them.
    if (!(event.metaKey || event.ctrlKey) || event.altKey) {
      return;
    }
    // "+" needs Shift on most layouts, so it and the unshifted "=" both
    // mean zoom in (matching every browser's own Cmd/Ctrl+= shortcut).
    if (event.key === "+" || event.key === "=") {
      event.preventDefault();
      zoom_("in");
    } else if (event.key === "-") {
      event.preventDefault();
      zoom_("out");
    } else if (event.key === "0") {
      event.preventDefault();
      zoom_("reset");
    } else if (event.key === "[" && !isTextInputFocused() && !isBrowserMode()) {
      // `--browser` has no per-window history to move through (`PUT /nav`
      // is always `501` there) — left out of the branch entirely rather
      // than preventDefault-ing a keystroke that would otherwise still do
      // whatever the browser itself binds `⌘[`/`Ctrl+[` to (e.g. Safari's
      // own back-in-tab-history shortcut).
      event.preventDefault();
      navigate("back");
    } else if (event.key === "]" && !isTextInputFocused() && !isBrowserMode()) {
      event.preventDefault();
      navigate("forward");
    }
  });
})();

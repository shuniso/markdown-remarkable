(function () {
  "use strict";

  // Whole-document zoom. `document.documentElement.style.fontSize` is set
  // to a percentage of the browser default (100% == 1.0), which the
  // rem-based sizes in assets/style.css (`.markdown-body`, `.review`) scale
  // against automatically — see render.rs's `page()` for where this script
  // is embedded (right before live.js) and docs/superpowers/specs/
  // 2026-08-23-baseline-ux-design.md for the design.
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
  // which echoes it to stderr as `[mdview:js] <msg>` under
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

  // Keyboard shortcuts, for every mode/platform that has no native menu to
  // intercept them first: Windows, Linux, and `--browser` mode on every OS
  // (including macOS — the muda menu only exists in the native window). On
  // macOS's native window, the menu's accelerators (Cmd+=/-/0, see app.rs)
  // consume the keydown before the WebView ever sees it, so this listener
  // simply never fires there for those keys — nothing to double-handle.
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
    }
  });
})();

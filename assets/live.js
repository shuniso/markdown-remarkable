(function () {
  "use strict";

  // The server embeds the version that was current *before* it rendered
  // this page (see page()/respond_with_page in the Rust source), so this
  // baseline can never be newer than what's actually on screen.
  var baselineVersion = String(window.__mdviewVersion);
  var requestInFlight = false;
  var FETCH_TIMEOUT_MS = 5000;

  function fetchWithTimeout(url) {
    var controller = new AbortController();
    var timeoutId = setTimeout(function () {
      controller.abort();
    }, FETCH_TIMEOUT_MS);
    return fetch(url, { cache: "no-store", signal: controller.signal }).finally(
      function () {
        clearTimeout(timeoutId);
      }
    );
  }

  // Swaps in the freshly-saved content in place (preserving scroll
  // position) instead of a full page reload. Only falls back to
  // location.reload() if that itself fails — e.g. the request timed out,
  // /body responded with something other than 200 (a read failure is a
  // 200-with-error-text fragment, so that's not it), or this page has no
  // <main> to swap into.
  function refreshBody() {
    return fetchWithTimeout("/body")
      .then(function (response) {
        if (!response.ok) {
          throw new Error("unexpected status " + response.status);
        }
        var title = response.headers.get("X-Mdview-Title");
        return response.text().then(function (html) {
          var main = document.querySelector("main");
          if (!main) {
            throw new Error("no <main> to update");
          }
          main.innerHTML = html;
          if (title) {
            // The server percent-encodes the file name (headers are
            // ASCII-only); a malformed value just keeps the old title.
            try {
              document.title = decodeURIComponent(title);
            } catch (_) {}
          }
          if (window.__mdviewReview) {
            window.__mdviewReview.onBodyReplaced();
          }
          if (window.__mdviewTree) {
            window.__mdviewTree.onBodyReplaced();
          }
        });
      })
      .catch(function () {
        location.reload();
      });
  }

  function poll() {
    if (requestInFlight) {
      return;
    }
    requestInFlight = true;

    fetchWithTimeout("/version")
      .then(function (response) {
        return response.text();
      })
      .then(function (version) {
        if (version !== baselineVersion) {
          // Update the baseline optimistically: even if refreshBody() falls
          // back to a full reload, that reload replaces this whole script
          // (and its state) anyway, so there's no risk of getting stuck on
          // a stale baseline here.
          baselineVersion = version;
          return refreshBody();
        }
      })
      .catch(function () {
        // Ignore fetch failures and timeouts (e.g. the server restarting
        // mid-save); the next poll will pick things up once it's back.
      })
      .finally(function () {
        requestInFlight = false;
      });
  }

  setInterval(poll, 500);
})();

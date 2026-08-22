(function () {
  "use strict";

  // The server embeds the version that was current *before* it rendered
  // this page (see page()/respond_with_page in the Rust source), so this
  // baseline can never be newer than what's actually on screen.
  var baselineVersion = String(window.__mdviewVersion);
  var requestInFlight = false;
  var FETCH_TIMEOUT_MS = 5000;

  function poll() {
    if (requestInFlight) {
      return;
    }
    requestInFlight = true;

    var controller = new AbortController();
    var timeoutId = setTimeout(function () {
      controller.abort();
    }, FETCH_TIMEOUT_MS);

    fetch("/version", { cache: "no-store", signal: controller.signal })
      .then(function (response) {
        return response.text();
      })
      .then(function (version) {
        if (version !== baselineVersion) {
          location.reload();
        }
      })
      .catch(function () {
        // Ignore fetch failures and timeouts (e.g. the server restarting
        // mid-save); the next poll will pick things up once it's back.
      })
      .finally(function () {
        clearTimeout(timeoutId);
        requestInFlight = false;
      });
  }

  setInterval(poll, 500);
})();

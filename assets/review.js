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
    error: null,
    toast: null,
    unanchoredOpen: false,
  };

  var asideEl = null;

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
    render();
    fetch(REVIEW_URL, { method: "GET", cache: "no-store", headers: REQUEST_HEADERS })
      .then(function (response) {
        return response.json().then(function (payload) {
          return { ok: response.ok, payload: payload };
        });
      })
      .then(function (result) {
        state.loading = false;
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
        if (state.loaded) {
          state.error = null;
        }
        render();
      })
      .catch(function () {
        state.error = "保存に失敗しました";
        render();
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
      state.selectedHash = hash;
      state.editingId = null;
      render();
    };
  }

  function render() {
    applyMarkers();
    renderAside();
  }

  function renderAside() {
    if (!asideEl) {
      return;
    }
    clearChildren(asideEl);

    var header = el("div", "review-header");
    header.appendChild(
      el("span", "review-count", totalCommentCount() + " comments")
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
        var targetEl = findBlockElement(state.selectedHash);
        var excerpt = targetEl
          ? excerptForBlockElement(targetEl)
          : block.excerpt;
        reanchor(block.hash, state.selectedHash, excerpt);
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
    render();
    loadReview();
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }
})();

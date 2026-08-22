//! Markdown -> HTML rendering and the surrounding HTML page template.
//!
//! Everything in this module is a pure function: no I/O, no network. That
//! keeps it trivially unit-testable and keeps `server.rs` a thin adapter
//! that reads a file, calls into here, and writes the response.

use pulldown_cmark::{html, CowStr, Event, Options, Parser, Tag, TagEnd};
use sha2::{Digest, Sha256};
use std::ops::Range;

/// The bundled GitHub-flavored stylesheet, embedded at compile time so the
/// binary needs no external assets at runtime.
const STYLE_CSS: &str = include_str!("../assets/style.css");

/// The bundled viewer client script (zoom + `window.__mdviewViewer`),
/// embedded at compile time. Injected right before [`LIVE_JS`] whenever
/// live-reload is requested — see [`page`].
const VIEWER_JS: &str = include_str!("../assets/viewer.js");

/// The bundled live-reload client script, embedded at compile time. Only
/// injected into the page when live-reload is requested (see [`page`]).
const LIVE_JS: &str = include_str!("../assets/live.js");

/// The bundled review-comments client script, embedded at compile time.
/// Injected alongside [`LIVE_JS`] whenever live-reload is requested — the
/// review pane only makes sense on a live view (`--export`'s static HTML
/// has nowhere to `PUT` comments to).
const REVIEW_JS: &str = include_str!("../assets/review.js");

/// Content-Security-Policy applied to every rendered page. There is no
/// external network access at all (everything is inlined at compile time),
/// so this is deliberately as strict as `default-src 'none'` plus the exact
/// exceptions the page actually needs: inline `<style>`/`<script>` (both
/// embedded, never user-controlled), `connect-src 'self'` for the
/// live-reload script's `fetch('/version')`, and images. `img-src` allows
/// both `http(s):` and `data:`, but only `http(s):` image targets actually
/// reach the page today — `is_safe_link_target` rewrites `data:` (like any
/// other non-allowlisted scheme) to `#` before it ever becomes an `<img
/// src>`. `data:` is included here anyway as defense-in-depth, in case a
/// future change adds another path for `<img>` markup that doesn't go
/// through that sanitizer.
const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; img-src data: http: https:; connect-src 'self'; form-action 'none'";

/// A single top-level Markdown block (paragraph, heading, list, code block,
/// table, blockquote, raw HTML block, thematic break, footnote definition,
/// ...), as identified for review comments. See [`blocks`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// First 16 hex characters of `sha256(source)` — a stable identifier
    /// for this block's content, used to anchor review comments across
    /// re-renders. Two blocks with identical (trimmed) source hash the
    /// same; that's by design (see the review design doc).
    pub hash: String,
    /// The block's Markdown source, trimmed of leading/trailing whitespace.
    pub source: String,
    /// The block's first line, truncated to 80 characters (code fences are
    /// skipped in favor of the first line of actual code content).
    pub excerpt: String,
    /// 1-based line number of the block's first line in the original
    /// Markdown source. Counts the `\n` characters before the block's
    /// (untrimmed) start offset — see [`line_range`].
    pub line_start: usize,
    /// 1-based line number of the block's last line in the original
    /// Markdown source, computed after trimming only trailing whitespace
    /// off the block's source (so a block's own trailing blank lines never
    /// inflate this) — see [`line_range`].
    pub line_end: usize,
}

/// Converts Markdown source into an HTML fragment (no surrounding
/// `<html>`/`<body>` scaffolding — see [`page`] for that).
///
/// Tables, strikethrough, task lists, and footnotes are enabled.
///
/// Two things pulldown-cmark would otherwise hand back unchanged are
/// neutralized before rendering (see [`sanitize_events`] for details): raw
/// HTML embedded in the source, and `javascript:`/`data:`-style link or
/// image targets.
///
/// Every top-level block (as split by [`blocks`]) is wrapped in
/// `<div class="blk" data-hash="..." data-line-start="..."
/// data-line-end="...">...</div>` so the review UI can locate and mark up
/// individual blocks, and label them with their source line range. The
/// sanitization pass runs per block (each block's event slice is
/// self-contained — raw HTML blocks never straddle a block boundary — so
/// this is equivalent to sanitizing the whole document at once).
pub fn to_html(markdown: &str) -> String {
    let mut html_output = String::new();
    for (range, events) in parsed_blocks(markdown) {
        let (line_start, line_end) = line_range(markdown, &range);
        let source = markdown[range].trim();
        let hash = hash_source(source);
        let sanitized = sanitize_events(events);
        // `data-excerpt` lets the review UI (assets/review.js) show a
        // preview for a block that has no saved comment yet, without
        // re-deriving one client-side from the rendered HTML text.
        //
        // Deliberately *not* `excerpt_of(source)` (the same raw-markdown
        // excerpt `blocks()` returns): that string is the literal
        // Markdown source, which for e.g. a link is `[label](url)` —
        // embedding it here would put a raw `javascript:`/`data:` target
        // (or, for a raw-HTML block, an `<!-- comment -->`) into the
        // live page as inert-but-visible attribute text, even though
        // `sanitize_events` just neutralized/discarded exactly that. The
        // excerpt used here is built from `sanitized`'s own `Text`/`Code`
        // content instead, so it can only ever contain what already
        // survived sanitization.
        let excerpt = plain_text_excerpt(&sanitized);
        let mut block_html = String::new();
        html::push_html(&mut block_html, sanitized.into_iter());
        html_output.push_str("<div class=\"blk\" data-hash=\"");
        html_output.push_str(&hash);
        html_output.push_str("\" data-line-start=\"");
        html_output.push_str(&line_start.to_string());
        html_output.push_str("\" data-line-end=\"");
        html_output.push_str(&line_end.to_string());
        html_output.push_str("\" data-excerpt=\"");
        html_output.push_str(&escape_html_text(&excerpt));
        html_output.push_str("\">");
        html_output.push_str(&block_html);
        html_output.push_str("</div>\n");
    }
    html_output
}

/// Builds an 80-character, single-line preview from a sanitized event
/// slice's visible text content only (`Text`/`Code`; nothing else —
/// certainly not a link/image's `dest_url`, which never appears in `Text`
/// anyway). Stops at the first line break of any kind (an embedded `\n`
/// inside a chunk, or a `SoftBreak`/`HardBreak` event) so this is always
/// one line, the same way `excerpt_of` is for `blocks()`.
fn plain_text_excerpt(events: &[Event<'_>]) -> String {
    let mut text = String::new();
    'events: for event in events {
        match event {
            Event::Text(chunk) | Event::Code(chunk) => {
                for ch in chunk.chars() {
                    if ch == '\n' {
                        break 'events;
                    }
                    text.push(ch);
                    if text.chars().count() >= 80 {
                        break 'events;
                    }
                }
            }
            Event::SoftBreak | Event::HardBreak => break 'events,
            _ => {}
        }
    }
    // Leading/trailing whitespace can show up here even though the source
    // block itself is trimmed — e.g. a comment-stripped HTML block leaves
    // behind whatever whitespace surrounded the removed `<!--...-->`.
    text.trim().to_string()
}

/// Splits `markdown` into its top-level blocks, in document order. This is
/// the same split [`to_html`] uses to wrap each block in a `data-hash` div,
/// so `blocks(markdown).len()` and the number of `.blk` divs `to_html`
/// produces always match, in the same order with the same hashes.
pub fn blocks(markdown: &str) -> Vec<Block> {
    parsed_blocks(markdown)
        .into_iter()
        .map(|(range, _events)| {
            let (line_start, line_end) = line_range(markdown, &range);
            let source = markdown[range].trim().to_string();
            let hash = hash_source(&source);
            let excerpt = excerpt_of(&source);
            Block {
                hash,
                source,
                excerpt,
                line_start,
                line_end,
            }
        })
        .collect()
}

/// Computes a block's 1-based `(line_start, line_end)` within `markdown`,
/// given its (untrimmed) byte `range` as reported by
/// [`Parser::into_offset_iter`]. `line_start` counts the `\n` characters
/// before `range.start`, plus one. `line_end` counts the `\n` characters
/// before the position where the block's source ends *after trimming only
/// trailing whitespace* (so a block's own trailing blank lines inside
/// `range` — which [`parsed_blocks`] doesn't otherwise strip — never
/// inflate the reported end line), plus one.
///
/// Counting raw `\n` bytes rather than reasoning about line contents means
/// this gives the same, correct answer whether `markdown` uses `\n` or
/// `\r\n` line endings: a `\r\n` pair still contributes exactly one `\n`
/// per line.
fn line_range(markdown: &str, range: &Range<usize>) -> (usize, usize) {
    let line_start = count_newlines_before(markdown, range.start) + 1;
    let trimmed_end = range.start + markdown[range.clone()].trim_end().len();
    let line_end = count_newlines_before(markdown, trimmed_end) + 1;
    (line_start, line_end)
}

fn count_newlines_before(markdown: &str, byte_pos: usize) -> usize {
    markdown[..byte_pos].bytes().filter(|&b| b == b'\n').count()
}

fn markdown_options() -> Options {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);
    options
}

/// Parses `markdown` and groups its events into top-level blocks: each
/// entry is the block's byte range in `markdown` (untrimmed) paired with
/// its owned event slice, ready to be sanitized/rendered independently.
///
/// A block is either a `Start`/`End` pair at nesting depth 0 (covering
/// every event in between, however deeply nested — a whole list, table,
/// blockquote, etc. is one block) or, at depth 0, a single event that is
/// neither `Start` nor `End` (e.g. `Event::Rule`, or a stray depth-0
/// `Event::Html`).
fn parsed_blocks(markdown: &str) -> Vec<(Range<usize>, Vec<Event<'_>>)> {
    let events: Vec<(Event<'_>, Range<usize>)> = Parser::new_ext(markdown, markdown_options())
        .into_offset_iter()
        .collect();

    let mut result = Vec::new();
    let mut depth: i32 = 0;
    let mut block_start_idx: Option<usize> = None;

    for (i, (event, _)) in events.iter().enumerate() {
        match event {
            Event::Start(_) => {
                if depth == 0 {
                    block_start_idx = Some(i);
                }
                depth += 1;
            }
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    if let Some(start) = block_start_idx.take() {
                        result.push((start, i));
                    }
                }
            }
            _ => {
                if depth == 0 {
                    result.push((i, i));
                }
            }
        }
    }

    result
        .into_iter()
        .map(|(start, end)| {
            let range = events[start].1.start..events[end].1.end;
            let block_events = events[start..=end].iter().map(|(e, _)| e.clone()).collect();
            (range, block_events)
        })
        .collect()
}

/// `sha256(trim(source))`'s first 16 hex characters. `source` is expected
/// to already be trimmed (both [`blocks`] and [`to_html`] trim before
/// calling this), so this never trims itself — it just hex-encodes.
fn hash_source(source: &str) -> String {
    let digest = Sha256::digest(source.as_bytes());
    let mut hex = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// The block's first line, truncated to 80 *characters* (not bytes, so a
/// multi-byte UTF-8 character is never split). If the first line is a code
/// fence (```` ``` ```` or `~~~`, possibly indented), the fence line is
/// skipped in favor of the next line — the first line of actual code
/// content — since the fence itself isn't useful as an excerpt.
fn excerpt_of(trimmed_source: &str) -> String {
    let mut lines = trimmed_source.lines();
    let first = lines.next().unwrap_or("");
    let excerpt_line = if is_code_fence_line(first) {
        lines.next().unwrap_or("")
    } else {
        first
    };
    excerpt_line.chars().take(80).collect()
}

fn is_code_fence_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("```") || trimmed.starts_with("~~~")
}

/// Rewrites a full parse event stream so it's safe to hand to
/// `pulldown_cmark::html::push_html` unchanged. Two kinds of event are
/// touched:
///
/// - Raw HTML (`Html`/`InlineHtml`): pulldown-cmark passes it through
///   verbatim, which would let e.g. a `<script>` block in the source
///   execute in the browser. Its text is run through
///   [`strip_html_comments`] and then re-emitted as a `Text` event, so it
///   goes through the same HTML-escaping path as any other text and
///   renders as literal, inert text — with any `<!--...-->` ranges removed
///   rather than shown.
///
///   Block-level raw HTML needs special handling here: pulldown-cmark
///   splits an HTML block into one `Html` event per source line (and, after
///   CRLF normalization, sometimes more than one event per line), all
///   between a `Start(Tag::HtmlBlock)`/`End(TagEnd::HtmlBlock)` pair. A
///   multi-line comment like `<!--\nsecret\n-->` therefore arrives as three
///   separate `Html` events, and a line like `<!-- c --> visible` arrives
///   as a single event containing both the comment and real content. Either
///   per-event line-splitting or a bare "does this event start with
///   `<!--`" check gets one of those wrong, so instead every `Html` event
///   inside a block is buffered and only stripped/emitted as one `Text`
///   event once the block ends. `InlineHtml` doesn't need this: each event
///   is already exactly one self-contained raw-HTML unit (a tag or a
///   complete comment), never mixed with surrounding content.
/// - Link/image destinations (`Start(Tag::Link { .. })` /
///   `Start(Tag::Image { .. })`): Markdown lets a link or image point at
///   any URL scheme, including `javascript:` and `data:`, both of which are
///   executable/renderable in ways that amount to script injection.
///   [`sanitize_link_target`] replaces anything that isn't on a small
///   allowlist with `#`.
fn sanitize_events(events: Vec<Event<'_>>) -> Vec<Event<'_>> {
    let mut output = Vec::with_capacity(events.len());
    // While `Some`, we're between a `Start(Tag::HtmlBlock)` and its
    // matching `End`, accumulating every `Html` event's text so the whole
    // block can be desanitized as one unit instead of line by line.
    let mut html_block_buffer: Option<String> = None;

    for event in events {
        match event {
            Event::Start(Tag::HtmlBlock) => {
                html_block_buffer = Some(String::new());
                output.push(Event::Start(Tag::HtmlBlock));
            }
            Event::End(TagEnd::HtmlBlock) => {
                if let Some(buffer) = html_block_buffer.take() {
                    push_sanitized_raw_html(&mut output, &buffer);
                }
                output.push(Event::End(TagEnd::HtmlBlock));
            }
            Event::Html(text) => match html_block_buffer.as_mut() {
                Some(buffer) => buffer.push_str(&text),
                // Shouldn't happen in practice — pulldown-cmark only emits
                // `Html` events inside a `HtmlBlock` — but handle it the
                // same way as a one-line block just in case.
                None => push_sanitized_raw_html(&mut output, &text),
            },
            Event::InlineHtml(text) => push_sanitized_raw_html(&mut output, &text),
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }) => output.push(Event::Start(Tag::Link {
                link_type,
                dest_url: sanitize_link_target(dest_url),
                title,
                id,
            })),
            Event::Start(Tag::Image {
                link_type,
                dest_url,
                title,
                id,
            }) => output.push(Event::Start(Tag::Image {
                link_type,
                dest_url: sanitize_link_target(dest_url),
                title,
                id,
            })),
            other => output.push(other),
        }
    }

    output
}

/// Strips HTML comments out of `text` (see [`strip_html_comments`]) and, if
/// anything other than whitespace remains, pushes it as a single `Text`
/// event. If nothing but whitespace remains — the raw HTML was nothing but
/// a comment — pushes nothing at all, rather than a stray blank text node.
fn push_sanitized_raw_html<'a>(output: &mut Vec<Event<'a>>, text: &str) {
    let stripped = strip_html_comments(text);
    if stripped.trim().is_empty() {
        return;
    }
    output.push(Event::Text(CowStr::from(stripped)));
}

/// Removes every `<!--...-->` range from `text`, leaving everything else —
/// including real tags on the same line as a comment — untouched. An
/// unterminated `<!--` (no matching `-->` anywhere in `text`) consumes the
/// rest of `text`, since there's nothing meaningful to keep after a comment
/// that never closes within the content we were given.
fn strip_html_comments(text: &str) -> String {
    const OPEN: &str = "<!--";
    const CLOSE: &str = "-->";

    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        match rest.find(OPEN) {
            None => {
                out.push_str(rest);
                break;
            }
            Some(start) => {
                out.push_str(&rest[..start]);
                let after_open = &rest[start + OPEN.len()..];
                match after_open.find(CLOSE) {
                    Some(end) => rest = &after_open[end + CLOSE.len()..],
                    None => break,
                }
            }
        }
    }
    out
}

/// Replaces `dest_url` with `#` unless [`is_safe_link_target`] accepts it,
/// in which case surrounding whitespace is trimmed off before it's kept.
/// Angle-bracket destinations (`< https://example.com >`) preserve that
/// whitespace verbatim in `dest_url`; left untrimmed, it would reach
/// `escape_href` and come out percent-encoded (`%20`) instead of just
/// disappearing the way it visually looks like it should.
fn sanitize_link_target(dest_url: CowStr<'_>) -> CowStr<'_> {
    if !is_safe_link_target(&dest_url) {
        return CowStr::from("#");
    }
    let trimmed = dest_url.trim();
    if trimmed.len() == dest_url.len() {
        // Nothing to trim — keep the original (often a zero-copy borrow)
        // instead of reallocating.
        dest_url
    } else {
        CowStr::from(trimmed.to_owned())
    }
}

/// A link/image target is safe to keep as-is if it's a same-page fragment
/// (`#...`), a relative reference with no URL scheme at all, or uses one of
/// a small allowlist of schemes (`http`, `https`, `mailto`). Everything
/// else is rejected — `javascript:`, `data:`, `vbscript:`, `file:`, etc.,
/// and also protocol-relative references (`//evil.example`), which inherit
/// whatever scheme the page is loaded over and so are just as capable of
/// pointing at an attacker-controlled host as a fully-qualified URL.
fn is_safe_link_target(dest_url: &str) -> bool {
    let trimmed = dest_url.trim();
    if trimmed.starts_with('#') {
        return true;
    }
    if trimmed.starts_with("//") {
        return false;
    }
    match trimmed.find(':') {
        // No colon at all: a relative path, not an absolute URL.
        None => true,
        Some(colon_idx) => {
            let before_colon = &trimmed[..colon_idx];
            if before_colon.contains(['/', '?', '#']) {
                // A `/`, `?`, or `#` before the first `:` means it isn't
                // introducing a URI scheme (e.g. `a/b:c`, `./x?y:z`) — it's
                // a relative reference that happens to contain a colon.
                return true;
            }
            matches!(
                before_colon.to_ascii_lowercase().as_str(),
                "http" | "https" | "mailto"
            )
        }
    }
}

/// Wraps a rendered body in the full HTML page: doctype, embedded
/// stylesheet, a strict Content-Security-Policy, and — when `live` is
/// `Some(version)` — the embedded live-reload script plus a two-pane
/// layout with the review-comments sidebar (`assets/review.js`).
///
/// `title` is HTML-escaped before being placed in `<title>`. `body_html` is
/// assumed to already be safe HTML (i.e. the output of [`to_html`]) and is
/// not escaped again.
///
/// `live` carries the live-reload baseline version rather than a plain
/// `bool` so the caller (the HTTP server) can fix the baseline to the
/// version in effect *before* it read the file for this response — see
/// `server::respond_with_page` for why that ordering matters. `None` means
/// no live-reload at all (used by `--export`): a single-column page with no
/// scripts and no review sidebar, since there's no server to `PUT`
/// comments to.
pub fn page(title: &str, body_html: &str, live: Option<u64>) -> String {
    let title = escape_html_text(title);
    let body_section = match live {
        Some(version) => format!(
            "<div class=\"layout\">\n\
             <main class=\"markdown-body doc\">\n\
             {body_html}\n\
             </main>\n\
             <div class=\"splitter\" id=\"splitter\"></div>\n\
             <aside class=\"review\" id=\"review\"></aside>\n\
             </div>\n\
             <script>window.__mdviewVersion=\"{version}\";</script>\n\
             <script>\n{VIEWER_JS}\n</script>\n\
             <script>\n{LIVE_JS}\n</script>\n\
             <script>\n{REVIEW_JS}\n</script>"
        ),
        None => format!("<main class=\"markdown-body\">\n{body_html}\n</main>"),
    };

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta http-equiv="Content-Security-Policy" content="{CONTENT_SECURITY_POLICY}">
<title>{title}</title>
<style>{STYLE_CSS}</style>
</head>
<body>
{body_section}
</body>
</html>
"#
    )
}

/// Escapes text for use in an HTML text/attribute context (`&`, `<`, `>`,
/// `"`, `'`). Used only for the page title, which is plain text supplied by
/// the caller (the file name), not Markdown.
fn escape_html_text(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_headings() {
        let html = to_html("# Title\n\n## Subtitle");
        assert!(html.contains("<h1>Title</h1>"));
        assert!(html.contains("<h2>Subtitle</h2>"));
    }

    #[test]
    fn renders_emphasis() {
        let html = to_html("*em* and **strong**");
        assert!(html.contains("<em>em</em>"));
        assert!(html.contains("<strong>strong</strong>"));
    }

    #[test]
    fn renders_code_blocks() {
        let html = to_html("```\nlet x = 1;\n```");
        assert!(html.contains("<pre><code>"));
        assert!(html.contains("let x = 1;"));
    }

    #[test]
    fn renders_tables() {
        let html = to_html("| a | b |\n|---|---|\n| 1 | 2 |\n");
        assert!(html.contains("<table>"));
        assert!(html.contains("<th>a</th>"));
        assert!(html.contains("<td>1</td>"));
    }

    #[test]
    fn renders_strikethrough() {
        let html = to_html("~~gone~~");
        assert!(html.contains("<del>gone</del>"));
    }

    #[test]
    fn renders_task_lists() {
        let html = to_html("- [ ] todo\n- [x] done\n");
        assert!(html.contains(r#"type="checkbox""#));
        assert!(html.contains("checked"));
    }

    #[test]
    fn renders_footnotes() {
        let html = to_html("Body text[^1].\n\n[^1]: A note.\n");
        assert!(html.contains("footnote-reference"));
        assert!(html.contains("footnote-definition"));
    }

    #[test]
    fn escapes_raw_html_instead_of_passing_it_through() {
        let html = to_html("<script>alert(1)</script>");
        assert!(html.contains("&lt;script&gt;"));
        assert!(!html.contains("<script>"));
    }

    #[test]
    fn escapes_inline_raw_html() {
        let html = to_html("before <b>bold</b> after");
        assert!(html.contains("&lt;b&gt;"));
        assert!(!html.contains("<b>"));
    }

    #[test]
    fn discards_html_comments() {
        let html = to_html("<!-- x -->\n\ntext");
        assert!(!html.contains("x -->"));
        assert!(!html.contains("<!--"));
        assert!(html.contains("text"));
    }

    #[test]
    fn discards_inline_html_comments() {
        let html = to_html("before <!-- secret --> after");
        assert!(!html.contains("secret"));
        assert!(!html.contains("<!--"));
    }

    #[test]
    fn discards_multiline_html_comments_entirely() {
        // pulldown-cmark emits one `Html` event per line here
        // (`<!--\n`, `secret\n`, `-->\n`); a naive per-event
        // "starts with `<!--`" check only catches the first of the three
        // and lets `secret` leak through as escaped text.
        let html = to_html("<!--\nsecret\n-->\n\ntext");
        assert!(!html.contains("secret"));
        assert!(!html.contains("<!--"));
        assert!(!html.contains("-->"));
        assert!(html.contains("text"));
    }

    #[test]
    fn keeps_real_content_sharing_a_line_with_a_comment() {
        // The whole line is a single `Html` event here. Dropping any event
        // that merely *starts with* `<!--` would discard "visible" along
        // with the comment.
        let html = to_html("<!-- secretword --> visible\n");
        assert!(!html.contains("secretword"));
        assert!(!html.contains("<!--"));
        assert!(html.contains("visible"));
    }

    #[test]
    fn discards_crlf_html_comments() {
        let html = to_html("<!--\r\nsecret\r\n-->\r\n\r\ntext");
        assert!(!html.contains("secret"));
        assert!(!html.contains("<!--"));
        assert!(html.contains("text"));
    }

    #[test]
    fn renders_ordinary_html_blocks_as_escaped_text() {
        let html = to_html("<div>x</div>\n");
        assert!(html.contains("&lt;div&gt;x&lt;/div&gt;"));
        assert!(!html.contains("<div>x</div>"));
    }

    #[test]
    fn neutralizes_javascript_link_target() {
        let html = to_html("[x](javascript:alert(1))");
        assert!(!html.contains("javascript:"));
        assert!(html.contains("href=\"#\""));
    }

    #[test]
    fn neutralizes_data_link_target() {
        let html = to_html("[y](data:text/html,alert(1))");
        assert!(!html.contains("data:"));
        assert!(html.contains("href=\"#\""));
    }

    #[test]
    fn neutralizes_javascript_reference_link_target() {
        let html = to_html("[z][r]\n\n[r]: JaVaScRiPt:alert(1)\n");
        assert!(!html.to_lowercase().contains("javascript:"));
        assert!(html.contains("href=\"#\""));
    }

    #[test]
    fn neutralizes_javascript_image_target() {
        let html = to_html("![i](javascript:alert(1))");
        assert!(!html.contains("javascript:"));
        assert!(html.contains("src=\"#\""));
    }

    #[test]
    fn neutralizes_link_target_with_surrounding_whitespace_and_mixed_case() {
        let html = to_html("[w]( JAVASCRIPT:x)");
        assert!(!html.to_lowercase().contains("javascript:"));
        assert!(html.contains("href=\"#\""));
    }

    #[test]
    fn neutralizes_protocol_relative_link_target() {
        let html = to_html("[p](//evil.example/x)");
        assert!(!html.contains("evil.example"));
        assert!(html.contains("href=\"#\""));
    }

    #[test]
    fn keeps_allowed_link_schemes_and_relative_paths() {
        assert!(to_html("[a](https://example.com)").contains(r#"href="https://example.com""#));

        let mailto_html = to_html("[b](mailto:a@example.com)");
        assert!(mailto_html.contains("href=\"mailto:"));
        assert!(!mailto_html.contains("href=\"#\""));

        assert!(to_html("[c](#frag)").contains("href=\"#frag\""));
        assert!(to_html("[d](./a.md)").contains(r#"href="./a.md""#));
        assert!(to_html("[e](a/b:c)").contains(r#"href="a/b:c""#));
    }

    #[test]
    fn trims_whitespace_from_an_allowed_link_target() {
        // Angle-bracket reference definitions like `< https://example.com >`
        // preserve the internal leading/trailing spaces in `dest_url`; left
        // untrimmed, `escape_href` would percent-encode them (`%20...`)
        // instead of producing a clean `https://example.com` URL.
        let html = to_html("[r][ref]\n\n[ref]: < https://example.com >\n");
        assert!(html.contains(r#"href="https://example.com""#));
        assert!(!html.contains("%20"));
    }

    #[test]
    fn page_with_live_includes_version_baseline_and_polling() {
        let html = page("Doc", "<p>hi</p>", Some(7));
        assert!(html.contains(r#"__mdviewVersion="7""#));
        assert!(html.contains("/version"));
        assert!(html.contains("<script>"));
    }

    #[test]
    fn live_reload_script_aborts_stalled_fetches() {
        // The embedded live-reload JS should bound each poll with an
        // AbortController-based timeout rather than letting a stalled
        // fetch hold `requestInFlight` (and so the in-flight guard) open
        // forever.
        let html = page("Doc", "<p>hi</p>", Some(1));
        assert!(html.contains("AbortController"));
        assert!(html.contains("signal"));
    }

    #[test]
    fn page_without_live_excludes_version_polling() {
        let html = page("Doc", "<p>hi</p>", None);
        assert!(!html.contains("__mdviewVersion"));
        assert!(!html.contains("/version"));
        assert!(!html.contains("<script>"));
    }

    #[test]
    fn page_with_live_includes_a_two_pane_layout_and_review_script() {
        let html = page("Doc", "<p>hi</p>", Some(1));
        assert!(html.contains(r#"<div class="layout">"#));
        assert!(html.contains(r#"class="markdown-body doc""#));
        assert!(html.contains(r#"<aside class="review" id="review"></aside>"#));
        // review.js content (a distinctive, stable identifier from it).
        assert!(html.contains("__mdviewReview"));
    }

    #[test]
    fn page_with_live_includes_the_pane_splitter() {
        let html = page("Doc", "<p>hi</p>", Some(1));
        assert!(html.contains(r#"class="splitter" id="splitter""#));
        // The splitter must sit between the doc pane and the review aside.
        let splitter_idx = html.find("id=\"splitter\"").expect("splitter present");
        let doc_idx = html.find("markdown-body doc").expect("doc pane present");
        let aside_idx = html.find("<aside").expect("review aside present");
        assert!(doc_idx < splitter_idx && splitter_idx < aside_idx);
    }

    #[test]
    fn page_with_live_embeds_the_viewer_script_before_live_js() {
        let html = page("Doc", "<p>hi</p>", Some(1));
        // viewer.js content (a distinctive, stable identifier from it).
        assert!(html.contains("__mdviewViewer"));
        let viewer_idx = html.find("__mdviewViewer").expect("viewer script present");
        let live_idx = html.find("AbortController").expect("live.js present");
        assert!(
            viewer_idx < live_idx,
            "viewer.js must be embedded before live.js"
        );
    }

    #[test]
    fn page_without_live_has_no_layout_aside_splitter_or_scripts() {
        let html = page("Doc", "<p>hi</p>", None);
        assert!(!html.contains("class=\"layout\""));
        assert!(!html.contains("<aside"));
        // Not a bare `!html.contains("splitter")`: the embedded stylesheet
        // (assets/style.css) always defines `.splitter`'s CSS rules
        // regardless of `live`, so only the *element* itself is telling.
        assert!(!html.contains("id=\"splitter\""));
        assert!(!html.contains("__mdviewReview"));
        assert!(!html.contains("__mdviewViewer"));
    }

    #[test]
    fn page_includes_content_security_policy() {
        let html = page("Doc", "<p>hi</p>", None);
        assert!(html.contains("Content-Security-Policy"));
        assert!(html.contains("default-src 'none'"));
    }

    #[test]
    fn page_escapes_title() {
        let html = page("<script>alert(1)</script>", "<p>hi</p>", None);
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(!html.contains("<title><script>"));
    }

    #[test]
    fn page_embeds_body_unescaped() {
        let html = page("Doc", "<p>hi</p>", None);
        assert!(html.contains("<p>hi</p>"));
    }

    // -- blocks() / to_html() block-splitting ---------------------------

    #[test]
    fn splits_paragraphs_headings_and_rules_into_separate_blocks() {
        let md = "# Title\n\nFirst paragraph.\n\n---\n\nSecond paragraph.\n";
        let blocks = blocks(md);
        assert_eq!(blocks.len(), 4);
        assert_eq!(blocks[0].source, "# Title");
        assert_eq!(blocks[1].source, "First paragraph.");
        assert_eq!(blocks[2].source, "---");
        assert_eq!(blocks[3].source, "Second paragraph.");
    }

    #[test]
    fn a_list_is_a_single_block_regardless_of_item_count() {
        let md = "- one\n- two\n- three\n";
        let blocks = blocks(md);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].source, "- one\n- two\n- three");
    }

    #[test]
    fn a_table_is_a_single_block() {
        let md = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let blocks = blocks(md);
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].source.starts_with("| a | b |"));
    }

    #[test]
    fn a_blockquote_is_a_single_block() {
        let md = "> line one\n> line two\n";
        let blocks = blocks(md);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].source, "> line one\n> line two");
    }

    #[test]
    fn a_fenced_code_block_is_a_single_block() {
        let md = "```rust\nlet x = 1;\nlet y = 2;\n```\n";
        let blocks = blocks(md);
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].source.starts_with("```rust"));
        assert!(blocks[0].source.ends_with("```"));
    }

    #[test]
    fn a_raw_html_block_is_a_single_block() {
        let md = "before\n\n<div>\nraw\n</div>\n\nafter\n";
        let blocks = blocks(md);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].source, "before");
        assert!(blocks[1].source.contains("<div>"));
        assert_eq!(blocks[2].source, "after");
    }

    #[test]
    fn blocks_preserve_document_order() {
        let md = "# H\n\npara\n\n- li\n\n```\ncode\n```\n";
        let blocks = blocks(md);
        assert_eq!(blocks.len(), 4);
        assert_eq!(blocks[0].source, "# H");
        assert_eq!(blocks[1].source, "para");
        assert!(blocks[2].source.starts_with("- li"));
        assert!(blocks[3].source.starts_with("```"));
    }

    #[test]
    fn hash_is_stable_for_identical_input() {
        let md = "# Title\n\nSome text.\n";
        let first = blocks(md);
        let second = blocks(md);
        assert_eq!(first.len(), second.len());
        for (a, b) in first.iter().zip(second.iter()) {
            assert_eq!(a.hash, b.hash);
        }
    }

    #[test]
    fn hash_changes_when_source_changes_by_one_character() {
        let a = blocks("Some text.")[0].hash.clone();
        let b = blocks("Some text!")[0].hash.clone();
        assert_ne!(a, b);
    }

    #[test]
    fn hash_is_16_lowercase_hex_characters() {
        let block = &blocks("# Title\n")[0];
        assert_eq!(block.hash.len(), 16);
        assert!(block.hash.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(block.hash, block.hash.to_lowercase());
    }

    #[test]
    fn identical_blocks_hash_the_same() {
        let blocks = blocks("Same text.\n\nSame text.\n");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].hash, blocks[1].hash);
    }

    #[test]
    fn excerpt_is_the_first_line_truncated_to_80_chars() {
        let long_line = "x".repeat(120);
        let md = format!("{long_line}\n");
        let block = &blocks(&md)[0];
        assert_eq!(block.excerpt.chars().count(), 80);
        assert_eq!(block.excerpt, "x".repeat(80));
    }

    #[test]
    fn heading_excerpt_is_the_heading_line_itself() {
        let block = &blocks("## Design notes\n")[0];
        assert_eq!(block.excerpt, "## Design notes");
    }

    #[test]
    fn code_block_excerpt_skips_the_fence_line() {
        let block = &blocks("```rust\nlet x = 1;\n```\n")[0];
        assert_eq!(block.excerpt, "let x = 1;");
    }

    // -- line_start / line_end -------------------------------------------

    #[test]
    fn line_start_and_line_end_are_computed_per_block() {
        // Line numbers (1-based):
        // 1: # Title
        // 2: (blank)
        // 3: Line 1
        // 4: Line 2
        // 5: Line 3
        // 6: (blank)
        // 7: ```
        // 8: code line
        // 9: ```
        let md = "# Title\n\nLine 1\nLine 2\nLine 3\n\n```\ncode line\n```\n";
        let blocks = blocks(md);
        assert_eq!(blocks.len(), 3);

        assert_eq!(blocks[0].source, "# Title");
        assert_eq!(blocks[0].line_start, 1);
        assert_eq!(blocks[0].line_end, 1);

        assert_eq!(blocks[1].source, "Line 1\nLine 2\nLine 3");
        assert_eq!(blocks[1].line_start, 3);
        assert_eq!(blocks[1].line_end, 5);

        assert!(blocks[2].source.starts_with("```"));
        assert_eq!(blocks[2].line_start, 7);
        assert_eq!(blocks[2].line_end, 9);
    }

    #[test]
    fn line_numbers_are_correct_with_crlf_line_endings() {
        let md = "# Title\r\n\r\nLine 1\r\nLine 2\r\nLine 3\r\n";
        let blocks = blocks(md);
        assert_eq!(blocks.len(), 2);

        assert_eq!(blocks[0].line_start, 1);
        assert_eq!(blocks[0].line_end, 1);

        assert_eq!(blocks[1].line_start, 3);
        assert_eq!(blocks[1].line_end, 5);
    }

    #[test]
    fn to_html_wraps_every_block_in_a_data_hash_div_matching_blocks() {
        let md = "# H\n\npara\n\n- li\n\n```\ncode\n```\n";
        let expected = blocks(md);
        let html = to_html(md);

        let found_hashes: Vec<&str> = html
            .match_indices("data-hash=\"")
            .map(|(idx, _)| {
                let rest = &html[idx + "data-hash=\"".len()..];
                &rest[..rest.find('"').expect("closing quote")]
            })
            .collect();

        assert_eq!(found_hashes.len(), expected.len());
        for (found, block) in found_hashes.iter().zip(expected.iter()) {
            assert_eq!(*found, block.hash);
        }
    }

    #[test]
    fn to_html_still_contains_expected_fragment_content_when_wrapped() {
        let html = to_html("# Hello\n");
        assert!(html.contains("<h1>Hello</h1>"));
        assert!(html.contains("class=\"blk\""));
    }

    #[test]
    fn to_html_includes_data_line_start_and_data_line_end_attributes() {
        let html = to_html("# Title\n\nLine 1\nLine 2\nLine 3\n");
        assert!(html.contains("data-line-start=\"1\" data-line-end=\"1\""));
        assert!(html.contains("data-line-start=\"3\" data-line-end=\"5\""));
    }

    #[test]
    fn to_html_includes_an_escaped_data_excerpt_attribute() {
        let html = to_html("Has <angle> & amp\n");
        // `<angle>` survives sanitize_events as literal, escaped text
        // (see escapes_inline_raw_html); the excerpt built from that text
        // must itself be HTML-escaped before landing in an attribute value.
        assert!(html.contains("data-excerpt=\"Has &lt;angle&gt; &amp; amp\""));
    }

    #[test]
    fn data_excerpt_never_leaks_a_neutralized_javascript_link_target() {
        // A regression check for a real bug caught while adding
        // data-excerpt: an earlier version built the excerpt from the raw
        // Markdown source (the same string `blocks()`/`excerpt_of` use),
        // which for a link is literally `[label](url)` — that would have
        // put the neutralized `javascript:` URL right back into the page,
        // just in a different attribute, undoing sanitize_events' whole
        // point. The excerpt here must come from sanitized text content
        // only.
        let html = to_html("[click me](javascript:alert(1))\n");
        assert!(!html.contains("javascript:"));
        assert!(html.contains("data-excerpt=\"click me\""));
    }

    #[test]
    fn data_excerpt_never_leaks_a_discarded_html_comment() {
        let html = to_html("<!-- secretword --> visible text\n");
        assert!(!html.contains("secretword"));
        assert!(html.contains("data-excerpt=\"visible text\""));
    }

    #[test]
    fn blocks_and_to_html_agree_on_a_document_with_nested_and_adjacent_block_types() {
        // Exercises: a footnote definition, a fenced code block nested
        // inside a list item, a list nested inside a blockquote, and a
        // table immediately followed by a thematic break — all in one
        // document, to catch any depth-tracking edge case that a simpler
        // fixture wouldn't.
        let md = "\
Body text[^1].

[^1]: A footnote definition with some text.

- item one
  ```
  code fence inside a list item
  ```
- item two

> quoted intro
> - quoted item one
> - quoted item two

| a | b |
|---|---|
| 1 | 2 |

---

final paragraph
";
        let expected = blocks(md);

        // Sanity-check the fixture itself: every feature this test means
        // to exercise must actually be present as its own top-level
        // block, or a parser/split change could make the rest of this
        // test vacuously pass.
        assert!(
            expected.iter().any(|b| b.source.starts_with("[^1]:")),
            "no footnote-definition block in {expected:#?}"
        );
        assert!(
            expected
                .iter()
                .any(|b| b.source.starts_with('-') && b.source.contains("```")),
            "no list-with-nested-code-fence block in {expected:#?}"
        );
        assert!(
            expected
                .iter()
                .any(|b| b.source.starts_with('>') && b.source.contains("- quoted")),
            "no blockquote-with-nested-list block in {expected:#?}"
        );
        assert!(
            expected.iter().any(|b| b.source.starts_with('|')),
            "no table block in {expected:#?}"
        );
        assert!(
            expected.iter().any(|b| b.source == "---"),
            "no thematic-break block in {expected:#?}"
        );

        let html = to_html(md);
        let found_hashes: Vec<&str> = html
            .match_indices("data-hash=\"")
            .map(|(idx, _)| {
                let rest = &html[idx + "data-hash=\"".len()..];
                &rest[..rest.find('"').expect("closing quote")]
            })
            .collect();

        assert_eq!(found_hashes.len(), expected.len());
        for (found, block) in found_hashes.iter().zip(expected.iter()) {
            assert_eq!(*found, block.hash);
        }
    }
}

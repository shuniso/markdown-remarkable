//! Markdown -> HTML rendering and the surrounding HTML page template.
//!
//! Everything in this module is a pure function: no I/O, no network. That
//! keeps it trivially unit-testable and keeps `server.rs` a thin adapter
//! that reads a file, calls into here, and writes the response.

use pulldown_cmark::{html, CowStr, Event, Options, Parser, Tag, TagEnd};

/// The bundled GitHub-flavored stylesheet, embedded at compile time so the
/// binary needs no external assets at runtime.
const STYLE_CSS: &str = include_str!("../assets/style.css");

/// The bundled live-reload client script, embedded at compile time. Only
/// injected into the page when live-reload is requested (see [`page`]).
const LIVE_JS: &str = include_str!("../assets/live.js");

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

/// Converts Markdown source into an HTML fragment (no surrounding
/// `<html>`/`<body>` scaffolding — see [`page`] for that).
///
/// Tables, strikethrough, task lists, and footnotes are enabled.
///
/// Two things pulldown-cmark would otherwise hand back unchanged are
/// neutralized before rendering (see [`sanitize_events`] for details): raw
/// HTML embedded in the source, and `javascript:`/`data:`-style link or
/// image targets.
pub fn to_html(markdown: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);

    let events: Vec<Event<'_>> = Parser::new_ext(markdown, options).collect();
    let sanitized = sanitize_events(events);

    let mut html_output = String::new();
    html::push_html(&mut html_output, sanitized.into_iter());
    html_output
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
/// `Some(version)` — the embedded live-reload script.
///
/// `title` is HTML-escaped before being placed in `<title>`. `body_html` is
/// assumed to already be safe HTML (i.e. the output of [`to_html`]) and is
/// not escaped again.
///
/// `live` carries the live-reload baseline version rather than a plain
/// `bool` so the caller (the HTTP server) can fix the baseline to the
/// version in effect *before* it read the file for this response — see
/// `server::respond_with_page` for why that ordering matters. `None` means
/// no live-reload at all (used by `--export`).
pub fn page(title: &str, body_html: &str, live: Option<u64>) -> String {
    let title = escape_html_text(title);
    let live_script = match live {
        Some(version) => format!(
            "<script>window.__mdviewVersion=\"{version}\";</script>\n<script>\n{LIVE_JS}\n</script>"
        ),
        None => String::new(),
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
<main class="markdown-body">
{body_html}
</main>
{live_script}
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
}

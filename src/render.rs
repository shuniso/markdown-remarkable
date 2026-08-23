//! Markdown -> HTML rendering and the surrounding HTML page template.
//!
//! Everything in this module is a pure function: no I/O, no network. That
//! keeps it trivially unit-testable and keeps `server.rs` a thin adapter
//! that reads a file, calls into here, and writes the response.

use pulldown_cmark::{html, Alignment, CowStr, Event, Options, Parser, Tag, TagEnd};
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

/// Which of the three kinds of source range an [`Anchor`] identifies. See
/// the nested-anchors design doc
/// (`docs/superpowers/specs/2026-08-23-nested-anchors-design.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorKind {
    /// A top-level Markdown block — the same granularity as [`Block`].
    Block,
    /// A list item (`Tag::Item`), at any nesting depth.
    Item,
    /// A table row, including the header row (`Tag::TableHead` /
    /// `Tag::TableRow`).
    Row,
}

/// A single review-comment anchor: a top-level block, a list item (any
/// nesting depth), or a table row (including the header row). See
/// [`anchors`]. `Block` is the pre-existing, coarser granularity ([`Block`]
/// itself still exists — [`blocks`] is now a thin filter over this); `Item`
/// and `Row` anchors nest inside a `Block`'s own source range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    pub kind: AnchorKind,
    /// First 16 hex characters of `sha256(source)` — see [`Block::hash`].
    /// Computed by the same function regardless of `kind`, so identical
    /// (trimmed) source hashes the same whether it came from a block, an
    /// item, or a row (by design — see the nested-anchors design doc).
    pub hash: String,
    /// The anchor's Markdown source, trimmed of leading/trailing
    /// whitespace. For `Item`, this includes the list marker (`- `, `1. `,
    /// `- [ ] `, ...) and any nested content (sub-lists, code fences, ...).
    /// For `Row`, this is the row's raw `| ... |` source line.
    pub source: String,
    /// A short, single-line preview. For `Block`, see [`Block::excerpt`].
    /// For `Item`, the first source line with its list marker stripped,
    /// truncated to 80 characters. For `Row`, each cell's trimmed raw text
    /// joined with `" | "`, truncated to 80 characters overall.
    pub excerpt: String,
    /// 1-based line number of the anchor's first source line — see
    /// [`line_range`].
    pub line_start: usize,
    /// 1-based line number of the anchor's last source line — see
    /// [`line_range`].
    pub line_end: usize,
    /// Index into the same [`anchors`] result of this anchor's nearest
    /// enclosing anchor: the containing `Block` for a top-level item/row,
    /// or the containing `Item` for a nested list item (nesting is
    /// transparent through non-anchor wrappers like `BlockQuote`/`List`/
    /// `Table` — see the nested-anchors design doc). `None` only for a
    /// `Block`-kind anchor, which has no parent.
    pub parent: Option<usize>,
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
/// `<div class="blk" data-kind="block" data-hash="..." data-line-start="..."
/// data-line-end="..." data-excerpt="...">...</div>` so the review UI can
/// locate and mark up individual blocks, and label them with their source
/// line range. Within a block, every list item (`<li>`, any nesting depth)
/// and table row (`<tr>`, including the header row) gets the same
/// `class="anchor" data-kind="item"|"row" data-hash="..." data-line-start="..."
/// data-line-end="..." data-excerpt="...">` treatment — see [`anchors`] for
/// the shared range/hash/excerpt logic this and `to_html` both build on,
/// and [`render_events_html`] for how the wrapper tags are interleaved with
/// pulldown-cmark's own rendering.
pub fn to_html(markdown: &str) -> String {
    let mut html_output = String::new();
    for (range, events) in parsed_blocks(markdown) {
        let (line_start, line_end) = line_range(markdown, &range);
        let source = markdown[range].trim();
        let hash = hash_source(source);

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
        // excerpt used here is built from the block's own sanitized
        // `Text`/`Code` content instead, so it can only ever contain what
        // already survived sanitization.
        let raw_events: Vec<Event<'_>> = events.iter().map(|(e, _)| e.clone()).collect();
        let sanitized_for_excerpt = sanitize_events(raw_events);
        let excerpt = plain_text_excerpt(&sanitized_for_excerpt);

        html_output.push_str("<div class=\"blk\" data-kind=\"block\" data-hash=\"");
        html_output.push_str(&hash);
        html_output.push_str("\" data-line-start=\"");
        html_output.push_str(&line_start.to_string());
        html_output.push_str("\" data-line-end=\"");
        html_output.push_str(&line_end.to_string());
        html_output.push_str("\" data-excerpt=\"");
        html_output.push_str(&escape_html_text(&excerpt));
        html_output.push_str("\">");

        // `render_events_html` tracks a table's per-column alignments
        // itself (a stack, pushed on `Start(Tag::Table(_))` and popped on
        // `End(TagEnd::Table)`) as it scans this block's flat event slice,
        // so it notices a table nested at *any* depth — inside a
        // blockquote, a list item, a footnote definition, ... — not only
        // when the table IS the whole block (i.e. `events[0] ==
        // Start(Tag::Table(_))`, the only case an earlier version of this
        // function handled). See its docs for why that state can't just be
        // recovered from a fresh `push_html` call once a row's cells are
        // rendered separately from `Start(Tag::Table(_))`.
        render_events_html(markdown, &events, None, 0, &mut html_output);

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

/// The row-anchor analogue of [`plain_text_excerpt`]: joins each
/// `TableCell`'s own plain-text preview with `" | "`, truncated to 80
/// characters overall. Used for a `tr.anchor`'s `data-excerpt` — `events`
/// is a row's sanitized inner event slice, same rationale as
/// [`plain_text_excerpt`] for why this is built from sanitized content
/// rather than raw Markdown (see [`row_excerpt`] for the raw-Markdown
/// analogue used by [`anchors`]/export).
fn plain_text_excerpt_row(events: &[Event<'_>]) -> String {
    let mut cells: Vec<String> = Vec::new();
    let mut i = 0;
    while i < events.len() {
        if matches!(events[i], Event::Start(Tag::TableCell)) {
            let end = matching_end_index(events, i);
            cells.push(plain_text_excerpt(&events[i + 1..end]));
            i = end + 1;
        } else {
            i += 1;
        }
    }
    let joined = cells.join(" | ");
    joined.chars().take(80).collect()
}

/// Given `events[start_idx]` is a `Start`, returns the index of its
/// matching `End` (tracking nested `Start`/`End` depth in between). Used to
/// find a table cell's extent within an already-sanitized (range-less)
/// event slice — see [`plain_text_excerpt_row`], [`render_row_cells`]. The
/// ranged analogue, used while events still carry source byte ranges, is
/// [`matching_end_index_ranged`].
fn matching_end_index(events: &[Event<'_>], start_idx: usize) -> usize {
    let mut depth = 1;
    let mut j = start_idx + 1;
    while depth > 0 {
        match &events[j] {
            Event::Start(_) => depth += 1,
            Event::End(_) => depth -= 1,
            _ => {}
        }
        if depth > 0 {
            j += 1;
        }
    }
    j
}

/// The ranged-event analogue of [`matching_end_index`], used while events
/// still carry their source byte ranges (before [`sanitize_events`] drops
/// them) — see [`scan_anchor_span`], [`row_excerpt`], [`render_row_cells`].
fn matching_end_index_ranged(events: &[(Event<'_>, Range<usize>)], start_idx: usize) -> usize {
    let mut depth = 1;
    let mut j = start_idx + 1;
    while depth > 0 {
        match &events[j].0 {
            Event::Start(_) => depth += 1,
            Event::End(_) => depth -= 1,
            _ => {}
        }
        if depth > 0 {
            j += 1;
        }
    }
    j
}

/// The item-anchor analogue of [`excerpt_of`]: the item's first source line
/// with its list marker stripped (see [`strip_item_marker`]), truncated to
/// 80 characters.
fn item_excerpt(trimmed_source: &str) -> String {
    let first_line = trimmed_source.lines().next().unwrap_or("");
    strip_item_marker(first_line).chars().take(80).collect()
}

/// Strips a list item's marker from the start of `line`: a bullet (`- `,
/// `* `, `+ `) or an ordered marker (one or more ASCII digits followed by
/// `. ` or `) `), then — if present — a task-list checkbox (`[ ] `, `[x] `,
/// `[X] `). Returns `line` (aside from leading-whitespace trimming)
/// unchanged if it doesn't start with a recognized bullet/ordered marker.
fn strip_item_marker(line: &str) -> &str {
    let trimmed = line.trim_start();
    let after_bullet = ["- ", "* ", "+ "]
        .iter()
        .find_map(|marker| trimmed.strip_prefix(marker))
        .unwrap_or_else(|| {
            // Ordered marker: leading ASCII digits (always 1 byte each, so
            // the char count doubles as the byte offset) then `. `/`) `.
            let digit_count = trimmed.chars().take_while(char::is_ascii_digit).count();
            if digit_count == 0 {
                return trimmed;
            }
            let after_digits = &trimmed[digit_count..];
            after_digits
                .strip_prefix(". ")
                .or_else(|| after_digits.strip_prefix(") "))
                .unwrap_or(trimmed)
        });
    ["[ ] ", "[x] ", "[X] "]
        .iter()
        .find_map(|marker| after_bullet.strip_prefix(marker))
        .unwrap_or(after_bullet)
}

/// The row-anchor analogue of [`excerpt_of`]: each `TableCell`'s raw
/// (trimmed) Markdown text, joined with `" | "` and truncated to 80
/// characters overall. `inner_events` is a row anchor's inner event slice
/// (its `TableCell` children) with byte ranges still attached, as produced
/// by [`parsed_blocks`]/consumed by [`scan_anchor_span`].
fn row_excerpt(markdown: &str, inner_events: &[(Event<'_>, Range<usize>)]) -> String {
    let mut cells: Vec<String> = Vec::new();
    let mut i = 0;
    while i < inner_events.len() {
        if matches!(inner_events[i].0, Event::Start(Tag::TableCell)) {
            let end = matching_end_index_ranged(inner_events, i);
            let cell_range = inner_events[i].1.start..inner_events[end].1.end;
            cells.push(markdown[cell_range].trim().to_string());
            i = end + 1;
        } else {
            i += 1;
        }
    }
    let joined = cells.join(" | ");
    joined.chars().take(80).collect()
}

/// `event` is an `Item`/`TableHead`/`TableRow` `Start` — the three
/// event kinds [`walk_anchors`]/[`render_events_html`] treat as anchor
/// boundaries — or `None` for anything else (including a bare `TableCell`
/// or `Table` boundary, neither of which is its own anchor kind).
fn anchor_kind_of(event: &Event<'_>) -> Option<AnchorKind> {
    match event {
        Event::Start(Tag::Item) => Some(AnchorKind::Item),
        Event::Start(Tag::TableHead) | Event::Start(Tag::TableRow) => Some(AnchorKind::Row),
        _ => None,
    }
}

/// Everything [`walk_anchors`] (for [`anchors`]) and [`render_events_html`]
/// (for [`to_html`]) need once they've found an anchor-kind `Start`'s
/// matching `End`: the computed hash/source/excerpt/line-range, plus
/// `end_idx` so the caller knows where to resume scanning. Both callers
/// build this the same way ([`scan_anchor_span`]), so `anchors()` and the
/// `data-hash`/`data-line-start`/`data-line-end` attributes `to_html` emits
/// can never disagree about a given item/row's identity.
struct AnchorSpan {
    kind: AnchorKind,
    hash: String,
    source: String,
    excerpt: String,
    line_start: usize,
    line_end: usize,
    end_idx: usize,
}

/// Given `events[start_idx]` is `Start(Tag::Item)`, `Start(Tag::TableHead)`,
/// or `Start(Tag::TableRow)` (`kind` says which — see [`anchor_kind_of`]),
/// finds its matching `End` and computes the resulting [`AnchorSpan`].
fn scan_anchor_span(
    markdown: &str,
    events: &[(Event<'_>, Range<usize>)],
    start_idx: usize,
    kind: AnchorKind,
) -> AnchorSpan {
    let end_idx = matching_end_index_ranged(events, start_idx);
    let full_range = events[start_idx].1.start..events[end_idx].1.end;
    let (line_start, line_end) = line_range(markdown, &full_range);
    let source = markdown[full_range].trim().to_string();
    let hash = hash_source(&source);
    let excerpt = match kind {
        AnchorKind::Item => item_excerpt(&source),
        AnchorKind::Row => row_excerpt(markdown, &events[start_idx + 1..end_idx]),
        AnchorKind::Block => unreachable!("scan_anchor_span is only called for Item/Row starts"),
    };
    AnchorSpan {
        kind,
        hash,
        source,
        excerpt,
        line_start,
        line_end,
        end_idx,
    }
}

/// Recursion depth cap for [`walk_anchors`]/[`render_events_html`]: once
/// nesting reaches this many levels, a further-nested `Item`/`TableHead`/
/// `TableRow` is no longer treated as its own anchor — [`anchors`] stops
/// including it (and everything inside it), and `to_html` renders it (and
/// everything inside it) as ordinary leaf content via `push_html` instead
/// of a hand-written `<li>`/`<tr>`. `push_html` itself never recurses on
/// nesting depth at all (it's a flat iterator loop, however deep a list
/// actually goes), so this cap exists only to bound *our own* recursion,
/// not pulldown-cmark's — ordinary documents never come close to it, but a
/// pathological or malicious input (thousands of nested list items)
/// shouldn't be able to blow the stack via `walk_anchors`/
/// `render_events_html` either.
const MAX_ANCHOR_DEPTH: u32 = 64;

/// Recursively finds every `Item`/`TableHead`/`TableRow` anchor within
/// `events` (a block's, or an already-found anchor's inner, event slice),
/// appending each to `anchors` in document order with `parent` pointing
/// back at `parent_idx`, then recursing into its own inner events (at
/// `depth + 1`) with the new anchor as the parent — so a nested list item's
/// parent is its immediately enclosing item, not the top-level block
/// (nesting is transparent through non-anchor wrappers like `BlockQuote`/
/// `List`/`Table` — see the nested-anchors design doc). `depth` starts at 0
/// for a top-level block's own direct children; once it reaches
/// [`MAX_ANCHOR_DEPTH`] this returns immediately without scanning `events`
/// at all, so nothing at or beyond that depth becomes an anchor.
fn walk_anchors(
    markdown: &str,
    events: &[(Event<'_>, Range<usize>)],
    parent_idx: usize,
    depth: u32,
    anchors: &mut Vec<Anchor>,
) {
    if depth >= MAX_ANCHOR_DEPTH {
        return;
    }
    let mut i = 0;
    while i < events.len() {
        let Some(kind) = anchor_kind_of(&events[i].0) else {
            i += 1;
            continue;
        };
        let span = scan_anchor_span(markdown, events, i, kind);
        let idx = anchors.len();
        anchors.push(Anchor {
            kind: span.kind,
            hash: span.hash,
            source: span.source,
            excerpt: span.excerpt,
            line_start: span.line_start,
            line_end: span.line_end,
            parent: Some(parent_idx),
        });
        walk_anchors(
            markdown,
            &events[i + 1..span.end_idx],
            idx,
            depth + 1,
            anchors,
        );
        i = span.end_idx + 1;
    }
}

/// Every review-comment anchor in `markdown`, in document order: each
/// top-level block ([`Block`]/[`blocks`]'s granularity), immediately
/// followed by its nested list items and table rows (any depth, in
/// document order), before moving on to the next block. See the
/// nested-anchors design doc
/// (`docs/superpowers/specs/2026-08-23-nested-anchors-design.md`).
///
/// [`blocks`] is a thin filter over this: `anchors(markdown)` restricted to
/// `kind == AnchorKind::Block`.
pub fn anchors(markdown: &str) -> Vec<Anchor> {
    let mut anchors = Vec::new();
    for (range, events) in parsed_blocks(markdown) {
        let (line_start, line_end) = line_range(markdown, &range);
        let source = markdown[range].trim().to_string();
        let hash = hash_source(&source);
        let excerpt = excerpt_of(&source);
        let idx = anchors.len();
        anchors.push(Anchor {
            kind: AnchorKind::Block,
            hash,
            source,
            excerpt,
            line_start,
            line_end,
            parent: None,
        });
        walk_anchors(markdown, &events, idx, 0, &mut anchors);
    }
    anchors
}

/// Splits `markdown` into its top-level blocks, in document order — the
/// `kind == AnchorKind::Block` subset of [`anchors`], kept as its own
/// narrower type since most callers (review.rs's export, mainly) only ever
/// dealt with block-level granularity before nested item/row anchors
/// existed. `blocks(markdown).len()` and the number of `.blk` divs
/// `to_html` produces always match, in the same order with the same
/// hashes.
pub fn blocks(markdown: &str) -> Vec<Block> {
    anchors(markdown)
        .into_iter()
        .filter(|a| a.kind == AnchorKind::Block)
        .map(|a| Block {
            hash: a.hash,
            source: a.source,
            excerpt: a.excerpt,
            line_start: a.line_start,
            line_end: a.line_end,
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

/// One event paired with its source byte range, as reported by
/// [`Parser::into_offset_iter`]. [`parsed_blocks`] returns a block's whole
/// event slice in this form so [`walk_anchors`]/[`render_events_html`] can
/// locate a nested item/row's extent without re-parsing.
type RangedEvent<'a> = (Event<'a>, Range<usize>);

/// Parses `markdown` and groups its events into top-level blocks: each
/// entry is the block's byte range in `markdown` (untrimmed) paired with
/// its owned [`RangedEvent`] slice.
///
/// A block is either a `Start`/`End` pair at nesting depth 0 (covering
/// every event in between, however deeply nested — a whole list, table,
/// blockquote, etc. is one block) or, at depth 0, a single event that is
/// neither `Start` nor `End` (e.g. `Event::Rule`, or a stray depth-0
/// `Event::Html`).
fn parsed_blocks(markdown: &str) -> Vec<(Range<usize>, Vec<RangedEvent<'_>>)> {
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
            let block_events: Vec<(Event<'_>, Range<usize>)> = events[start..=end].to_vec();
            (range, block_events)
        })
        .collect()
}

/// Renders `events` (a block's, or an anchor's inner, event slice) as HTML
/// into `out`, wrapping every nested `Item`/`TableHead`/`TableRow` in its
/// own anchor tag (`<li>`/`<tr>`, via [`push_anchor_open`]) — see
/// [`to_html`]. Everything else, including a bare `Table`/`TableCell`
/// boundary (neither is itself an anchor), is rendered in maximal
/// contiguous runs ("leaf runs") through the ordinary [`sanitize_events`] +
/// `html::push_html` pipeline — see [`flush_leaf`] for what a fresh
/// `push_html` call per run does and doesn't change about the output.
///
/// `alignments` seeds a small stack of per-table column alignments: pushed
/// on every `Start(Tag::Table(a))` this function's own scan encounters
/// (`a.clone()`) and popped on the matching `End(TagEnd::Table)`, so a
/// row's cells (rendered by [`render_row_cells`], which needs the *current*
/// table's alignments) know the right styling regardless of how deep the
/// table is nested — inside a blockquote, a list item, a footnote
/// definition, even (in principle) another table's cell content. A stack
/// rather than a single `Option` because a recursive call for a nested
/// `Item` starts from whatever alignment state was already current at that
/// point (the parameter), and that same call's own scan may then push
/// further table(s) found inside the item on top of it. This state has to
/// be tracked by hand at all, rather than just letting a fresh `push_html`
/// call recover it from the `Start(Tag::Table(_))` event itself, because a
/// row's cells are rendered via a *separate* `push_html` call from the
/// table's own opening tag (see [`flush_leaf`]) — each such call resets
/// pulldown-cmark's internal head/body and column-alignment tracking, so
/// nothing inside a single fresh call can see a table's alignments unless
/// this function hands them over explicitly.
///
/// `depth` is 0 for a block's own direct children, incrementing by one for
/// every `Item` anchor recursed into; once it reaches [`MAX_ANCHOR_DEPTH`],
/// a further-nested `Item`/`TableHead`/`TableRow` is no longer treated as
/// an anchor at all — it (and everything inside it) becomes ordinary leaf
/// content, rendered by `push_html` like any other run.
fn render_events_html(
    markdown: &str,
    events: &[(Event<'_>, Range<usize>)],
    alignments: Option<&[Alignment]>,
    depth: u32,
    out: &mut String,
) {
    let mut alignment_stack: Vec<Vec<Alignment>> = match alignments {
        Some(a) => vec![a.to_vec()],
        None => Vec::new(),
    };
    let mut i = 0;
    let mut leaf_start = 0;
    while i < events.len() {
        match &events[i].0 {
            Event::Start(Tag::Table(table_alignments)) => {
                alignment_stack.push(table_alignments.clone());
                i += 1;
                continue;
            }
            Event::End(TagEnd::Table) => {
                alignment_stack.pop();
                i += 1;
                continue;
            }
            _ => {}
        }

        let kind = if depth < MAX_ANCHOR_DEPTH {
            anchor_kind_of(&events[i].0)
        } else {
            None
        };
        let Some(kind) = kind else {
            i += 1;
            continue;
        };
        flush_leaf(events, leaf_start, i, out);

        let span = scan_anchor_span(markdown, events, i, kind);
        let inner = &events[i + 1..span.end_idx];
        let inner_sanitized = sanitize_events(inner.iter().map(|(e, _)| e.clone()).collect());
        let data_excerpt = match kind {
            AnchorKind::Item => plain_text_excerpt(&inner_sanitized),
            AnchorKind::Row => plain_text_excerpt_row(&inner_sanitized),
            AnchorKind::Block => unreachable!("anchor_kind_of never returns Block"),
        };
        let is_head = matches!(events[i].0, Event::Start(Tag::TableHead));
        let current_alignments = alignment_stack.last().map(Vec::as_slice);

        match kind {
            AnchorKind::Item => {
                push_anchor_open(out, "li", "item", &span, &data_excerpt);
                render_events_html(markdown, inner, current_alignments, depth + 1, out);
                out.push_str("</li>\n");
            }
            AnchorKind::Row => {
                if is_head {
                    out.push_str("<thead>");
                }
                push_anchor_open(out, "tr", "row", &span, &data_excerpt);
                render_row_cells(inner, current_alignments, is_head, out);
                out.push_str("</tr>\n");
                if is_head {
                    out.push_str("</thead><tbody>\n");
                }
            }
            AnchorKind::Block => unreachable!("anchor_kind_of never returns Block"),
        }

        i = span.end_idx + 1;
        leaf_start = i;
    }
    flush_leaf(events, leaf_start, events.len(), out);
}

/// Sanitizes and renders `events[start..end]` (a maximal run of events
/// containing no anchor-kind `Start` — see [`anchor_kind_of`]) via a single
/// fresh `push_html` call. A no-op if the run is empty (one anchor
/// immediately follows another, or the slice starts/ends right at one).
///
/// A fresh `push_html` call per run — rather than one call over the whole
/// block, as before nested anchors existed — means pulldown-cmark's
/// internal `end_newline` tracking (whether the last thing written ended in
/// `\n`) resets to `true` at the start of each run instead of carrying over
/// from whatever was written just before (e.g. a hand-written `<li>`, which
/// has no trailing newline). The only consequence is a possible
/// extra/missing blank line in the *raw HTML* right at such a boundary
/// (e.g. before a loose list item's `<p>`) — invisible in the rendered
/// page, since whitespace between block-level tags has no visual effect.
/// Nothing here depends on tight-vs-loose list paragraph suppression:
/// pulldown-cmark decides that at parse time (a tight list's items simply
/// never contain `Paragraph` events in the first place), not in
/// `html::push_html`, so that distinction is unaffected by how many pieces
/// the event stream ends up rendered in.
fn flush_leaf(events: &[(Event<'_>, Range<usize>)], start: usize, end: usize, out: &mut String) {
    if start == end {
        return;
    }
    let slice: Vec<Event<'_>> = events[start..end].iter().map(|(e, _)| e.clone()).collect();
    let sanitized = sanitize_events(slice);
    html::push_html(out, sanitized.into_iter());
}

/// Renders a row's `TableCell` children as `<th>`/`<td>` (`is_head` picks
/// which), each with the same column text-align inline style
/// pulldown-cmark's own `html::push_html` would emit (from `alignments`,
/// indexed by cell position) — replicated by hand here since it depends on
/// state (head-vs-body, per-column alignment) that a fresh per-run
/// `push_html` call (see [`flush_leaf`]) can't carry across the row-anchor
/// boundary. Each cell's own inner content is inline-only — no block-level
/// Markdown construct can appear inside a table cell — so *that* part is
/// safe to sanitize/render with an ordinary fresh `push_html` call; nothing
/// inline depends on the state above.
fn render_row_cells(
    events: &[(Event<'_>, Range<usize>)],
    alignments: Option<&[Alignment]>,
    is_head: bool,
    out: &mut String,
) {
    let tag = if is_head { "th" } else { "td" };
    let mut column = 0usize;
    let mut i = 0;
    while i < events.len() {
        if !matches!(events[i].0, Event::Start(Tag::TableCell)) {
            i += 1;
            continue;
        }
        let end = matching_end_index_ranged(events, i);

        out.push('<');
        out.push_str(tag);
        match alignments.and_then(|a| a.get(column)) {
            Some(Alignment::Left) => out.push_str(" style=\"text-align: left\">"),
            Some(Alignment::Center) => out.push_str(" style=\"text-align: center\">"),
            Some(Alignment::Right) => out.push_str(" style=\"text-align: right\">"),
            _ => out.push('>'),
        }
        let inner: Vec<Event<'_>> = events[i + 1..end].iter().map(|(e, _)| e.clone()).collect();
        html::push_html(out, sanitize_events(inner).into_iter());
        out.push_str("</");
        out.push_str(tag);
        out.push('>');

        column += 1;
        i = end + 1;
    }
}

/// Writes an anchor's opening tag: `<{tag} class="anchor"
/// data-kind="{kind_name}" data-hash="..." data-line-start="..."
/// data-line-end="..." data-excerpt="...">`. `excerpt` is HTML-escaped;
/// everything else in `span` is already a hex hash or a decimal line
/// number, so none of it needs escaping.
fn push_anchor_open(
    out: &mut String,
    tag: &str,
    kind_name: &str,
    span: &AnchorSpan,
    excerpt: &str,
) {
    out.push('<');
    out.push_str(tag);
    out.push_str(" class=\"anchor\" data-kind=\"");
    out.push_str(kind_name);
    out.push_str("\" data-hash=\"");
    out.push_str(&span.hash);
    out.push_str("\" data-line-start=\"");
    out.push_str(&span.line_start.to_string());
    out.push_str("\" data-line-end=\"");
    out.push_str(&span.line_end.to_string());
    out.push_str("\" data-excerpt=\"");
    out.push_str(&escape_html_text(excerpt));
    out.push_str("\">");
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

    /// Finds every `data-hash="..."` value that belongs to a *block*-level
    /// `.blk` div specifically (i.e. immediately preceded by
    /// `data-kind="block" `), skipping any nested `data-hash` on an
    /// `li.anchor`/`tr.anchor` — used by the tests below that check
    /// `to_html`'s block-level divs against `blocks()`, which now coexist
    /// with (and would otherwise be outnumbered by) nested item/row anchor
    /// hashes.
    fn block_level_hashes(html: &str) -> Vec<&str> {
        const MARKER: &str = "data-kind=\"block\" data-hash=\"";
        html.match_indices(MARKER)
            .map(|(idx, _)| {
                let rest = &html[idx + MARKER.len()..];
                &rest[..rest.find('"').expect("closing quote")]
            })
            .collect()
    }

    #[test]
    fn to_html_wraps_every_block_in_a_data_hash_div_matching_blocks() {
        let md = "# H\n\npara\n\n- li\n\n```\ncode\n```\n";
        let expected = blocks(md);
        let html = to_html(md);

        let found_hashes = block_level_hashes(&html);

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
        let found_hashes = block_level_hashes(&html);

        assert_eq!(found_hashes.len(), expected.len());
        for (found, block) in found_hashes.iter().zip(expected.iter()) {
            assert_eq!(*found, block.hash);
        }
    }

    // -- anchors(): nested list items and table rows ---------------------

    #[test]
    fn anchors_include_blocks_items_and_rows_in_document_order() {
        let md = "- one\n- two\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
        let anchors = anchors(md);
        let kinds: Vec<AnchorKind> = anchors.iter().map(|a| a.kind).collect();
        assert_eq!(
            kinds,
            vec![
                AnchorKind::Block, // the list
                AnchorKind::Item,  // "one"
                AnchorKind::Item,  // "two"
                AnchorKind::Block, // the table
                AnchorKind::Row,   // header row
                AnchorKind::Row,   // "| 1 | 2 |"
            ]
        );
    }

    #[test]
    fn item_anchors_have_the_enclosing_block_as_parent() {
        let md = "- one\n- two\n";
        let anchors = anchors(md);
        assert_eq!(anchors[0].kind, AnchorKind::Block);
        assert_eq!(anchors[1].parent, Some(0));
        assert_eq!(anchors[2].parent, Some(0));
    }

    #[test]
    fn nested_item_anchors_have_the_immediately_enclosing_item_as_parent() {
        // Three levels deep: a top-level item containing a nested list
        // whose item contains a further-nested list.
        let md = "- outer\n  - middle\n    - inner\n";
        let anchors = anchors(md);
        assert_eq!(anchors.len(), 4); // block, outer, middle, inner
        let block_idx = anchors
            .iter()
            .position(|a| a.kind == AnchorKind::Block)
            .expect("block anchor");
        let outer = anchors
            .iter()
            .position(|a| a.excerpt == "outer")
            .expect("outer item");
        let middle = anchors
            .iter()
            .position(|a| a.excerpt == "middle")
            .expect("middle item");
        let inner = anchors
            .iter()
            .position(|a| a.excerpt == "inner")
            .expect("inner item");
        assert_eq!(anchors[outer].parent, Some(block_idx));
        assert_eq!(anchors[middle].parent, Some(outer));
        assert_eq!(anchors[inner].parent, Some(middle));
    }

    #[test]
    fn row_anchors_have_the_table_block_as_parent() {
        let md = "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n";
        let anchors = anchors(md);
        assert_eq!(anchors[0].kind, AnchorKind::Block);
        for row in &anchors[1..] {
            assert_eq!(row.kind, AnchorKind::Row);
            assert_eq!(row.parent, Some(0));
        }
    }

    #[test]
    fn item_excerpt_strips_bullet_task_and_ordered_markers() {
        let md = "- 三番目の項目\n- [ ] todo item\n- [x] done item\n";
        let found_anchors = anchors(md);
        let items: Vec<&str> = found_anchors
            .iter()
            .filter(|a| a.kind == AnchorKind::Item)
            .map(|a| a.excerpt.as_str())
            .collect();
        assert_eq!(items, vec!["三番目の項目", "todo item", "done item"]);

        let ordered_anchors = anchors("1. first\n2. second\n");
        let ordered_items: Vec<&str> = ordered_anchors
            .iter()
            .filter(|a| a.kind == AnchorKind::Item)
            .map(|a| a.excerpt.as_str())
            .collect();
        assert_eq!(ordered_items, vec!["first", "second"]);
    }

    #[test]
    fn row_excerpt_joins_cells_with_a_pipe_separator() {
        let md = "| 値1 | 値2 | 値3 |\n|---|---|---|\n| a | b | c |\n";
        let anchors = anchors(md);
        let rows: Vec<&str> = anchors
            .iter()
            .filter(|a| a.kind == AnchorKind::Row)
            .map(|a| a.excerpt.as_str())
            .collect();
        assert_eq!(rows, vec!["値1 | 値2 | 値3", "a | b | c"]);
    }

    #[test]
    fn item_and_row_hashes_use_the_same_function_as_block_hashes() {
        // A single-item list's block-level source ("- same text") and its
        // sole item's own source are the identical string, so their hashes
        // — computed by the same hash_source function regardless of kind —
        // must match too.
        let found = anchors("- same text\n");
        let block_hash = &found
            .iter()
            .find(|a| a.kind == AnchorKind::Block)
            .unwrap()
            .hash;
        let item_hash = &found
            .iter()
            .find(|a| a.kind == AnchorKind::Item)
            .unwrap()
            .hash;
        assert_eq!(block_hash, item_hash);
    }

    #[test]
    fn item_line_numbers_are_computed_per_item() {
        let md = "- one\n- two\n- three\n";
        let anchors = anchors(md);
        let items: Vec<&Anchor> = anchors
            .iter()
            .filter(|a| a.kind == AnchorKind::Item)
            .collect();
        assert_eq!(items[0].line_start, 1);
        assert_eq!(items[0].line_end, 1);
        assert_eq!(items[1].line_start, 2);
        assert_eq!(items[2].line_start, 3);
    }

    #[test]
    fn row_line_numbers_skip_the_delimiter_line() {
        let md = "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n";
        let anchors = anchors(md);
        let rows: Vec<&Anchor> = anchors
            .iter()
            .filter(|a| a.kind == AnchorKind::Row)
            .collect();
        assert_eq!(rows[0].line_start, 1); // header
        assert_eq!(rows[1].line_start, 3); // first data row (line 2 is the delimiter)
        assert_eq!(rows[2].line_start, 4);
    }

    #[test]
    fn blocks_is_unaffected_by_nested_item_and_row_anchors() {
        let md = "- one\n- two\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
        let block_list = blocks(md);
        assert_eq!(block_list.len(), 2);
        assert!(block_list[0].source.starts_with("- one"));
        assert!(block_list[1].source.starts_with("| a | b |"));
    }

    // -- to_html(): nested anchor tags ------------------------------------

    #[test]
    fn to_html_wraps_list_items_in_anchor_li_tags() {
        let html = to_html("- one\n- two\n");
        assert!(html.contains(r#"<li class="anchor" data-kind="item""#));
        assert_eq!(html.matches(r#"<li class="anchor""#).count(), 2);
    }

    #[test]
    fn to_html_wraps_table_rows_in_anchor_tr_tags_including_the_header() {
        let html = to_html("| a | b |\n|---|---|\n| 1 | 2 |\n");
        assert!(html.contains(r#"<thead><tr class="anchor" data-kind="row""#));
        assert_eq!(html.matches(r#"<tr class="anchor""#).count(), 2);
        assert!(html.contains("</tr>\n</thead><tbody>\n"));
        assert!(html.contains("</tbody></table>"));
    }

    #[test]
    fn to_html_nested_list_items_each_get_their_own_anchor() {
        let html = to_html("- outer\n  - middle\n    - inner\n");
        assert_eq!(html.matches(r#"data-kind="item""#).count(), 3);
    }

    #[test]
    fn to_html_anchors_and_anchors_agree_on_count_kind_and_order() {
        let md = "\
# Heading

- one
  - nested one
  - nested two
- two

1. first
2. second

- [ ] todo
- [x] done

| a | b |
|---|---|
| 1 | 2 |
| 3 | 4 |

final paragraph
";
        let expected = anchors(md);
        let html = to_html(md);

        let found: Vec<(&str, &str)> = html
            .match_indices("data-kind=\"")
            .map(|(idx, _)| {
                let kind_rest = &html[idx + "data-kind=\"".len()..];
                let kind = &kind_rest[..kind_rest.find('"').expect("closing quote")];
                let hash_marker = "data-hash=\"";
                let hash_idx = kind_rest.find(hash_marker).expect("data-hash follows");
                let hash_rest = &kind_rest[hash_idx + hash_marker.len()..];
                let hash = &hash_rest[..hash_rest.find('"').expect("closing quote")];
                (kind, hash)
            })
            .collect();

        assert_eq!(found.len(), expected.len());
        for ((found_kind, found_hash), anchor) in found.iter().zip(expected.iter()) {
            let expected_kind = match anchor.kind {
                AnchorKind::Block => "block",
                AnchorKind::Item => "item",
                AnchorKind::Row => "row",
            };
            assert_eq!(*found_kind, expected_kind, "anchor: {anchor:#?}");
            assert_eq!(*found_hash, anchor.hash, "anchor: {anchor:#?}");
        }
    }

    #[test]
    fn to_html_table_cells_keep_column_alignment_styling_in_body_rows() {
        // A regression check for the exact bug the design doc warns about:
        // rendering a body row's cells through a *fresh* `push_html` call
        // (rather than replicating pulldown-cmark's own TableCell handling
        // by hand) would default to head state (`<th>` instead of `<td>`)
        // and lose column alignment entirely.
        let html = to_html("| a | b |\n|:--|--:|\n| 1 | 2 |\n");
        assert!(html.contains(r#"<td style="text-align: left">1</td>"#));
        assert!(html.contains(r#"<td style="text-align: right">2</td>"#));
        assert!(!html.contains("<th>1</th>"));
    }

    #[test]
    fn to_html_keeps_table_alignment_for_a_table_nested_inside_a_blockquote() {
        // Regression: to_html() used to only capture column alignments
        // when the table WAS the top-level block (events.first() ==
        // Start(Tag::Table(_))). A table quoted inside a blockquote is a
        // block whose first event is Start(Tag::BlockQuote), so the old
        // code never captured its alignments at all and every cell lost
        // its text-align styling.
        let html = to_html("> | a | b |\n> |:--|--:|\n> | 1 | 2 |\n");
        assert!(
            html.contains(r#"<td style="text-align: left">1</td>"#),
            "{html}"
        );
        assert!(
            html.contains(r#"<td style="text-align: right">2</td>"#),
            "{html}"
        );
    }

    #[test]
    fn to_html_keeps_table_alignment_for_a_table_nested_inside_a_list_item() {
        let html = to_html("- item\n\n  | a | b |\n  |:--|--:|\n  | 1 | 2 |\n");
        assert!(
            html.contains(r#"<td style="text-align: left">1</td>"#),
            "{html}"
        );
        assert!(
            html.contains(r#"<td style="text-align: right">2</td>"#),
            "{html}"
        );
    }

    #[test]
    fn to_html_task_list_checkboxes_still_render_inside_item_anchors() {
        let html = to_html("- [ ] todo\n- [x] done\n");
        assert!(html.contains(r#"type="checkbox""#));
        assert!(html.contains("checked"));
        assert!(html.contains(r#"<li class="anchor" data-kind="item""#));
    }

    #[test]
    fn to_html_item_data_excerpt_strips_the_marker_and_is_escaped() {
        let html = to_html("- Has <angle> item\n");
        assert!(html.contains(r#"data-kind="item""#));
        let marker = "data-excerpt=\"";
        let idx = html.find(marker).expect("a data-excerpt attribute exists");
        // The *first* data-excerpt in the document belongs to the outer
        // .blk div (whole-list excerpt keeps the marker); the item's own
        // data-excerpt is the second one and must have it stripped.
        let second = html[idx + marker.len()..]
            .find(marker)
            .map(|rel| idx + marker.len() + rel)
            .expect("a second data-excerpt attribute exists (the item's)");
        let rest = &html[second + marker.len()..];
        let value = &rest[..rest.find('"').expect("closing quote")];
        assert_eq!(value, "Has &lt;angle&gt; item");
    }

    #[test]
    fn item_data_excerpt_never_leaks_a_neutralized_javascript_link_target() {
        // The item-anchor analogue of
        // data_excerpt_never_leaks_a_neutralized_javascript_link_target:
        // an item's data-excerpt must come from sanitized content, not the
        // raw Markdown source, even though the item is nested inside a
        // list rather than being its own top-level block.
        let html = to_html("- [click me](javascript:alert(1))\n");
        assert!(!html.contains("javascript:"));
        assert!(html.contains(r#"data-kind="item""#));
        assert!(html.contains("data-excerpt=\"click me\""));
    }

    #[test]
    fn item_data_excerpt_never_leaks_a_discarded_html_comment() {
        let html = to_html("- <!-- secretword --> visible text\n");
        assert!(!html.contains("secretword"));
        assert!(html.contains(r#"data-kind="item""#));
        assert!(html.contains("data-excerpt=\"visible text\""));
    }

    #[test]
    fn to_html_row_data_excerpt_joins_cells() {
        let html = to_html("| 値1 | 値2 |\n|---|---|\n| a | b |\n");
        assert!(html.contains(r#"data-kind="row""#));
        assert!(html.contains("data-excerpt=\"値1 | 値2\""));
        assert!(html.contains("data-excerpt=\"a | b\""));
    }

    #[test]
    fn blk_div_carries_data_kind_block() {
        let html = to_html("# Title\n");
        assert!(html.contains(r#"<div class="blk" data-kind="block""#));
    }

    // -- to_html() vs. plain push_html: content parity ---------------------
    //
    // `to_html` renders every block (and every nested item/row inside it)
    // through its own hand-rolled interleaving of `<div>`/`<li>`/`<tr>`
    // wrapper tags and per-run `push_html` calls (see `render_events_html`),
    // rather than a single `push_html` call over the whole document. These
    // tests assert that — once the wrapper markup `to_html` itself adds is
    // stripped back out — the two ways of rendering agree on everything
    // else, for a range of fixtures exercising the constructs most likely
    // to expose a divergence: tight/loose lists, list markers/numbering,
    // task lists, deep nesting, lists/tables inside a blockquote or a
    // footnote definition, and code/blockquote/table content nested inside
    // a list item.

    /// Strips the anchor wrapper markup `to_html` adds — `<div class="blk"
    /// ...>...</div>` around every block, and `class="anchor" data-kind=
    /// "item"|"row" data-hash="..." data-line-start="..." data-line-end=
    /// "..." data-excerpt="..."` on every nested `<li>`/`<tr>` — leaving
    /// bare `<li>`/`<tr>` tags, so what remains can be compared against a
    /// plain `pulldown_cmark::html::push_html` render of the same document.
    /// Manual string scanning rather than a `regex` dependency (not a
    /// project dependency, and this is test-only code operating on a small,
    /// fixed set of attribute patterns `to_html` itself controls the exact
    /// wording of): every attribute value `to_html` emits is HTML-escaped
    /// (`escape_html_text`), so it can never itself contain an unescaped
    /// `>` that would let a naive "find the next `>`" scan stop early.
    fn strip_anchor_wrappers(html: &str) -> String {
        let without_li = replace_tag_open(html, "<li class=\"anchor\"", "<li>");
        let without_tr = replace_tag_open(&without_li, "<tr class=\"anchor\"", "<tr>");
        remove_blk_wrapper_divs(&without_tr)
    }

    /// Removes every `<div class="blk" ...>...</div>\n` wrapper `to_html`
    /// adds around a block, keeping the block's own inner HTML —
    /// deliberately *not* just "delete every literal `<div class=\"blk\"
    /// ...>` open tag, then every literal `</div>\n` close": a block can
    /// legitimately render its own `<div>` (a footnote definition's
    /// `<div class="footnote-definition">`, closed the same way with
    /// `</div>\n`), and that naive approach would delete one of *those*
    /// instead of the wrapper's own matching close once there's more than
    /// one `</div>` in the document. Instead, from each `blk` div's own
    /// opening tag, `<div`/`</div>` nesting depth is tracked forward to
    /// find *that specific* div's matching close.
    fn remove_blk_wrapper_divs(html: &str) -> String {
        let mut out = String::with_capacity(html.len());
        let mut pos = 0;
        while let Some(rel) = html[pos..].find("<div class=\"blk\"") {
            let open_start = pos + rel;
            out.push_str(&html[pos..open_start]);
            let open_tag_end = open_start
                + html[open_start..]
                    .find('>')
                    .expect("blk div has a closing '>'")
                + 1;

            let mut depth = 1i32;
            let mut cursor = open_tag_end;
            let close_start = loop {
                let next_open = html[cursor..].find("<div").map(|i| cursor + i);
                let next_close = html[cursor..].find("</div>").map(|i| cursor + i);
                match (next_open, next_close) {
                    (Some(o), Some(c)) if o < c => {
                        depth += 1;
                        cursor = o + "<div".len();
                    }
                    (_, Some(c)) => {
                        depth -= 1;
                        cursor = c + "</div>".len();
                        if depth == 0 {
                            break c;
                        }
                    }
                    _ => panic!("unbalanced <div> after a blk wrapper in test fixture output"),
                }
            };
            out.push_str(&html[open_tag_end..close_start]);
            pos = close_start + "</div>".len();
            if html[pos..].starts_with('\n') {
                pos += 1; // the `\n` to_html always appends after `</div>`.
            }
        }
        out.push_str(&html[pos..]);
        out
    }

    /// Replaces every opening tag in `html` that starts with the literal
    /// `open_prefix` (up to and including its closing `>`) with
    /// `replacement`, leaving everything else untouched.
    fn replace_tag_open(html: &str, open_prefix: &str, replacement: &str) -> String {
        let mut out = String::with_capacity(html.len());
        let mut rest = html;
        loop {
            match rest.find(open_prefix) {
                None => {
                    out.push_str(rest);
                    break;
                }
                Some(idx) => {
                    out.push_str(&rest[..idx]);
                    let after = &rest[idx..];
                    let close = after.find('>').expect("tag has a closing '>'");
                    out.push_str(replacement);
                    rest = &after[close + 1..];
                }
            }
        }
        out
    }

    /// Drops every whitespace character. `to_html` renders a block's
    /// content via several separate `push_html` calls (one per leaf run —
    /// see `render_events_html`/`flush_leaf`) rather than one call over the
    /// whole block; each such call resets pulldown-cmark's own
    /// `end_newline` tracking, which can add or drop a blank
    /// line/whitespace right at a run boundary without changing the
    /// rendered page at all (whitespace between block-level tags is never
    /// visible). Comparing with every whitespace character removed ignores
    /// exactly that class of difference while still catching any real
    /// (non-whitespace) content mismatch.
    fn strip_all_whitespace(s: &str) -> String {
        s.chars().filter(|c| !c.is_whitespace()).collect()
    }

    /// Asserts that `to_html(markdown)`, with its anchor wrapper markup
    /// stripped, matches a plain `pulldown_cmark::html::push_html` render of
    /// the same `markdown` (same `Options`, same `sanitize_events` pass),
    /// ignoring whitespace-only differences.
    fn assert_to_html_matches_push_html(markdown: &str) {
        let ours_raw = to_html(markdown);
        let ours = strip_all_whitespace(&strip_anchor_wrappers(&ours_raw));

        let events: Vec<Event<'_>> = Parser::new_ext(markdown, markdown_options()).collect();
        let sanitized = sanitize_events(events);
        let mut canonical_raw = String::new();
        html::push_html(&mut canonical_raw, sanitized.into_iter());
        let canonical = strip_all_whitespace(&canonical_raw);

        assert_eq!(
            ours, canonical,
            "\nmarkdown:\n{markdown}\n\nto_html (stripped):\n{ours_raw}\n\npush_html (canonical):\n{canonical_raw}\n"
        );
    }

    #[test]
    fn to_html_matches_push_html_for_a_tight_list() {
        assert_to_html_matches_push_html("- one\n- two\n- three\n");
    }

    #[test]
    fn to_html_matches_push_html_for_a_loose_list() {
        assert_to_html_matches_push_html("- one\n\n- two\n\n- three\n");
    }

    #[test]
    fn to_html_matches_push_html_for_an_ordered_list_starting_at_five() {
        assert_to_html_matches_push_html("5. five\n6. six\n7. seven\n");
    }

    #[test]
    fn to_html_matches_push_html_for_a_task_list() {
        assert_to_html_matches_push_html("- [ ] todo\n- [x] done\n");
    }

    #[test]
    fn to_html_matches_push_html_for_a_three_level_nested_list() {
        assert_to_html_matches_push_html("- outer\n  - middle\n    - inner\n");
    }

    #[test]
    fn to_html_matches_push_html_for_a_list_inside_a_blockquote() {
        assert_to_html_matches_push_html("> - a\n> - b\n");
    }

    #[test]
    fn to_html_matches_push_html_for_a_list_item_containing_a_code_fence_blockquote_and_table() {
        assert_to_html_matches_push_html(
            "- item with code\n  \
             ```\n  \
             code line\n  \
             ```\n\
             - item with quote\n  \
             > quoted text\n\
             - item with table\n\n  \
             | a | b |\n  \
             |---|---|\n  \
             | 1 | 2 |\n",
        );
    }

    #[test]
    fn to_html_matches_push_html_for_table_alignment() {
        assert_to_html_matches_push_html("| a | b | c |\n|:--|:--:|--:|\n| 1 | 2 | 3 |\n");
    }

    #[test]
    fn to_html_matches_push_html_for_a_list_inside_a_footnote_definition() {
        // Exactly one footnote in this fixture (referenced once, defined
        // once): both `to_html`'s per-block/per-leaf-run push_html calls
        // and a single whole-document push_html call assign it the same
        // number ("1") regardless, since pulldown-cmark's footnote
        // numbering starts fresh (an empty `numbers` map) at the start of
        // *any* push_html call and this is the first (and only) footnote
        // either call ever sees — so this fixture doesn't hit the known
        // "footnote numbers reset per block" divergence a fixture with
        // multiple footnotes across blocks would (see README's
        // Limitations).
        assert_to_html_matches_push_html(
            "Body text[^1].\n\n[^1]: A note with a list:\n\n    - point one\n    - point two\n",
        );
    }

    // -- recursion depth cap -----------------------------------------------

    #[test]
    fn deeply_nested_lists_do_not_panic_and_stay_within_the_anchor_depth_cap() {
        // 200 levels of nesting, each indented two spaces further than its
        // parent (matching a "- " marker's width) — well past
        // MAX_ANCHOR_DEPTH. Neither anchors() nor to_html() should panic,
        // and no more than MAX_ANCHOR_DEPTH Item anchors should ever be
        // produced along a single nesting chain.
        let mut md = String::new();
        for level in 0..200 {
            md.push_str(&"  ".repeat(level));
            md.push_str("- level\n");
        }

        let html = to_html(&md);
        assert!(!html.is_empty());
        assert!(html.contains("level"));

        let found = anchors(&md);
        let item_count = found.iter().filter(|a| a.kind == AnchorKind::Item).count();
        assert!(
            item_count as u32 <= MAX_ANCHOR_DEPTH,
            "item_count = {item_count}"
        );
    }
}

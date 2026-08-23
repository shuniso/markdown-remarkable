//! Block-level review comments: the JSON sidecar model, atomic load/save,
//! re-anchoring against a re-rendered document, and the Markdown export
//! format handed to an AI agent (or a human) as a review summary.
//!
//! Everything here is pure/file-I/O only — no HTTP, no UI. `routes.rs`
//! wires this up to `GET/PUT /review` and `POST /export`.

use crate::render::{self, Anchor, AnchorKind};
use crate::util::file_title;
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The only supported sidecar schema version. A sidecar with any other
/// `version` is rejected outright (see [`load`]/[`validate`]) rather than
/// guessed at — forward compatibility is a future problem, not this one's.
const SCHEMA_VERSION: u32 = 1;

/// Maximum size of a single comment's `text`, in bytes. Generous enough for
/// a substantial review note, small enough that a malformed/malicious PUT
/// body can't grow the sidecar without bound.
const MAX_COMMENT_TEXT_BYTES: usize = 64 * 1024;

/// Maximum length (in characters) an excerpt is normalized to by
/// [`validate`]. Bigger than the 80 characters `render::blocks` actually
/// produces, but bounded so a client can't smuggle an arbitrarily long
/// string into what's meant to be a one-line preview.
const MAX_EXCERPT_CHARS: usize = 200;

/// Maximum total number of comments a document may hold —
/// [`ReviewDoc::file_comments`] plus every block's comments, combined.
/// [`MAX_COMMENT_TEXT_BYTES`] alone only bounds a single comment's size,
/// not how many of them a client can pile up in one `PUT /review`.
const MAX_TOTAL_COMMENTS: usize = 10_000;

/// Maximum serialized size, in bytes, of the whole document — checked using
/// the same `serde_json::to_vec_pretty` encoding [`save`] actually writes to
/// disk (not the more compact `to_vec`), so this bound reflects the exact
/// byte count the sidecar file will have, not an underestimate of it. A
/// coarser backstop alongside [`MAX_TOTAL_COMMENTS`]/[`MAX_COMMENT_TEXT_BYTES`]:
/// even within both of those per-field limits, enough blocks and excerpts
/// together could still add up to an unreasonably large sidecar.
const MAX_TOTAL_SIDECAR_BYTES: usize = 8 * 1024 * 1024;

/// The full review sidecar document: every block that has ever had a
/// comment, and those comments. Serialized as-is to `<file>.review.json`
/// (see [`sidecar_path`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewDoc {
    pub version: u32,
    pub file: String,
    pub blocks: Vec<ReviewBlock>,
    /// Comments on the document as a whole, not anchored to any block/item/
    /// row — a total-evaluation note rather than feedback on a specific
    /// span. `#[serde(default)]` so a sidecar written before this field
    /// existed still loads (with an empty `Vec`). Always considered
    /// "anchored" — see [`unanchored`], which never looks at this field.
    #[serde(default)]
    pub file_comments: Vec<Comment>,
}

/// One commented anchor: the anchor identity it was last attached to
/// (`hash`, `excerpt`, `kind` — see `render::Anchor`) and the comments
/// attached to it. An anchor with no comments left is dropped from
/// `blocks` entirely rather than kept around empty. The field is still
/// named `blocks` (not `anchors`) for sidecar-format/API backward
/// compatibility — a `ReviewBlock` now anchors to any [`render::Anchor`]
/// (block, list item, or table row), not just a top-level block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewBlock {
    pub hash: String,
    pub excerpt: String,
    /// Which kind of source range `hash` identifies — `"block"`, `"item"`,
    /// or `"row"`. Defaults to `"block"` when absent, so a sidecar written
    /// before nested item/row anchors existed still loads (every comment
    /// in it was necessarily on a top-level block).
    #[serde(default)]
    pub kind: AnchorKindDto,
    pub comments: Vec<Comment>,
}

/// The wire form of [`render::AnchorKind`] — a separate type (rather than
/// reusing `render::AnchorKind` directly) so the sidecar's serde
/// representation (`"block"`/`"item"`/`"row"`, with a `Default` for
/// backward compatibility) stays a concern of the sidecar format, not of
/// `render`'s own in-memory model. Purely descriptive on the server side:
/// nothing here is validated *against* it, since a `ReviewBlock` is matched
/// to the live document by `hash` alone (see [`unanchored`]) — `kind` only
/// ever needs to already be a valid enum variant, which `serde`
/// deserialization guarantees on its own for any well-formed request body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnchorKindDto {
    #[default]
    Block,
    Item,
    Row,
}

/// A single comment. `id` is generated client-side (`assets/review.js`) in
/// the form `c_` + 16 lowercase hex characters; the server only checks the
/// shape (see [`validate`]), not where it came from — `PUT /review`
/// replaces the whole document in one request, so there's no server-side
/// "create a comment" step that would need to hand back a fresh id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Comment {
    pub id: String,
    pub text: String,
    pub created: String,
    pub updated: String,
}

/// The sidecar path for `md`: `notes.md` -> `notes.md.review.json`, next to
/// the Markdown file itself.
pub fn sidecar_path(md: &Path) -> PathBuf {
    let mut name = md.as_os_str().to_owned();
    name.push(".review.json");
    PathBuf::from(name)
}

/// The export path for `md`: `notes.md` -> `notes.review.md`, next to the
/// Markdown file itself (`file_stem` + `.review.md`, keeping the same
/// parent directory).
pub fn export_path(md: &Path) -> PathBuf {
    let stem = md
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| md.display().to_string());
    md.with_file_name(format!("{stem}.review.md"))
}

/// Loads the review sidecar for `md`. A missing sidecar is not an error —
/// it just means no comments exist yet, and an empty document is returned.
/// A sidecar that exists but fails to parse, or whose `version` isn't
/// [`SCHEMA_VERSION`], is an error: the caller must not silently treat that
/// as "no comments" (that would risk the next save wiping out data the
/// user just can't see right now).
pub fn load(md: &Path) -> Result<ReviewDoc> {
    let path = sidecar_path(md);
    match fs::read(&path) {
        Ok(bytes) => {
            let doc: ReviewDoc =
                serde_json::from_slice(&bytes).context("failed to parse review sidecar as JSON")?;
            if doc.version != SCHEMA_VERSION {
                return Err(anyhow!(
                    "unsupported review sidecar version: {} (expected {SCHEMA_VERSION})",
                    doc.version
                ));
            }
            Ok(doc)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(empty_doc(md)),
        Err(err) => Err(err).context("failed to read review sidecar"),
    }
}

/// An empty review document for `md`: no comments yet.
fn empty_doc(md: &Path) -> ReviewDoc {
    ReviewDoc {
        version: SCHEMA_VERSION,
        file: file_title(md),
        blocks: Vec::new(),
        file_comments: Vec::new(),
    }
}

/// Process-wide counter mixed into every temp file name `atomic_write`
/// creates, so two writes racing in the same process never try to create
/// the same temp path.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Writes `contents` to `path` atomically and safely against a temp-file
/// symlink race: a temp file named `<path>.<pid>.<counter>.tmp` (unique
/// per process and per call) is created with `OpenOptions::create_new`,
/// which fails if anything — a stale leftover, or a symlink an attacker
/// planted at that exact name hoping to get followed — already exists
/// there, rather than opening (and truncating) whatever it points to. Only
/// once that succeeds is `contents` written and the temp file renamed into
/// place. If the write or the rename fails, the temp file this call itself
/// created is removed; a failure from `create_new` never triggers a
/// removal, since in that case this call didn't create anything to clean
/// up (and removing it could delete something it doesn't own, e.g. that
/// planted symlink).
fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let pid = std::process::id();
    let counter = TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(format!(".{pid}.{counter}.tmp"));
    let tmp_path = PathBuf::from(tmp_name);

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp_path)
        .context("failed to create temp file")?;

    // From here on this process created `tmp_path` itself, so it's safe —
    // and correct — to remove it on any failure below.
    if let Err(err) = file
        .write_all(contents)
        .context("failed to write temp file")
    {
        drop(file);
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }
    drop(file);

    if let Err(err) = fs::rename(&tmp_path, path).context("failed to finalize file") {
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }
    Ok(())
}

/// Saves `doc` as the review sidecar for `md` — see [`atomic_write`] for
/// how the write itself is made crash- and race-safe.
pub fn save(md: &Path, doc: &ReviewDoc) -> Result<()> {
    let path = sidecar_path(md);
    let json = serde_json::to_vec_pretty(doc).context("failed to serialize review document")?;
    atomic_write(&path, &json)
}

/// Validates and normalizes `doc` before it's persisted: schema version,
/// block hash shape, per-comment id format (`c_` + 16 lowercase hex) and
/// bounded text size, and excerpt normalization — every block's `excerpt`
/// is forced to a single line (`\n`/`\r` become spaces) and capped at
/// [`MAX_EXCERPT_CHARS`] characters, since a client could otherwise submit
/// something `render::blocks` would never itself produce (arbitrary
/// multi-line or oversized text). Also enforces two whole-document bounds —
/// [`MAX_TOTAL_COMMENTS`] (across `file_comments` and every block combined)
/// and [`MAX_TOTAL_SIDECAR_BYTES`] (the document's own serialized size) —
/// since the per-field limits above don't by themselves bound how large the
/// document as a whole can grow. Doesn't touch the filesystem or
/// `render::blocks` — this is pure structural validation/normalization of
/// the document as submitted.
pub fn validate(doc: &mut ReviewDoc) -> Result<()> {
    if doc.version != SCHEMA_VERSION {
        return Err(anyhow!(
            "unsupported review document version: {} (expected {SCHEMA_VERSION})",
            doc.version
        ));
    }
    for block in &mut doc.blocks {
        if !is_valid_hash(&block.hash) {
            return Err(anyhow!("invalid block hash: {:?}", block.hash));
        }
        normalize_excerpt(&mut block.excerpt);
        validate_comments(&block.comments)?;
    }
    validate_comments(&doc.file_comments)?;

    let total_comments: usize = doc.file_comments.len()
        + doc
            .blocks
            .iter()
            .map(|block| block.comments.len())
            .sum::<usize>();
    if total_comments > MAX_TOTAL_COMMENTS {
        return Err(anyhow!(
            "review document has {total_comments} comments, exceeding the {MAX_TOTAL_COMMENTS} limit"
        ));
    }

    let serialized_len = serde_json::to_vec_pretty(doc)
        .context("failed to serialize review document for size validation")?
        .len();
    if serialized_len > MAX_TOTAL_SIDECAR_BYTES {
        return Err(anyhow!(
            "review document is {serialized_len} bytes, exceeding the {MAX_TOTAL_SIDECAR_BYTES} byte limit"
        ));
    }

    Ok(())
}

/// Shared by [`validate`] for both a block's comments and
/// [`ReviewDoc::file_comments`]: every comment's `id` matches the expected
/// shape and its `text` stays within [`MAX_COMMENT_TEXT_BYTES`].
fn validate_comments(comments: &[Comment]) -> Result<()> {
    for comment in comments {
        if !is_valid_comment_id(&comment.id) {
            return Err(anyhow!("invalid comment id: {:?}", comment.id));
        }
        if comment.text.len() > MAX_COMMENT_TEXT_BYTES {
            return Err(anyhow!(
                "comment text exceeds {MAX_COMMENT_TEXT_BYTES} bytes"
            ));
        }
    }
    Ok(())
}

fn is_valid_hash(hash: &str) -> bool {
    hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `id` must match `^c_[0-9a-f]{16}$`: a `c_` prefix followed by exactly 16
/// lowercase hex characters.
fn is_valid_comment_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix("c_") else {
        return false;
    };
    rest.len() == 16
        && rest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Forces `excerpt` to a single line (`\n`/`\r` become spaces) and caps it
/// at [`MAX_EXCERPT_CHARS`] characters, in place.
fn normalize_excerpt(excerpt: &mut String) {
    let normalized: String = excerpt
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .take(MAX_EXCERPT_CHARS)
        .collect();
    *excerpt = normalized;
}

/// The hashes in `doc` that have no matching entry in `anchors` — i.e.
/// review blocks whose original source is no longer found anywhere in the
/// current document (edited or deleted). `anchors` is `render::anchors`'
/// full result (blocks *and* nested items/rows — see the nested-anchors
/// design doc), so a comment on a list item or table row is correctly
/// recognized as anchored even though it never appears in `render::blocks`.
/// Order follows `doc.blocks`.
pub fn unanchored<'a>(doc: &'a ReviewDoc, anchors: &[Anchor]) -> Vec<&'a str> {
    doc.blocks
        .iter()
        .filter(|review_block| !anchors.iter().any(|a| a.hash == review_block.hash))
        .map(|review_block| review_block.hash.as_str())
        .collect()
}

/// Current UTC time as RFC 3339 with whole seconds (`2026-08-23T01:30:00Z`);
/// sub-second precision is noise in a human-readable export header.
fn now_rfc3339() -> String {
    let now = time::OffsetDateTime::now_utc();
    now.replace_nanosecond(0)
        .unwrap_or(now)
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Renders the review as a standalone Markdown document: a heading, an
/// "Exported: <timestamp> · N comments on M blocks" summary line — or,
/// whenever [`ReviewDoc::file_comments`] is non-empty, "N comments (K on
/// the file, M on B blocks)" instead, where `N = K + M` (with an "(+U
/// unanchored)" suffix appended in either case whenever there are
/// unanchored comments) — followed by a `> (file): <md_name>` section for
/// `file_comments` (only emitted when there is at least one; always first,
/// since it isn't anchored to any position in the document), then one
/// section per commented, anchored anchor — block,
/// list item, or table row — in document order (a block immediately
/// followed by its own nested items/rows, before moving on to the next
/// block — see `render::anchors`): a single quoted line giving the
/// anchor's own source line range and excerpt (`> L12-L18: <excerpt>`, or
/// `> L40: <excerpt>` for a one-line span), followed by its comments as a
/// bullet list, where a comment's second and later lines are indented two
/// spaces as continuation lines rather than starting new top-level
/// bullets. A list item or table row's line additionally carries a
/// `（in list Lstart-Lend）`/`（in table Lstart-Lend）` suffix naming its
/// enclosing block's line range (see [`nested_suffix`]) — a plain block's
/// line does not. The anchor's full source is deliberately *not* quoted —
/// for handing this off to an AI agent, the line range plus a short
/// excerpt is enough to locate it, and quoting the whole thing again would
/// just be noise. An `## Unanchored` section follows for comments whose
/// anchor no longer exists in `markdown` (only emitted when there is at
/// least one), using the same one-line quote format but with `(not found)`
/// in place of a line range, since there's no live anchor to report one
/// for.
///
/// If the same hash occurs more than once in the live document (an
/// exact-duplicate block/item/row), only its first occurrence gets a
/// section — so `M blocks` in the summary always equals the number of
/// sections actually shown, never double-counting a duplicate.
///
/// `now` is the RFC3339 export timestamp; passed in (rather than computed
/// here) so this stays a pure, deterministically-testable function — see
/// [`export`] for the impure wrapper that supplies it.
pub fn export_markdown(md_name: &str, markdown: &str, doc: &ReviewDoc, now: &str) -> String {
    let live_anchors = render::anchors(markdown);
    // Filtered to entries that actually have a comment to show: `unanchored`
    // itself only checks hash membership, so a `ReviewBlock` with an empty
    // `comments` list (shouldn't normally happen — the client drops a block
    // the moment its last comment is deleted, see review.js's
    // `dropIfEmpty` — but nothing here enforces that server-side) would
    // otherwise still open an `## Unanchored` section for a quote line with
    // no comments under it.
    let unanchored_hashes: Vec<&str> = unanchored(doc, &live_anchors)
        .into_iter()
        .filter(|hash| {
            doc.blocks
                .iter()
                .find(|rb| rb.hash == *hash)
                .is_some_and(|rb| !rb.comments.is_empty())
        })
        .collect();

    // One entry per *distinct* hash that's both anchored (present in the
    // live document) and commented, in document order — the first
    // occurrence wins whenever the same hash repeats.
    let mut seen_hashes: HashSet<&str> = HashSet::new();
    let mut sections: Vec<(&Anchor, &ReviewBlock)> = Vec::new();
    for anchor in &live_anchors {
        if !seen_hashes.insert(anchor.hash.as_str()) {
            continue;
        }
        if let Some(review_block) = doc.blocks.iter().find(|rb| rb.hash == anchor.hash) {
            if !review_block.comments.is_empty() {
                sections.push((anchor, review_block));
            }
        }
    }

    let anchored_comments: usize = sections.iter().map(|(_, rb)| rb.comments.len()).sum();
    let anchored_blocks = sections.len();
    let unanchored_comments: usize = unanchored_hashes
        .iter()
        .filter_map(|hash| doc.blocks.iter().find(|rb| rb.hash == *hash))
        .map(|rb| rb.comments.len())
        .sum();
    let file_comment_count = doc.file_comments.len();

    let mut out = String::new();
    out.push_str(&format!("# Review: {md_name}\n\n"));
    if file_comment_count > 0 {
        let total_comments = file_comment_count + anchored_comments;
        out.push_str(&format!(
            "Exported: {now} · {total_comments} comments \
             ({file_comment_count} on the file, {anchored_comments} on {anchored_blocks} blocks)"
        ));
    } else {
        out.push_str(&format!(
            "Exported: {now} · {anchored_comments} comments on {anchored_blocks} blocks"
        ));
    }
    if unanchored_comments > 0 {
        out.push_str(&format!(" (+{unanchored_comments} unanchored)"));
    }
    out.push_str("\n\n");

    if file_comment_count > 0 {
        out.push_str("> (file): ");
        out.push_str(&single_line(md_name));
        out.push_str("\n\n");
        for comment in &doc.file_comments {
            push_comment_bullet(&mut out, "- ", &comment.text);
        }
        out.push('\n');
    }

    for (anchor, review_block) in &sections {
        out.push_str("> ");
        out.push_str(&line_label(anchor.line_start, anchor.line_end));
        out.push_str(": ");
        out.push_str(&single_line(&anchor.excerpt));
        if let Some(suffix) = nested_suffix(&live_anchors, anchor) {
            out.push(' ');
            out.push_str(&suffix);
        }
        out.push_str("\n\n");
        for comment in &review_block.comments {
            push_comment_bullet(&mut out, "- ", &comment.text);
        }
        out.push('\n');
    }

    if !unanchored_hashes.is_empty() {
        out.push_str("## Unanchored\n\n");
        for hash in &unanchored_hashes {
            let Some(review_block) = doc.blocks.iter().find(|rb| rb.hash == *hash) else {
                continue;
            };
            out.push_str("> (not found): ");
            out.push_str(&single_line(&review_block.excerpt));
            out.push_str("\n\n");
            for comment in &review_block.comments {
                push_comment_bullet(&mut out, "- ", &comment.text);
            }
            out.push('\n');
        }
    }

    let trimmed_len = out.trim_end().len();
    out.truncate(trimmed_len);
    out.push('\n');
    out
}

/// For an item/row anchor, the `（in list Lstart-Lend）`/`（in table
/// Lstart-Lend）`/`（in block Lstart-Lend）` suffix naming its enclosing
/// block's line range — `None` for a block-kind anchor, which gets no
/// suffix (unchanged format). Walks `anchor.parent` up through `anchors`
/// until it reaches the block-kind ancestor: for a nested list item this is
/// *not* its immediately enclosing item, but the top-level block (the whole
/// list) — deliberate, per the nested-anchors design doc: a nested item's
/// own position is already pinpointed by its own line range in the quote
/// line itself, so the suffix's job is just to say which block to look in,
/// and "the whole list" locates that reliably regardless of nesting depth.
///
/// The label word itself is decided by the *ancestor block's own source*
/// (see [`block_kind_label`]), not by `anchor.kind` — an item nested inside
/// a blockquote (`> - quoted item`) has `anchor.kind == Item`, but its
/// enclosing top-level block is a blockquote, not literally a list, so its
/// suffix reads `（in block …）` rather than the misleading `（in list …）`.
fn nested_suffix(anchors: &[Anchor], anchor: &Anchor) -> Option<String> {
    if anchor.kind == AnchorKind::Block {
        return None;
    }
    let mut ancestor = anchor;
    while ancestor.kind != AnchorKind::Block {
        ancestor = anchors.get(ancestor.parent?)?;
    }
    let label = block_kind_label(&ancestor.source);
    Some(format!(
        "（in {label} {}）",
        line_label(ancestor.line_start, ancestor.line_end)
    ))
}

/// The `nested_suffix` label word for a top-level block, decided by its own
/// (trimmed) `source`'s first characters: `"table"` if it starts with `|`,
/// `"list"` if it starts with a bullet (`- `/`* `/`+ `) or an ordered marker
/// (digits followed by `. `/`) `), `"block"` otherwise — e.g. a blockquote
/// that happens to wrap a list or table, where the item/row's *own* kind
/// doesn't reflect what the enclosing top-level block literally is.
fn block_kind_label(source: &str) -> &'static str {
    let trimmed = source.trim_start();
    if trimmed.starts_with('|') {
        return "table";
    }
    if ["- ", "* ", "+ "].iter().any(|m| trimmed.starts_with(m)) {
        return "list";
    }
    let digit_count = trimmed.chars().take_while(char::is_ascii_digit).count();
    if digit_count > 0 {
        let after_digits = &trimmed[digit_count..];
        if after_digits.starts_with(". ") || after_digits.starts_with(") ") {
            return "list";
        }
    }
    "block"
}

/// The `Lstart-Lend`/`Lstart` label used in an exported block's quote line:
/// a range (`L12-L18`) when the block spans more than one source line, or a
/// single line number (`L40`) when `start == end`.
fn line_label(start: usize, end: usize) -> String {
    if start == end {
        format!("L{start}")
    } else {
        format!("L{start}-L{end}")
    }
}

/// Replaces any `\n`/`\r` in `s` with a space, so it's safe to place inside
/// a single Markdown heading/bullet line. `render::Block::excerpt` and a
/// validated `ReviewBlock::excerpt` are already single-line by
/// construction (see [`normalize_excerpt`]), but this stays defensive
/// against a sidecar written before that normalization existed.
fn single_line(s: &str) -> String {
    s.chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect()
}

/// Appends one comment as a Markdown bullet: `prefix` followed by the
/// comment's first line, with every subsequent line indented two spaces as
/// a continuation of the same list item (rather than starting a new
/// top-level line, which would visually detach it from the bullet).
fn push_comment_bullet(out: &mut String, prefix: &str, text: &str) {
    let mut lines = text.lines();
    out.push_str(prefix);
    out.push_str(lines.next().unwrap_or(""));
    out.push('\n');
    for line in lines {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
    }
}

/// Writes the export Markdown for `md` (given its already-read
/// `markdown` content and loaded review `doc`) to [`export_path`], and
/// returns `(path, markdown)` — the file name-only path plus the exact
/// text written, so the caller (an HTTP handler) can hand both back to the
/// client without a second disk read.
pub fn export(md: &Path, markdown: &str, doc: &ReviewDoc) -> Result<(PathBuf, String)> {
    let path = export_path(md);
    let md_name = file_title(md);
    let now = now_rfc3339();
    let rendered = export_markdown(&md_name, markdown, doc, &now);
    atomic_write(&path, rendered.as_bytes())?;
    Ok((path, rendered))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(id: &str, text: &str) -> Comment {
        Comment {
            id: id.to_string(),
            text: text.to_string(),
            created: "2026-08-22T07:00:00Z".to_string(),
            updated: "2026-08-22T07:00:00Z".to_string(),
        }
    }

    // -- sidecar paths ----------------------------------------------------

    #[test]
    fn sidecar_path_appends_review_json_to_the_full_file_name() {
        let path = sidecar_path(Path::new("/tmp/notes.md"));
        assert_eq!(path, Path::new("/tmp/notes.md.review.json"));
    }

    #[test]
    fn export_path_replaces_the_extension_with_review_md() {
        let path = export_path(Path::new("/tmp/notes.md"));
        assert_eq!(path, Path::new("/tmp/notes.review.md"));
    }

    // -- load / save round-trip -------------------------------------------

    #[test]
    fn loading_a_missing_sidecar_returns_an_empty_document() {
        let dir = tempfile::tempdir().expect("tempdir");
        let md = dir.path().join("notes.md");
        std::fs::write(&md, "# Hi\n").expect("write md");

        let doc = load(&md).expect("load");
        assert_eq!(doc.version, 1);
        assert_eq!(doc.file, "notes.md");
        assert!(doc.blocks.is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let md = dir.path().join("notes.md");
        std::fs::write(&md, "# Hi\n").expect("write md");

        let doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![ReviewBlock {
                hash: "3f9a1c00deadbeef".to_string(),
                excerpt: "# Hi".to_string(),
                kind: AnchorKindDto::Block,
                comments: vec![comment("c_1", "looks good")],
            }],
            file_comments: Vec::new(),
        };

        save(&md, &doc).expect("save");
        let loaded = load(&md).expect("load");
        assert_eq!(loaded, doc);
    }

    #[test]
    fn save_then_load_round_trips_file_comments() {
        let dir = tempfile::tempdir().expect("tempdir");
        let md = dir.path().join("notes.md");
        std::fs::write(&md, "# Hi\n").expect("write md");

        let doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![],
            file_comments: vec![comment("c_1", "全体として章立てが前後している")],
        };

        save(&md, &doc).expect("save");
        let loaded = load(&md).expect("load");
        assert_eq!(loaded, doc);
    }

    #[test]
    fn save_does_not_leave_a_tmp_file_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let md = dir.path().join("notes.md");
        std::fs::write(&md, "# Hi\n").expect("write md");

        let doc = empty_doc(&md);
        save(&md, &doc).expect("save");

        let sidecar = sidecar_path(&md);
        assert!(sidecar.exists());

        let leftover_tmp_files: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().extension().and_then(|e| e.to_str()) == Some("tmp"))
            .collect();
        assert!(
            leftover_tmp_files.is_empty(),
            "leftover tmp files: {leftover_tmp_files:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn save_does_not_follow_a_symlink_placed_at_a_tmp_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let md = dir.path().join("notes.md");
        std::fs::write(&md, "# Hi\n").expect("write md");
        let sidecar = sidecar_path(&md);

        // A file that must never be written to, even if `save`'s
        // internally chosen temp-file name happens to collide with a
        // symlink pointing at it (a classic tmp-file symlink race). The
        // exact pid/counter `save` will pick isn't observable from here
        // (the counter is a process-wide static shared with every other
        // test in this binary that calls `save`/`export`), so a generous
        // range of plausible counter values is pre-seeded with symlinks
        // instead of guessing one exact name.
        let victim = dir.path().join("victim.txt");
        std::fs::write(&victim, "original").expect("write victim");

        let pid = std::process::id();
        let mut symlinks = Vec::new();
        for counter in 0..2000u64 {
            let mut tmp_name = sidecar.as_os_str().to_owned();
            tmp_name.push(format!(".{pid}.{counter}.tmp"));
            let tmp_path = PathBuf::from(tmp_name);
            if std::os::unix::fs::symlink(&victim, &tmp_path).is_ok() {
                symlinks.push(tmp_path);
            }
        }

        let doc = empty_doc(&md);
        // May succeed (if `save`'s chosen counter missed every pre-placed
        // symlink) or fail (if `create_new` correctly refused one of
        // them) — either way, the victim file must be untouched.
        let _ = save(&md, &doc);

        assert_eq!(
            std::fs::read_to_string(&victim).expect("read victim"),
            "original",
            "victim file was overwritten through a symlinked tmp path"
        );

        for symlink in symlinks {
            let _ = std::fs::remove_file(symlink);
        }
    }

    #[test]
    fn loading_a_sidecar_with_the_wrong_version_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let md = dir.path().join("notes.md");
        std::fs::write(&md, "# Hi\n").expect("write md");
        std::fs::write(
            sidecar_path(&md),
            r#"{"version":2,"file":"notes.md","blocks":[]}"#,
        )
        .expect("write sidecar");

        let result = load(&md);
        assert!(result.is_err());
    }

    #[test]
    fn loading_a_sidecar_written_before_kind_existed_defaults_to_block() {
        // A sidecar written by an older version of mdview (before nested
        // item/row anchors existed) has no "kind" field on its blocks at
        // all — `#[serde(default)]` on `ReviewBlock::kind` must fill that
        // in as `AnchorKindDto::Block`, not fail to parse.
        let dir = tempfile::tempdir().expect("tempdir");
        let md = dir.path().join("notes.md");
        std::fs::write(&md, "# Hi\n").expect("write md");
        std::fs::write(
            sidecar_path(&md),
            r#"{"version":1,"file":"notes.md","blocks":[{"hash":"0123456789abcdef","excerpt":"x","comments":[]}]}"#,
        )
        .expect("write sidecar");

        let doc = load(&md).expect("load");
        assert_eq!(doc.blocks.len(), 1);
        assert_eq!(doc.blocks[0].kind, AnchorKindDto::Block);
    }

    #[test]
    fn loading_a_sidecar_written_before_file_comments_existed_defaults_to_empty() {
        // A sidecar written before file-wide comments existed has no
        // "file_comments" field at all — `#[serde(default)]` on
        // `ReviewDoc::file_comments` must fill that in as an empty `Vec`,
        // not fail to parse.
        let dir = tempfile::tempdir().expect("tempdir");
        let md = dir.path().join("notes.md");
        std::fs::write(&md, "# Hi\n").expect("write md");
        std::fs::write(
            sidecar_path(&md),
            r#"{"version":1,"file":"notes.md","blocks":[]}"#,
        )
        .expect("write sidecar");

        let doc = load(&md).expect("load");
        assert!(doc.file_comments.is_empty());
    }

    #[test]
    fn loading_malformed_json_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let md = dir.path().join("notes.md");
        std::fs::write(&md, "# Hi\n").expect("write md");
        std::fs::write(sidecar_path(&md), "{not valid json").expect("write sidecar");

        let result = load(&md);
        assert!(result.is_err());
    }

    // -- validate -----------------------------------------------------------

    #[test]
    fn validate_accepts_a_well_formed_document() {
        let mut doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![ReviewBlock {
                hash: "0123456789abcdef".to_string(),
                excerpt: "x".to_string(),
                kind: AnchorKindDto::Block,
                comments: vec![comment("c_0123456789abcdef", "ok")],
            }],
            file_comments: Vec::new(),
        };
        assert!(validate(&mut doc).is_ok());
    }

    #[test]
    fn validate_rejects_wrong_version() {
        let mut doc = ReviewDoc {
            version: 2,
            file: "notes.md".to_string(),
            blocks: vec![],
            file_comments: Vec::new(),
        };
        assert!(validate(&mut doc).is_err());
    }

    #[test]
    fn validate_rejects_a_hash_that_is_not_16_hex_chars() {
        let mut doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![ReviewBlock {
                hash: "not-hex".to_string(),
                excerpt: "x".to_string(),
                kind: AnchorKindDto::Block,
                comments: vec![],
            }],
            file_comments: Vec::new(),
        };
        assert!(validate(&mut doc).is_err());
    }

    #[test]
    fn validate_rejects_an_empty_comment_id() {
        let mut doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![ReviewBlock {
                hash: "0123456789abcdef".to_string(),
                excerpt: "x".to_string(),
                kind: AnchorKindDto::Block,
                comments: vec![comment("", "ok")],
            }],
            file_comments: Vec::new(),
        };
        assert!(validate(&mut doc).is_err());
    }

    #[test]
    fn validate_rejects_a_comment_id_not_matching_the_expected_format() {
        let mut doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![ReviewBlock {
                hash: "0123456789abcdef".to_string(),
                excerpt: "x".to_string(),
                kind: AnchorKindDto::Block,
                comments: vec![comment("not-the-right-format", "ok")],
            }],
            file_comments: Vec::new(),
        };
        assert!(validate(&mut doc).is_err());
    }

    #[test]
    fn validate_rejects_uppercase_hex_in_a_comment_id() {
        let mut doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![ReviewBlock {
                hash: "0123456789abcdef".to_string(),
                excerpt: "x".to_string(),
                kind: AnchorKindDto::Block,
                comments: vec![comment("c_0123456789ABCDEF", "ok")],
            }],
            file_comments: Vec::new(),
        };
        assert!(validate(&mut doc).is_err());
    }

    #[test]
    fn validate_accepts_a_well_formed_comment_id() {
        let mut doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![ReviewBlock {
                hash: "0123456789abcdef".to_string(),
                excerpt: "x".to_string(),
                kind: AnchorKindDto::Block,
                comments: vec![comment("c_0123456789abcdef", "ok")],
            }],
            file_comments: Vec::new(),
        };
        assert!(validate(&mut doc).is_ok());
    }

    #[test]
    fn validate_rejects_comment_text_over_64_kib() {
        let mut doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![ReviewBlock {
                hash: "0123456789abcdef".to_string(),
                excerpt: "x".to_string(),
                kind: AnchorKindDto::Block,
                comments: vec![comment("c_0123456789abcdef", &"x".repeat(64 * 1024 + 1))],
            }],
            file_comments: Vec::new(),
        };
        assert!(validate(&mut doc).is_err());
    }

    #[test]
    fn validate_accepts_comment_text_at_exactly_64_kib() {
        let mut doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![ReviewBlock {
                hash: "0123456789abcdef".to_string(),
                excerpt: "x".to_string(),
                kind: AnchorKindDto::Block,
                comments: vec![comment("c_0123456789abcdef", &"x".repeat(64 * 1024))],
            }],
            file_comments: Vec::new(),
        };
        assert!(validate(&mut doc).is_ok());
    }

    #[test]
    fn validate_normalizes_excerpt_newlines_to_spaces_and_caps_at_200_chars() {
        let long_excerpt = format!("first\nsecond\r\nthird {}", "x".repeat(250));
        let mut doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![ReviewBlock {
                hash: "0123456789abcdef".to_string(),
                excerpt: long_excerpt,
                kind: AnchorKindDto::Block,
                comments: vec![],
            }],
            file_comments: Vec::new(),
        };
        validate(&mut doc).expect("validate");
        let excerpt = &doc.blocks[0].excerpt;
        assert!(!excerpt.contains('\n'));
        assert!(!excerpt.contains('\r'));
        assert_eq!(excerpt.chars().count(), 200);
        // Each `\r` and `\n` becomes its own space, so the `\r\n` between
        // "second" and "third" becomes two spaces, not one.
        assert!(excerpt.starts_with("first second  third "));
    }

    #[test]
    fn validate_accepts_well_formed_file_comments() {
        let mut doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![],
            file_comments: vec![comment("c_0123456789abcdef", "ok")],
        };
        assert!(validate(&mut doc).is_ok());
    }

    #[test]
    fn validate_rejects_a_malformed_file_comment_id() {
        let mut doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![],
            file_comments: vec![comment("not-the-right-format", "ok")],
        };
        assert!(validate(&mut doc).is_err());
    }

    #[test]
    fn validate_rejects_file_comment_text_over_64_kib() {
        let mut doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![],
            file_comments: vec![comment("c_0123456789abcdef", &"x".repeat(64 * 1024 + 1))],
        };
        assert!(validate(&mut doc).is_err());
    }

    #[test]
    fn validate_rejects_more_than_10_000_comments_combined_across_file_and_blocks() {
        // The 10,000 cap counts file_comments and every block's comments
        // together, not either pool on its own — half the comments live on
        // a block here, half on the file, and the combined total is what
        // must be rejected.
        let file_comments: Vec<Comment> = (0..5001)
            .map(|i| comment(&format!("c_{i:016x}"), "x"))
            .collect();
        let block_comments: Vec<Comment> = (5001..10002)
            .map(|i| comment(&format!("c_{i:016x}"), "x"))
            .collect();
        let mut doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![ReviewBlock {
                hash: "0123456789abcdef".to_string(),
                excerpt: "x".to_string(),
                kind: AnchorKindDto::Block,
                comments: block_comments,
            }],
            file_comments,
        };
        assert!(validate(&mut doc).is_err());
    }

    #[test]
    fn validate_accepts_exactly_10_000_total_comments() {
        let file_comments: Vec<Comment> = (0..10_000)
            .map(|i| comment(&format!("c_{i:016x}"), "x"))
            .collect();
        let mut doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![],
            file_comments,
        };
        assert!(validate(&mut doc).is_ok());
    }

    #[test]
    fn validate_rejects_a_document_whose_serialized_size_exceeds_8_mib() {
        // Each comment stays within the per-comment 64 KiB text cap, and
        // the total comment count (200) stays well under the 10,000 cap —
        // only the whole-document byte size is what pushes this over.
        let file_comments: Vec<Comment> = (0..200)
            .map(|i| comment(&format!("c_{i:016x}"), &"x".repeat(64 * 1024)))
            .collect();
        let mut doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![],
            file_comments,
        };
        assert!(validate(&mut doc).is_err());
    }

    // -- unanchored -----------------------------------------------------------

    #[test]
    fn unanchored_finds_hashes_missing_from_the_current_blocks() {
        let doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![
                ReviewBlock {
                    hash: "aaaaaaaaaaaaaaaa".to_string(),
                    excerpt: "a".to_string(),
                    kind: AnchorKindDto::Block,
                    comments: vec![comment("c_1", "x")],
                },
                ReviewBlock {
                    hash: "bbbbbbbbbbbbbbbb".to_string(),
                    excerpt: "b".to_string(),
                    kind: AnchorKindDto::Block,
                    comments: vec![comment("c_2", "y")],
                },
            ],
            file_comments: Vec::new(),
        };
        let live = render::anchors("some text that hashes to something else\n");
        let result = unanchored(&doc, &live);
        assert_eq!(result.len(), 2);
        assert!(result.contains(&"aaaaaaaaaaaaaaaa"));
        assert!(result.contains(&"bbbbbbbbbbbbbbbb"));
    }

    #[test]
    fn unanchored_is_empty_when_every_hash_matches() {
        let live = render::anchors("# Title\n\nbody text\n");
        let doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: live
                .iter()
                .map(|b| ReviewBlock {
                    hash: b.hash.clone(),
                    excerpt: b.excerpt.clone(),
                    kind: AnchorKindDto::Block,
                    comments: vec![comment("c_1", "ok")],
                })
                .collect(),
            file_comments: Vec::new(),
        };
        assert!(unanchored(&doc, &live).is_empty());
    }

    // -- export_markdown -----------------------------------------------------------

    #[test]
    fn export_markdown_of_an_empty_document_has_only_the_header() {
        let doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![],
            file_comments: Vec::new(),
        };
        let out = export_markdown("notes.md", "# Hi\n", &doc, "2026-08-22T07:10:00Z");
        assert_eq!(
            out,
            "# Review: notes.md\n\nExported: 2026-08-22T07:10:00Z · 0 comments on 0 blocks\n"
        );
    }

    #[test]
    fn export_markdown_matches_the_expected_layout() {
        let markdown = "## 設計方針\n\nNext paragraph with more than eighty characters so the excerpt truncation actually applies to it here.\n";
        let live = render::blocks(markdown);
        let heading_hash = live[0].hash.clone();
        let para_hash = live[1].hash.clone();

        let doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![
                ReviewBlock {
                    hash: heading_hash,
                    excerpt: "## 設計方針".to_string(),
                    kind: AnchorKindDto::Block,
                    comments: vec![
                        comment("c_1", "ここは根拠が弱い"),
                        comment("c_2", "代替案も書く"),
                    ],
                },
                ReviewBlock {
                    hash: para_hash,
                    excerpt: live[1].excerpt.clone(),
                    kind: AnchorKindDto::Block,
                    comments: vec![comment("c_3", "…")],
                },
            ],
            file_comments: Vec::new(),
        };

        let out = export_markdown("notes.md", markdown, &doc, "2026-08-22T07:10:00Z");

        let expected = format!(
            "# Review: notes.md\n\n\
             Exported: 2026-08-22T07:10:00Z · 3 comments on 2 blocks\n\n\
             > L1: ## 設計方針\n\n\
             - ここは根拠が弱い\n\
             - 代替案も書く\n\n\
             > L3: {}\n\n\
             - …\n",
            live[1].excerpt
        );
        assert_eq!(out, expected);
    }

    #[test]
    fn export_markdown_puts_file_comments_first_with_a_file_section_and_combined_count() {
        let markdown = "## 設計方針\n\nBody paragraph.\n";
        let live = render::blocks(markdown);
        let doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![ReviewBlock {
                hash: live[0].hash.clone(),
                excerpt: live[0].excerpt.clone(),
                kind: AnchorKindDto::Block,
                comments: vec![comment("c_1", "設計方針の根拠")],
            }],
            file_comments: vec![comment("c_2", "全体として章立てが前後している")],
        };

        let out = export_markdown("notes.md", markdown, &doc, "2026-08-24T01:00:00Z");
        let expected = "# Review: notes.md\n\n\
             Exported: 2026-08-24T01:00:00Z · 2 comments (1 on the file, 1 on 1 blocks)\n\n\
             > (file): notes.md\n\n\
             - 全体として章立てが前後している\n\n\
             > L1: ## 設計方針\n\n\
             - 設計方針の根拠\n";
        assert_eq!(out, expected);
        // The file section comes before any block section.
        let file_idx = out.find("(file)").expect("file section present");
        let block_idx = out.find("L1: ## 設計方針").expect("block section present");
        assert!(file_idx < block_idx);
    }

    #[test]
    fn export_markdown_omits_the_file_section_when_there_are_no_file_comments() {
        let doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![],
            file_comments: Vec::new(),
        };
        let out = export_markdown("notes.md", "# Hi\n", &doc, "2026-08-22T07:10:00Z");
        assert!(!out.contains("(file)"));
        // Count format stays the original "M comments on B blocks" — no
        // parenthesized breakdown — when there are no file comments.
        assert!(out.contains("0 comments on 0 blocks"));
        assert!(!out.contains("on the file"));
    }

    #[test]
    fn export_markdown_file_comments_only_has_no_block_sections() {
        let doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![],
            file_comments: vec![comment("c_1", "全体コメント")],
        };
        let out = export_markdown("notes.md", "# Hi\n", &doc, "2026-08-22T07:10:00Z");
        assert_eq!(
            out,
            "# Review: notes.md\n\n\
             Exported: 2026-08-22T07:10:00Z · 1 comments (1 on the file, 0 on 0 blocks)\n\n\
             > (file): notes.md\n\n\
             - 全体コメント\n"
        );
    }

    #[test]
    fn export_markdown_indents_multiline_file_comment_continuation_lines() {
        let doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![],
            file_comments: vec![comment("c_1", "first line\nsecond line")],
        };
        let out = export_markdown("notes.md", "# Hi\n", &doc, "2026-08-22T07:10:00Z");
        assert!(out.contains("- first line\n  second line\n"));
    }

    #[test]
    fn export_markdown_labels_a_multiline_block_with_a_line_range() {
        let markdown = "> line one\n> line two\n";
        let live = render::blocks(markdown);
        assert_eq!(live[0].line_start, 1);
        assert_eq!(live[0].line_end, 2);
        let doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![ReviewBlock {
                hash: live[0].hash.clone(),
                excerpt: live[0].excerpt.clone(),
                kind: AnchorKindDto::Block,
                comments: vec![comment("c_1", "check this")],
            }],
            file_comments: Vec::new(),
        };

        let out = export_markdown("notes.md", markdown, &doc, "2026-08-22T07:10:00Z");
        // No full-source quote any more — just the line range and excerpt,
        // followed directly by the comment list.
        assert!(out.contains("> L1-L2: > line one\n\n- check this\n"));
        assert!(!out.contains("line two"));
    }

    #[test]
    fn export_markdown_labels_a_single_line_block_without_a_range() {
        let markdown = "# Heading\n";
        let live = render::blocks(markdown);
        assert_eq!(live[0].line_start, live[0].line_end);
        let doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![ReviewBlock {
                hash: live[0].hash.clone(),
                excerpt: live[0].excerpt.clone(),
                kind: AnchorKindDto::Block,
                comments: vec![comment("c_1", "ok")],
            }],
            file_comments: Vec::new(),
        };

        let out = export_markdown("notes.md", markdown, &doc, "2026-08-22T07:10:00Z");
        assert!(out.contains("> L1: # Heading\n\n"));
        assert!(!out.contains("L1-L1"));
    }

    #[test]
    fn export_markdown_indents_multiline_comment_continuation_lines() {
        let markdown = "# Heading\n";
        let live = render::blocks(markdown);
        let doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![ReviewBlock {
                hash: live[0].hash.clone(),
                excerpt: live[0].excerpt.clone(),
                kind: AnchorKindDto::Block,
                comments: vec![comment("c_1", "first line\nsecond line\nthird line")],
            }],
            file_comments: Vec::new(),
        };
        let out = export_markdown("notes.md", markdown, &doc, "2026-08-22T07:10:00Z");
        assert!(out.contains("- first line\n  second line\n  third line\n"));
    }

    #[test]
    fn export_markdown_lists_unanchored_comments_in_their_own_section() {
        let markdown = "# Current heading\n";
        let doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![ReviewBlock {
                hash: "0000000000000000".to_string(),
                excerpt: "旧 excerpt".to_string(),
                kind: AnchorKindDto::Block,
                comments: vec![comment("c_1", "コメント本文")],
            }],
            file_comments: Vec::new(),
        };

        let out = export_markdown("notes.md", markdown, &doc, "2026-08-22T07:10:00Z");
        assert!(out.contains("## Unanchored\n\n"));
        assert!(out.contains("> (not found): 旧 excerpt\n\n"));
        assert!(out.contains("- コメント本文\n"));
        // The summary counts only anchored sections, plus an explicit
        // unanchored suffix — not a total across everything.
        assert!(out.contains("0 comments on 0 blocks (+1 unanchored)"));
    }

    #[test]
    fn export_markdown_omits_the_unanchored_section_when_everything_is_anchored() {
        let markdown = "# Hi\n";
        let live = render::blocks(markdown);
        let doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![ReviewBlock {
                hash: live[0].hash.clone(),
                excerpt: live[0].excerpt.clone(),
                kind: AnchorKindDto::Block,
                comments: vec![comment("c_1", "ok")],
            }],
            file_comments: Vec::new(),
        };
        let out = export_markdown("notes.md", markdown, &doc, "2026-08-22T07:10:00Z");
        assert!(!out.contains("Unanchored"));
        assert!(!out.contains("unanchored"));
    }

    #[test]
    fn export_markdown_skips_blocks_with_no_comments() {
        let markdown = "# Heading\n\nUncommented paragraph.\n";
        let live = render::blocks(markdown);
        let doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![ReviewBlock {
                hash: live[0].hash.clone(),
                excerpt: live[0].excerpt.clone(),
                kind: AnchorKindDto::Block,
                comments: vec![comment("c_1", "noted")],
            }],
            file_comments: Vec::new(),
        };
        let out = export_markdown("notes.md", markdown, &doc, "2026-08-22T07:10:00Z");
        assert!(!out.contains("Uncommented paragraph"));
        assert_eq!(out.matches("> L").count(), 1);
    }

    #[test]
    fn export_markdown_orders_sections_by_document_order_not_comment_order() {
        let markdown = "First heading\n\nSecond heading\n";
        let live = render::blocks(markdown);
        // Comments added out of document order (second block commented
        // first) — the export must still number by document position.
        let doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![
                ReviewBlock {
                    hash: live[1].hash.clone(),
                    excerpt: live[1].excerpt.clone(),
                    kind: AnchorKindDto::Block,
                    comments: vec![comment("c_2", "second")],
                },
                ReviewBlock {
                    hash: live[0].hash.clone(),
                    excerpt: live[0].excerpt.clone(),
                    kind: AnchorKindDto::Block,
                    comments: vec![comment("c_1", "first")],
                },
            ],
            file_comments: Vec::new(),
        };
        let out = export_markdown("notes.md", markdown, &doc, "2026-08-22T07:10:00Z");
        let first_idx = out.find("First heading").expect("first heading present");
        let second_idx = out.find("Second heading").expect("second heading present");
        assert!(first_idx < second_idx);
    }

    #[test]
    fn export_markdown_dedups_a_hash_that_appears_in_multiple_live_blocks() {
        // Two identical paragraphs hash the same; only the first
        // occurrence should get a numbered section, and the summary count
        // must reflect that (not double-count the duplicate).
        let markdown = "Same text.\n\nSame text.\n\nOther text.\n";
        let live = render::blocks(markdown);
        assert_eq!(live[0].hash, live[1].hash, "fixture sanity check");

        let doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![ReviewBlock {
                hash: live[0].hash.clone(),
                excerpt: live[0].excerpt.clone(),
                kind: AnchorKindDto::Block,
                comments: vec![comment("c_1", "noted")],
            }],
            file_comments: Vec::new(),
        };
        let out = export_markdown("notes.md", markdown, &doc, "2026-08-22T07:10:00Z");
        assert_eq!(out.matches("> L").count(), 1);
        assert!(out.contains("1 comments on 1 blocks"));
    }

    // -- export_markdown: item/row anchors --------------------------------

    #[test]
    fn export_markdown_labels_a_list_item_with_an_in_list_suffix() {
        let markdown = "- first item\n- second item\n- 三番目の項目\n";
        let live = render::anchors(markdown);
        let block = &live[0];
        assert_eq!(block.kind, render::AnchorKind::Block);
        let third_item = live
            .iter()
            .find(|a| a.kind == render::AnchorKind::Item && a.excerpt == "三番目の項目")
            .expect("third item anchor");

        let doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![ReviewBlock {
                hash: third_item.hash.clone(),
                excerpt: third_item.excerpt.clone(),
                kind: AnchorKindDto::Item,
                comments: vec![comment("c_1", "コメント")],
            }],
            file_comments: Vec::new(),
        };
        let out = export_markdown("notes.md", markdown, &doc, "2026-08-22T07:10:00Z");

        let expected_quote = format!(
            "> {}: 三番目の項目 （in list {}）\n\n- コメント\n",
            line_label(third_item.line_start, third_item.line_end),
            line_label(block.line_start, block.line_end),
        );
        assert!(
            out.contains(&expected_quote),
            "expected {expected_quote:?} in {out:?}"
        );
    }

    #[test]
    fn export_markdown_labels_a_table_row_with_an_in_table_suffix() {
        let markdown = "| 値1 | 値2 | 値3 |\n|---|---|---|\n| a | b | c |\n| d | e | f |\n";
        let live = render::anchors(markdown);
        let block = &live[0];
        assert_eq!(block.kind, render::AnchorKind::Block);
        let second_row = live
            .iter()
            .find(|a| a.kind == render::AnchorKind::Row && a.excerpt == "d | e | f")
            .expect("second data row anchor");

        let doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![ReviewBlock {
                hash: second_row.hash.clone(),
                excerpt: second_row.excerpt.clone(),
                kind: AnchorKindDto::Row,
                comments: vec![comment("c_1", "確認")],
            }],
            file_comments: Vec::new(),
        };
        let out = export_markdown("notes.md", markdown, &doc, "2026-08-22T07:10:00Z");

        let expected_quote = format!(
            "> {}: d | e | f （in table {}）\n\n- 確認\n",
            line_label(second_row.line_start, second_row.line_end),
            line_label(block.line_start, block.line_end),
        );
        assert!(
            out.contains(&expected_quote),
            "expected {expected_quote:?} in {out:?}"
        );
    }

    #[test]
    fn export_markdown_block_anchors_get_no_in_list_or_in_table_suffix() {
        // Regression guard: a plain block-kind anchor's quote line must
        // stay exactly as before nested anchors existed — no suffix at all.
        let markdown = "# Heading\n";
        let live = render::blocks(markdown);
        let doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![ReviewBlock {
                hash: live[0].hash.clone(),
                excerpt: live[0].excerpt.clone(),
                kind: AnchorKindDto::Block,
                comments: vec![comment("c_1", "ok")],
            }],
            file_comments: Vec::new(),
        };
        let out = export_markdown("notes.md", markdown, &doc, "2026-08-22T07:10:00Z");
        assert!(out.contains("> L1: # Heading\n\n"));
        assert!(!out.contains("in list"));
        assert!(!out.contains("in table"));
    }

    #[test]
    fn export_markdown_nested_item_suffix_names_the_whole_list_not_the_immediate_parent_item() {
        // A doubly-nested item's "in list" suffix must point at the
        // top-level block's line range (the whole list), not its
        // immediately enclosing item's — per the nested-anchors design
        // doc: nesting is transparent for export purposes.
        let markdown = "- outer\n  - inner comment target\n";
        let live = render::anchors(markdown);
        let block = &live[0];
        let inner = live
            .iter()
            .find(|a| a.kind == render::AnchorKind::Item && a.excerpt == "inner comment target")
            .expect("inner item anchor");
        // Sanity check the fixture: the inner item's own line range must
        // differ from the block's, or this test wouldn't distinguish the
        // two possible (correct vs. incorrect) suffixes.
        assert_ne!(inner.line_start, block.line_start);

        let doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![ReviewBlock {
                hash: inner.hash.clone(),
                excerpt: inner.excerpt.clone(),
                kind: AnchorKindDto::Item,
                comments: vec![comment("c_1", "note")],
            }],
            file_comments: Vec::new(),
        };
        let out = export_markdown("notes.md", markdown, &doc, "2026-08-22T07:10:00Z");
        let expected_suffix = format!(
            "（in list {}）",
            line_label(block.line_start, block.line_end)
        );
        assert!(
            out.contains(&expected_suffix),
            "expected {expected_suffix:?} in {out:?}"
        );
    }

    #[test]
    fn export_markdown_labels_an_item_inside_a_blockquote_with_an_in_block_suffix() {
        // A list item's *own* kind is Item, but when its enclosing
        // top-level block is a blockquote (not literally a list), the
        // suffix must say "in block", not "in list" — the label is decided
        // by what the ancestor block's own source actually is.
        let markdown = "> - quoted item one\n> - quoted item two\n";
        let live = render::anchors(markdown);
        let block = &live[0];
        assert_eq!(block.kind, render::AnchorKind::Block);
        let item = live
            .iter()
            .find(|a| a.kind == render::AnchorKind::Item && a.excerpt == "quoted item one")
            .expect("quoted item anchor");

        let doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![ReviewBlock {
                hash: item.hash.clone(),
                excerpt: item.excerpt.clone(),
                kind: AnchorKindDto::Item,
                comments: vec![comment("c_1", "確認")],
            }],
            file_comments: Vec::new(),
        };
        let out = export_markdown("notes.md", markdown, &doc, "2026-08-22T07:10:00Z");

        let expected_suffix = format!(
            "（in block {}）",
            line_label(block.line_start, block.line_end)
        );
        assert!(
            out.contains(&expected_suffix),
            "expected {expected_suffix:?} in {out:?}"
        );
        assert!(!out.contains("in list"));
    }

    #[test]
    fn export_markdown_omits_the_unanchored_section_when_the_only_entry_has_no_comments() {
        // Defensive: `ReviewBlock`s with an empty `comments` list shouldn't
        // normally exist (the client drops a block the moment its last
        // comment is deleted), but nothing in review::validate rejects one
        // server-side. A block like that must not open an empty `##
        // Unanchored` section.
        let markdown = "# Current heading\n";
        let doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: vec![ReviewBlock {
                hash: "0000000000000000".to_string(),
                excerpt: "旧 excerpt".to_string(),
                kind: AnchorKindDto::Block,
                comments: vec![],
            }],
            file_comments: Vec::new(),
        };
        let out = export_markdown("notes.md", markdown, &doc, "2026-08-22T07:10:00Z");
        assert!(!out.contains("Unanchored"));
        assert!(!out.contains("unanchored"));
    }

    #[test]
    fn export_markdown_orders_a_block_immediately_followed_by_its_own_items() {
        let markdown = "- one\n- two\n\nUnrelated paragraph.\n";
        let live = render::anchors(markdown);
        let doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: live
                .iter()
                .map(|a| ReviewBlock {
                    hash: a.hash.clone(),
                    excerpt: a.excerpt.clone(),
                    kind: match a.kind {
                        render::AnchorKind::Block => AnchorKindDto::Block,
                        render::AnchorKind::Item => AnchorKindDto::Item,
                        render::AnchorKind::Row => AnchorKindDto::Row,
                    },
                    comments: vec![comment("c_1", "x")],
                })
                .collect(),
            file_comments: Vec::new(),
        };
        let out = export_markdown("notes.md", markdown, &doc, "2026-08-22T07:10:00Z");
        let list_idx = out.find("- one").expect("list block quoted");
        let one_idx = out.find("in list").expect("first item's suffix present");
        let unrelated_idx = out
            .find("Unrelated paragraph")
            .expect("unrelated paragraph quoted");
        assert!(list_idx < one_idx && one_idx < unrelated_idx);
    }

    // -- export (I/O) -----------------------------------------------------------

    #[test]
    fn export_writes_the_file_and_returns_its_name_only_path_and_markdown() {
        let dir = tempfile::tempdir().expect("tempdir");
        let md = dir.path().join("notes.md");
        std::fs::write(&md, "# Hi\n").expect("write md");
        let doc = empty_doc(&md);

        let (path, markdown) = export(&md, "# Hi\n", &doc).expect("export");
        assert_eq!(path, dir.path().join("notes.review.md"));
        let on_disk = std::fs::read_to_string(&path).expect("read exported file");
        assert_eq!(on_disk, markdown);
        assert!(markdown.starts_with("# Review: notes.md"));
    }
}

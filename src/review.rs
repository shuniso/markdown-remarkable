//! Block-level review comments: the JSON sidecar model, atomic load/save,
//! re-anchoring against a re-rendered document, and the Markdown export
//! format handed to an AI agent (or a human) as a review summary.
//!
//! Everything here is pure/file-I/O only — no HTTP, no UI. `routes.rs`
//! wires this up to `GET/PUT /review` and `POST /export`.

use crate::render::{self, Block};
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

/// The full review sidecar document: every block that has ever had a
/// comment, and those comments. Serialized as-is to `<file>.review.json`
/// (see [`sidecar_path`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewDoc {
    pub version: u32,
    pub file: String,
    pub blocks: Vec<ReviewBlock>,
}

/// One commented block: the block identity it was last anchored to (`hash`,
/// `excerpt` — see `render::Block`) and the comments attached to it. A
/// block with no comments left is dropped from `blocks` entirely rather
/// than kept around empty.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewBlock {
    pub hash: String,
    pub excerpt: String,
    pub comments: Vec<Comment>,
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
/// multi-line or oversized text). Doesn't touch the filesystem or
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
        for comment in &block.comments {
            if !is_valid_comment_id(&comment.id) {
                return Err(anyhow!("invalid comment id: {:?}", comment.id));
            }
            if comment.text.len() > MAX_COMMENT_TEXT_BYTES {
                return Err(anyhow!(
                    "comment text exceeds {MAX_COMMENT_TEXT_BYTES} bytes"
                ));
            }
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

/// The hashes in `doc` that have no matching entry in `blocks` — i.e.
/// review blocks whose original source is no longer found anywhere in the
/// current document (edited or deleted). Order follows `doc.blocks`.
pub fn unanchored<'a>(doc: &'a ReviewDoc, blocks: &[Block]) -> Vec<&'a str> {
    doc.blocks
        .iter()
        .filter(|review_block| !blocks.iter().any(|b| b.hash == review_block.hash))
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
/// "Exported: <timestamp> · N comments on M blocks" summary line (with an
/// "(+K unanchored)" suffix appended whenever there are unanchored
/// comments), then one section per commented, anchored block (in document
/// order): a single quoted line giving the block's source line range and
/// excerpt (`> L12-L18: <excerpt>`, or `> L40: <excerpt>` for a one-line
/// block), followed by its comments as a bullet list, where a comment's
/// second and later lines are indented two spaces as continuation lines
/// rather than starting new top-level bullets. The block's full source is
/// deliberately *not* quoted — for handing this off to an AI agent, the
/// line range plus a short excerpt is enough to locate the block, and
/// quoting the whole thing again would just be noise. An `## Unanchored`
/// section follows for comments whose block no longer exists in `markdown`
/// (only emitted when there is at least one), using the same one-line
/// quote format but with `(not found)` in place of a line range, since
/// there's no live block to report one for.
///
/// If the same block hash occurs more than once in the live document (an
/// exact-duplicate block), only its first occurrence gets a section — so
/// `M blocks` in the summary always equals the number of sections actually
/// shown, never double-counting a duplicate.
///
/// `now` is the RFC3339 export timestamp; passed in (rather than computed
/// here) so this stays a pure, deterministically-testable function — see
/// [`export`] for the impure wrapper that supplies it.
pub fn export_markdown(md_name: &str, markdown: &str, doc: &ReviewDoc, now: &str) -> String {
    let live_blocks = render::blocks(markdown);
    let unanchored_hashes = unanchored(doc, &live_blocks);

    // One entry per *distinct* hash that's both anchored (present in the
    // live document) and commented, in document order — the first
    // occurrence wins whenever the same hash repeats.
    let mut seen_hashes: HashSet<&str> = HashSet::new();
    let mut sections: Vec<(&Block, &ReviewBlock)> = Vec::new();
    for block in &live_blocks {
        if !seen_hashes.insert(block.hash.as_str()) {
            continue;
        }
        if let Some(review_block) = doc.blocks.iter().find(|rb| rb.hash == block.hash) {
            if !review_block.comments.is_empty() {
                sections.push((block, review_block));
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

    let mut out = String::new();
    out.push_str(&format!("# Review: {md_name}\n\n"));
    out.push_str(&format!(
        "Exported: {now} · {anchored_comments} comments on {anchored_blocks} blocks"
    ));
    if unanchored_comments > 0 {
        out.push_str(&format!(" (+{unanchored_comments} unanchored)"));
    }
    out.push_str("\n\n");

    for (block, review_block) in &sections {
        out.push_str("> ");
        out.push_str(&line_label(block.line_start, block.line_end));
        out.push_str(": ");
        out.push_str(&single_line(&block.excerpt));
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
                comments: vec![comment("c_1", "looks good")],
            }],
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
                comments: vec![comment("c_0123456789abcdef", "ok")],
            }],
        };
        assert!(validate(&mut doc).is_ok());
    }

    #[test]
    fn validate_rejects_wrong_version() {
        let mut doc = ReviewDoc {
            version: 2,
            file: "notes.md".to_string(),
            blocks: vec![],
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
                comments: vec![],
            }],
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
                comments: vec![comment("", "ok")],
            }],
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
                comments: vec![comment("not-the-right-format", "ok")],
            }],
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
                comments: vec![comment("c_0123456789ABCDEF", "ok")],
            }],
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
                comments: vec![comment("c_0123456789abcdef", "ok")],
            }],
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
                comments: vec![comment("c_0123456789abcdef", &"x".repeat(64 * 1024 + 1))],
            }],
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
                comments: vec![comment("c_0123456789abcdef", &"x".repeat(64 * 1024))],
            }],
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
                comments: vec![],
            }],
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
                    comments: vec![comment("c_1", "x")],
                },
                ReviewBlock {
                    hash: "bbbbbbbbbbbbbbbb".to_string(),
                    excerpt: "b".to_string(),
                    comments: vec![comment("c_2", "y")],
                },
            ],
        };
        let live = render::blocks("some text that hashes to something else\n");
        let result = unanchored(&doc, &live);
        assert_eq!(result.len(), 2);
        assert!(result.contains(&"aaaaaaaaaaaaaaaa"));
        assert!(result.contains(&"bbbbbbbbbbbbbbbb"));
    }

    #[test]
    fn unanchored_is_empty_when_every_hash_matches() {
        let live = render::blocks("# Title\n\nbody text\n");
        let doc = ReviewDoc {
            version: 1,
            file: "notes.md".to_string(),
            blocks: live
                .iter()
                .map(|b| ReviewBlock {
                    hash: b.hash.clone(),
                    excerpt: b.excerpt.clone(),
                    comments: vec![comment("c_1", "ok")],
                })
                .collect(),
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
                    comments: vec![
                        comment("c_1", "ここは根拠が弱い"),
                        comment("c_2", "代替案も書く"),
                    ],
                },
                ReviewBlock {
                    hash: para_hash,
                    excerpt: live[1].excerpt.clone(),
                    comments: vec![comment("c_3", "…")],
                },
            ],
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
                comments: vec![comment("c_1", "check this")],
            }],
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
                comments: vec![comment("c_1", "ok")],
            }],
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
                comments: vec![comment("c_1", "first line\nsecond line\nthird line")],
            }],
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
                comments: vec![comment("c_1", "コメント本文")],
            }],
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
                comments: vec![comment("c_1", "ok")],
            }],
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
                comments: vec![comment("c_1", "noted")],
            }],
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
                    comments: vec![comment("c_2", "second")],
                },
                ReviewBlock {
                    hash: live[0].hash.clone(),
                    excerpt: live[0].excerpt.clone(),
                    comments: vec![comment("c_1", "first")],
                },
            ],
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
                comments: vec![comment("c_1", "noted")],
            }],
        };
        let out = export_markdown("notes.md", markdown, &doc, "2026-08-22T07:10:00Z");
        assert_eq!(out.matches("> L").count(), 1);
        assert!(out.contains("1 comments on 1 blocks"));
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

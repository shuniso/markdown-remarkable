//! Filesystem watching: bumps a shared version counter whenever the viewed
//! file changes, so the HTTP server can tell the browser to reload.

use anyhow::{Context, Result};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Watches the parent directory of `path` (non-recursively) and increments
/// `version` whenever a Modify, Create, or Remove filesystem event touches
/// `path` itself. Remove is included (not just Modify/Create) so deleting
/// the file also triggers a reload — the server then serves its `500` page
/// instead of silently going stale — and a subsequent recreate is picked up
/// the same way.
///
/// Watching the *directory* rather than the file directly is deliberate:
/// many editors save "atomically" — write a temp file, then rename it over
/// the original — which some platforms/watchers only reliably surface as
/// events on the containing directory. Watching the directory catches the
/// resulting create/rename regardless of which save strategy the editor
/// uses.
///
/// The returned `RecommendedWatcher` must be kept alive by the caller (e.g.
/// bound to a variable in `main` for the lifetime of the program) — the
/// watch stops as soon as it is dropped.
pub fn watch(path: &Path, version: Arc<AtomicU64>) -> Result<RecommendedWatcher> {
    let target = path
        .canonicalize()
        .with_context(|| format!("failed to resolve path {}", path.display()))?;
    let parent = target
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
        let event = match result {
            Ok(event) => event,
            Err(err) => {
                eprintln!("warning: watch error: {err}");
                return;
            }
        };
        if !matches!(
            event.kind,
            EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
        ) {
            return;
        }
        let touches_target = event.paths.iter().any(|changed| {
            changed == &target || changed.canonicalize().ok().as_ref() == Some(&target)
        });
        if touches_target {
            version.fetch_add(1, Ordering::SeqCst);
        }
    })
    .context("failed to create filesystem watcher")?;

    watcher
        .watch(&parent, RecursiveMode::NonRecursive)
        .with_context(|| format!("failed to watch directory {}", parent.display()))?;

    Ok(watcher)
}

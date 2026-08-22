//! Integration tests for `watch::watch`: does it actually notice file
//! changes on the real filesystem, across every save strategy an editor
//! might use, and does it actually stop once dropped?

use markdown_remarkable::watch;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Polls `version` until it reaches at least `minimum`, up to `timeout`.
/// Filesystem watchers are inherently asynchronous (and, on some
/// platforms/backends, somewhat latent), so tests need to wait rather than
/// assert immediately after the write that should trigger an event.
fn wait_for_version_at_least(version: &AtomicU64, minimum: u64, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if version.load(Ordering::SeqCst) >= minimum {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn detects_in_place_appends() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let file_path = dir.path().join("doc.md");
    std::fs::write(&file_path, "start\n").expect("write initial file");

    let version = Arc::new(AtomicU64::new(0));
    let _watcher = watch::watch(&file_path, Arc::clone(&version)).expect("start watcher");

    let mut file = OpenOptions::new()
        .append(true)
        .open(&file_path)
        .expect("open file for append");
    file.write_all(b"more\n").expect("append to file");
    drop(file);

    assert!(
        wait_for_version_at_least(&version, 1, Duration::from_secs(3)),
        "version did not increment after an in-place append"
    );
}

#[test]
fn detects_atomic_rename_saves() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let file_path = dir.path().join("doc.md");
    std::fs::write(&file_path, "start\n").expect("write initial file");

    let version = Arc::new(AtomicU64::new(0));
    let _watcher = watch::watch(&file_path, Arc::clone(&version)).expect("start watcher");

    // Many editors "save" by writing a new temp file and renaming it over
    // the original, rather than writing in place.
    let tmp_path = dir.path().join("doc.md.tmp");
    std::fs::write(&tmp_path, "replaced via rename\n").expect("write replacement temp file");
    std::fs::rename(&tmp_path, &file_path).expect("atomically rename over the target file");

    assert!(
        wait_for_version_at_least(&version, 1, Duration::from_secs(3)),
        "version did not increment after an atomic rename save"
    );
}

#[test]
fn detects_deletion_alone() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let file_path = dir.path().join("doc.md");
    std::fs::write(&file_path, "start\n").expect("write initial file");

    let version = Arc::new(AtomicU64::new(0));
    let _watcher = watch::watch(&file_path, Arc::clone(&version)).expect("start watcher");

    std::fs::remove_file(&file_path).expect("remove file");

    assert!(
        wait_for_version_at_least(&version, 1, Duration::from_secs(3)),
        "version did not increment after the file was deleted (with no recreate)"
    );
}

#[test]
fn detects_remove_then_recreate() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let file_path = dir.path().join("doc.md");
    std::fs::write(&file_path, "start\n").expect("write initial file");

    let version = Arc::new(AtomicU64::new(0));
    let _watcher = watch::watch(&file_path, Arc::clone(&version)).expect("start watcher");

    std::fs::remove_file(&file_path).expect("remove file");
    std::fs::write(&file_path, "recreated\n").expect("recreate file under the same name");

    assert!(
        wait_for_version_at_least(&version, 1, Duration::from_secs(3)),
        "version did not increment after remove + recreate"
    );
}

#[test]
fn stops_reporting_once_the_watcher_is_dropped() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let file_path = dir.path().join("doc.md");
    std::fs::write(&file_path, "start\n").expect("write initial file");

    let version = Arc::new(AtomicU64::new(0));
    let watcher = watch::watch(&file_path, Arc::clone(&version)).expect("start watcher");
    drop(watcher);

    // Some backends (e.g. Linux's inotify) tear the watch down
    // asynchronously; without a beat to let that finish, a write right
    // after `drop` could still be seen and make this test flaky.
    thread::sleep(Duration::from_millis(200));

    std::fs::write(&file_path, "after drop\n").expect("write after dropping the watcher");
    thread::sleep(Duration::from_millis(500));

    assert_eq!(
        version.load(Ordering::SeqCst),
        0,
        "version incremented even though the watcher had been dropped"
    );
}

//! Window geometry persistence: pure functions for the config-file path,
//! size clamping, and validating a saved position against the monitors
//! actually attached — plus the thin (untested by design; see `app.rs`'s
//! module docs for why GUI-adjacent code isn't) file I/O wrapper `app.rs`
//! calls on startup/shutdown.
//!
//! Every function above `load`/`save` is a pure function of its arguments —
//! no `std::env`, no filesystem — specifically so the platform-specific path
//! logic and the clamping/validation rules can be unit-tested directly
//! without faking environment variables or a real monitor.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Matches `app.rs`'s `WindowBuilder::with_min_inner_size` — a saved size
/// smaller than this (e.g. from a future build with a smaller minimum, or a
/// hand-edited file) is clamped up to it rather than handed to the window
/// builder as-is.
pub const MIN_WIDTH: f64 = 480.0;
pub const MIN_HEIGHT: f64 = 320.0;

/// `~/Library/Application Support/mdview/window.json` (macOS) /
/// `$XDG_CONFIG_HOME/mdview/window.json` (Linux, falling back to
/// `~/.config`) / `%APPDATA%\mdview\window.json` (Windows), read on startup
/// and written on close/move/resize by `app.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// The rectangle (logical pixels) a monitor occupies, as reported by
/// `tao::event_loop::EventLoop::available_monitors`. Only what
/// [`position_is_visible`] needs — not `tao`'s own `MonitorHandle`, so this
/// module (and its tests) don't depend on `tao` at all.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MonitorRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Which OS-specific config-path rule to apply — see [`config_path_for`].
/// A plain enum (rather than `#[cfg(target_os = ...)]` branching baked
/// directly into the path logic) so every branch can be unit-tested
/// regardless of which platform the tests happen to run on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Macos,
    Windows,
    /// Everything else (Linux and other Unix-likes): the XDG Base Directory
    /// rule.
    Other,
}

#[cfg(target_os = "macos")]
const CURRENT_PLATFORM: Platform = Platform::Macos;
#[cfg(target_os = "windows")]
const CURRENT_PLATFORM: Platform = Platform::Windows;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const CURRENT_PLATFORM: Platform = Platform::Other;

/// The config-file path for `platform`, given the relevant environment
/// variables' values (`None` if unset/empty). Pure so every platform's rule
/// can be tested from any host — see [`config_path`] for the real,
/// environment-reading entry point `app.rs` actually calls.
///
/// - macOS: `<home>/Library/Application Support/mdview/window.json`.
///   `None` if `home` is `None` (no `HOME` in the environment).
/// - Windows: `<appdata>\mdview\window.json`. `None` if `appdata` is `None`.
/// - Other (Linux etc.): `<xdg_config_home>/mdview/window.json`, or
///   `<home>/.config/mdview/window.json` if `xdg_config_home` is `None`.
///   `None` if both are `None`.
pub fn config_path_for(
    platform: Platform,
    home: Option<&Path>,
    xdg_config_home: Option<&Path>,
    appdata: Option<&Path>,
) -> Option<PathBuf> {
    match platform {
        Platform::Macos => {
            home.map(|home| home.join("Library/Application Support/mdview/window.json"))
        }
        Platform::Windows => appdata.map(|appdata| appdata.join("mdview/window.json")),
        Platform::Other => xdg_config_home
            .map(Path::to_path_buf)
            .or_else(|| home.map(|home| home.join(".config")))
            .map(|base| base.join("mdview/window.json")),
    }
}

/// The config-file path for the platform this binary is actually running
/// on, read from the real environment (`HOME` / `XDG_CONFIG_HOME` /
/// `APPDATA`). `None` means there's nowhere sensible to read/write window
/// state — `app.rs` treats that the same as any other I/O failure: a
/// warning, never a hard error.
pub fn config_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let xdg_config_home = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let appdata = std::env::var_os("APPDATA").map(PathBuf::from);
    config_path_for(
        CURRENT_PLATFORM,
        home.as_deref(),
        xdg_config_home.as_deref(),
        appdata.as_deref(),
    )
}

/// Clamps a saved (or about-to-be-saved) size up to at least
/// [`MIN_WIDTH`]/[`MIN_HEIGHT`] — the same floor `app.rs` gives the window
/// itself via `with_min_inner_size`, so a state file from an older/smaller
/// minimum (or a hand-edited one) never asks the window builder for a size
/// smaller than the window is allowed to be.
pub fn clamp_size(width: f64, height: f64) -> (f64, f64) {
    (width.max(MIN_WIDTH), height.max(MIN_HEIGHT))
}

/// True if `(x, y)` — a saved window's top-left corner — falls inside at
/// least one of `monitors`. Used to decide whether a saved position is
/// still meaningful (the monitor it was on might have been unplugged, or
/// the saved file might just be stale/hand-edited); when it isn't, `app.rs`
/// applies only the saved size and lets the OS pick a position instead of
/// planting the window off-screen.
pub fn position_is_visible(x: f64, y: f64, monitors: &[MonitorRect]) -> bool {
    monitors
        .iter()
        .any(|m| x >= m.x && x < m.x + m.width && y >= m.y && y < m.y + m.height)
}

/// True only if every field of `state` is finite — rejects NaN and ±∞.
/// `serde_json` already refuses to parse an out-of-range JSON number
/// literal like `1e400` at all (a hard parse error, not a silent overflow
/// to infinity — see `load_rejects_a_non_finite_field`'s doc comment), so
/// this is defense-in-depth against any other way a non-finite value could
/// reach [`load`], rather than something the current JSON path can
/// actually trigger: `resolve_initial_geometry`/`position_is_visible` in
/// app.rs do arithmetic on these fields and aren't written to cope with
/// NaN/∞.
fn all_finite(state: &WindowState) -> bool {
    state.x.is_finite()
        && state.y.is_finite()
        && state.width.is_finite()
        && state.height.is_finite()
}

/// Reads and parses the window-state file at `path`. `None` covers every
/// failure mode uniformly (missing file, unreadable, malformed JSON, a
/// non-finite field) — callers can't tell these apart and don't need to: a
/// missing/bad state file just means "start with the defaults," never a
/// hard error.
pub fn load(path: &Path) -> Option<WindowState> {
    let contents = std::fs::read_to_string(path).ok()?;
    let state: WindowState = serde_json::from_str(&contents).ok()?;
    all_finite(&state).then_some(state)
}

/// Process-wide counter mixed into every temp file name [`save`] creates,
/// so two saves racing in the same process (shouldn't happen in practice —
/// `app.rs` only ever calls this from the single-threaded event loop — but
/// costs nothing to rule out) never try to create the same temp path.
static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Writes `state` to `path` as JSON, creating parent directories as
/// needed. Atomic and safe against a temp-file symlink race, the same way
/// `review::atomic_write` is: written to a `<path>.<pid>.<counter>.tmp`
/// sibling created with `create_new` (fails rather than following a
/// symlink an attacker planted at that exact name), then renamed into
/// place — so a reader (this process's own next `load`, or a concurrently
/// running `mdview`) never observes a half-written file, and a crash
/// mid-write leaves the previous, still-valid `window.json` in place
/// rather than a corrupted one.
pub fn save(path: &Path, state: &WindowState) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(state)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;

    let pid = std::process::id();
    let counter = TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(format!(".{pid}.{counter}.tmp"));
    let tmp_path = PathBuf::from(tmp_name);

    let write_result = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp_path)
        .and_then(|mut file| {
            use std::io::Write;
            file.write_all(json.as_bytes())
        });
    if let Err(err) = write_result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }

    if let Err(err) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- config_path_for ---------------------------------------------

    #[test]
    fn macos_path_is_under_application_support() {
        let path = config_path_for(Platform::Macos, Some(Path::new("/Users/alice")), None, None);
        assert_eq!(
            path,
            Some(PathBuf::from(
                "/Users/alice/Library/Application Support/mdview/window.json"
            ))
        );
    }

    #[test]
    fn macos_path_is_none_without_home() {
        assert_eq!(config_path_for(Platform::Macos, None, None, None), None);
    }

    #[test]
    fn windows_path_is_under_appdata() {
        let path = config_path_for(
            Platform::Windows,
            None,
            None,
            Some(Path::new(r"C:\Users\alice\AppData\Roaming")),
        );
        assert_eq!(
            path,
            Some(PathBuf::from(
                r"C:\Users\alice\AppData\Roaming/mdview/window.json"
            ))
        );
    }

    #[test]
    fn windows_path_is_none_without_appdata() {
        assert_eq!(
            config_path_for(
                Platform::Windows,
                Some(Path::new("/home/alice")),
                None,
                None
            ),
            None
        );
    }

    #[test]
    fn other_path_prefers_xdg_config_home() {
        let path = config_path_for(
            Platform::Other,
            Some(Path::new("/home/alice")),
            Some(Path::new("/custom/config")),
            None,
        );
        assert_eq!(
            path,
            Some(PathBuf::from("/custom/config/mdview/window.json"))
        );
    }

    #[test]
    fn other_path_falls_back_to_home_dot_config() {
        let path = config_path_for(Platform::Other, Some(Path::new("/home/alice")), None, None);
        assert_eq!(
            path,
            Some(PathBuf::from("/home/alice/.config/mdview/window.json"))
        );
    }

    #[test]
    fn other_path_is_none_without_home_or_xdg() {
        assert_eq!(config_path_for(Platform::Other, None, None, None), None);
    }

    // -- clamp_size -----------------------------------------------------

    #[test]
    fn clamp_size_leaves_a_large_enough_size_untouched() {
        assert_eq!(clamp_size(1024.0, 768.0), (1024.0, 768.0));
    }

    #[test]
    fn clamp_size_raises_a_too_small_width_and_height() {
        assert_eq!(clamp_size(100.0, 100.0), (MIN_WIDTH, MIN_HEIGHT));
    }

    #[test]
    fn clamp_size_only_raises_the_dimension_that_needs_it() {
        assert_eq!(clamp_size(100.0, 900.0), (MIN_WIDTH, 900.0));
        assert_eq!(clamp_size(900.0, 100.0), (900.0, MIN_HEIGHT));
    }

    // -- position_is_visible ---------------------------------------------

    #[test]
    fn position_inside_a_monitor_is_visible() {
        let monitors = [MonitorRect {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
        }];
        assert!(position_is_visible(100.0, 100.0, &monitors));
    }

    #[test]
    fn position_outside_every_monitor_is_not_visible() {
        let monitors = [MonitorRect {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
        }];
        assert!(!position_is_visible(-50.0, 100.0, &monitors));
        assert!(!position_is_visible(2000.0, 100.0, &monitors));
        assert!(!position_is_visible(100.0, 2000.0, &monitors));
    }

    #[test]
    fn position_visible_on_any_one_of_several_monitors() {
        let monitors = [
            MonitorRect {
                x: 0.0,
                y: 0.0,
                width: 1920.0,
                height: 1080.0,
            },
            MonitorRect {
                x: 1920.0,
                y: 0.0,
                width: 1920.0,
                height: 1080.0,
            },
        ];
        assert!(position_is_visible(2000.0, 500.0, &monitors));
    }

    #[test]
    fn no_monitors_means_no_position_is_visible() {
        assert!(!position_is_visible(0.0, 0.0, &[]));
    }

    #[test]
    fn monitor_bounds_are_half_open() {
        // The bottom/right edge itself is just past the last visible pixel.
        let monitors = [MonitorRect {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 100.0,
        }];
        assert!(position_is_visible(0.0, 0.0, &monitors));
        assert!(!position_is_visible(100.0, 0.0, &monitors));
        assert!(!position_is_visible(0.0, 100.0, &monitors));
    }

    // -- load / save (JSON round-trip + real file I/O) --------------------

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("nested").join("window.json");
        let state = WindowState {
            x: 12.0,
            y: 34.0,
            width: 1024.0,
            height: 768.0,
        };

        save(&path, &state).expect("save window state");
        let loaded = load(&path).expect("state file should parse");
        assert_eq!(loaded, state);
    }

    #[test]
    fn load_of_a_missing_file_is_none() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("does-not-exist.json");
        assert_eq!(load(&path), None);
    }

    #[test]
    fn load_of_malformed_json_is_none() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("window.json");
        std::fs::write(&path, "not json").expect("write malformed file");
        assert_eq!(load(&path), None);
    }

    #[test]
    fn load_rejects_a_non_finite_field() {
        // `1e400` is a syntactically valid JSON number token; serde_json
        // itself already rejects it at parse time ("number out of range"),
        // so this passes even without an explicit finiteness check in
        // `load` — kept as the regression test the review asked for. See
        // `all_finite_rejects_nan_and_infinity_directly` below for a test
        // that actually exercises the `all_finite` guard in isolation.
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("window.json");
        std::fs::write(&path, r#"{"x":1e400,"y":0.0,"width":800.0,"height":600.0}"#)
            .expect("write file with an out-of-range x");
        assert_eq!(load(&path), None);
    }

    #[test]
    fn all_finite_rejects_nan_and_infinity_directly() {
        // Exercises the `all_finite` guard on directly-constructed values
        // (bypassing JSON entirely) — the actual line of defense once
        // serde_json's own number-range check is out of the picture (e.g.
        // if a future parser swap silently started saturating to
        // infinity instead of erroring).
        assert!(!all_finite(&WindowState {
            x: f64::NAN,
            y: 0.0,
            width: 800.0,
            height: 600.0,
        }));
        assert!(!all_finite(&WindowState {
            x: 0.0,
            y: f64::INFINITY,
            width: 800.0,
            height: 600.0,
        }));
        assert!(!all_finite(&WindowState {
            x: 0.0,
            y: 0.0,
            width: f64::NEG_INFINITY,
            height: 600.0,
        }));
        assert!(!all_finite(&WindowState {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: f64::NAN,
        }));
        assert!(all_finite(&WindowState {
            x: 1.0,
            y: 2.0,
            width: 800.0,
            height: 600.0,
        }));
    }

    #[test]
    fn save_overwrites_a_malformed_existing_file() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("window.json");
        std::fs::write(&path, "not json").expect("write malformed file");

        let state = WindowState {
            x: 10.0,
            y: 20.0,
            width: 900.0,
            height: 700.0,
        };
        save(&path, &state).expect("save window state");

        assert_eq!(load(&path), Some(state));
    }

    #[test]
    fn save_does_not_leave_a_tmp_file_behind() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("window.json");
        let state = WindowState {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 600.0,
        };

        save(&path, &state).expect("save window state");

        let leftover_tmp_files: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read tempdir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().extension().and_then(|e| e.to_str()) == Some("tmp"))
            .collect();
        assert!(
            leftover_tmp_files.is_empty(),
            "leftover tmp files: {leftover_tmp_files:?}"
        );
    }
}

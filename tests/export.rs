//! CLI-level integration tests for `--export`, run against the actual
//! compiled `mdview` binary (`CARGO_BIN_EXE_mdview`) rather than calling
//! library functions directly, since the self-overwrite guard and the
//! `--export`/`--port`/`--no-open` conflict are both `main.rs` concerns.

use std::process::Command;

#[test]
fn refuses_to_export_over_the_input_file() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let file_path = dir.path().join("doc.md");
    let original_content = "# Original\n";
    std::fs::write(&file_path, original_content).expect("write markdown file");

    let output = Command::new(env!("CARGO_BIN_EXE_mdview"))
        .arg(&file_path)
        .arg("--export")
        .arg(&file_path)
        .output()
        .expect("run mdview");

    assert!(
        !output.status.success(),
        "expected a non-zero exit when --export targets the input file, got: {:?}",
        output.status
    );

    let content_after =
        std::fs::read_to_string(&file_path).expect("read markdown file after the export attempt");
    assert_eq!(
        content_after, original_content,
        "input file was modified despite the self-overwrite guard"
    );
}

#[test]
#[cfg(unix)]
fn refuses_to_export_over_a_symlink_to_the_input_file() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let file_path = dir.path().join("doc.md");
    let original_content = "# Original\n";
    std::fs::write(&file_path, original_content).expect("write markdown file");

    let symlink_path = dir.path().join("alias.md");
    std::os::unix::fs::symlink(&file_path, &symlink_path).expect("create symlink to input file");

    let output = Command::new(env!("CARGO_BIN_EXE_mdview"))
        .arg(&file_path)
        .arg("--export")
        .arg(&symlink_path)
        .output()
        .expect("run mdview");

    assert!(
        !output.status.success(),
        "expected a non-zero exit when --export targets a symlink to the input file, got: {:?}",
        output.status
    );

    let content_after =
        std::fs::read_to_string(&file_path).expect("read markdown file after the export attempt");
    assert_eq!(
        content_after, original_content,
        "input file was modified despite the symlink self-overwrite guard"
    );
}

#[test]
#[cfg(unix)]
fn refuses_to_export_over_a_hard_link_to_the_input_file() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let file_path = dir.path().join("doc.md");
    let original_content = "# Original\n";
    std::fs::write(&file_path, original_content).expect("write markdown file");

    // A hard link is a different path with no resolvable symlink target —
    // `canonicalize` alone can't tell it apart from an unrelated file, only
    // the (dev, ino) comparison can.
    let hardlink_path = dir.path().join("hardlink.md");
    std::fs::hard_link(&file_path, &hardlink_path).expect("create hard link to input file");

    let output = Command::new(env!("CARGO_BIN_EXE_mdview"))
        .arg(&file_path)
        .arg("--export")
        .arg(&hardlink_path)
        .output()
        .expect("run mdview");

    assert!(
        !output.status.success(),
        "expected a non-zero exit when --export targets a hard link to the input file, got: {:?}",
        output.status
    );

    let content_after =
        std::fs::read_to_string(&file_path).expect("read markdown file after the export attempt");
    assert_eq!(
        content_after, original_content,
        "input file was modified despite the hard-link self-overwrite guard"
    );
}

#[test]
fn refuses_to_export_when_the_output_directory_does_not_exist() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let input_path = dir.path().join("doc.md");
    std::fs::write(&input_path, "# Hello\n").expect("write markdown file");

    let output_path = dir.path().join("no-such-subdir").join("out.html");

    let output = Command::new(env!("CARGO_BIN_EXE_mdview"))
        .arg(&input_path)
        .arg("--export")
        .arg(&output_path)
        .output()
        .expect("run mdview");

    assert!(
        !output.status.success(),
        "expected a non-zero exit when the output directory doesn't exist, got: {:?}",
        output.status
    );
    assert!(
        !output_path.exists(),
        "no file should have been created under the missing directory"
    );
}

#[test]
fn exports_a_standalone_non_live_html_file() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let input_path = dir.path().join("doc.md");
    let output_path = dir.path().join("out.html");
    std::fs::write(&input_path, "# Hello\n").expect("write markdown file");

    let status = Command::new(env!("CARGO_BIN_EXE_mdview"))
        .arg(&input_path)
        .arg("--export")
        .arg(&output_path)
        .status()
        .expect("run mdview");

    assert!(status.success(), "expected a successful export");

    let exported = std::fs::read_to_string(&output_path).expect("read exported html file");
    assert!(exported.contains("<!doctype html>"));
    assert!(!exported.contains("/version"));
}

#[test]
fn export_conflicts_with_port_and_no_open() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let input_path = dir.path().join("doc.md");
    let output_path = dir.path().join("out.html");
    std::fs::write(&input_path, "# Hello\n").expect("write markdown file");

    let with_port = Command::new(env!("CARGO_BIN_EXE_mdview"))
        .arg(&input_path)
        .arg("--export")
        .arg(&output_path)
        .arg("--port")
        .arg("1234")
        .output()
        .expect("run mdview");
    assert!(
        !with_port.status.success(),
        "expected --export and --port to be rejected together"
    );

    let with_no_open = Command::new(env!("CARGO_BIN_EXE_mdview"))
        .arg(&input_path)
        .arg("--export")
        .arg(&output_path)
        .arg("--no-open")
        .output()
        .expect("run mdview");
    assert!(
        !with_no_open.status.success(),
        "expected --export and --no-open to be rejected together"
    );
}

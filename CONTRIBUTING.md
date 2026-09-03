# Contributing to markdown-remarkable

Thanks for your interest in contributing! This document covers everything
you need to build, test, and submit changes.

> **Note on naming**: the crate and binary are `markdown-remarkable`. You
> may still see the older name `mdview` in a few places — some of these are
> deliberately frozen internal identifiers, not leftovers to clean up:
> the custom URL scheme / page origin `mdview://localhost` (`http://mdview.localhost`
> on Windows), the CSRF header `X-Mdview-Request`, the `mdview.*` localStorage
> keys, the `window.__mdview*` globals, the `.mdview-error` CSS class, the
> environment variables `MDVIEW_DEBUG`/`MDVIEW_SIGN_IDENTITY`, and the macOS
> icon file `packaging/macos/mdview.icns`. These are kept as-is for
> compatibility (origin, header contract, saved settings) and are not
> renamed. Don't try to "finish" the rename.

## Development environment

- **Rust**: 1.88 or newer (see `rust-version` in `Cargo.toml`). Install via
  [rustup](https://rustup.rs/).
- **Linux**: the native window uses WebKitGTK, so building/running it (not
  `--browser` mode) needs the development packages:

  ```sh
  # Debian / Ubuntu
  sudo apt-get install -y libwebkit2gtk-4.1-dev libgtk-3-dev libxdo-dev

  # Fedora
  sudo dnf install -y gtk3-devel webkit2gtk4.1-devel

  # Arch
  sudo pacman -S webkit2gtk-4.1 gtk3
  ```

  See the README's [Linux dependencies](README.md#linux-dependencies)
  section for more detail (runtime WebKitGTK version requirements, the
  `--browser` fallback, etc).
- **Windows**: no extra dependencies are needed to build. At runtime, the
  native window needs the Microsoft Edge WebView2 Runtime (preinstalled on
  Windows 11; see the [Evergreen
  Bootstrapper](https://developer.microsoft.com/microsoft-edge/webview2/)
  on older systems).
- **macOS**: no extra dependencies are needed.

## Building, running, and testing

```sh
cargo build                                   # debug build
cargo run -- <file.md>                        # native window
cargo run -- --browser <file.md>              # browser tab instead
cargo test --locked                           # run the test suite
```

Before submitting a change, make sure it's clean under the same checks CI
runs:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

CI (`.github/workflows/ci.yml`) runs all three on Linux, macOS, and
Windows for every push to `main` and every pull request — a change that
fails any of them on any platform won't merge.

## Manual QA

Automated tests cover what they can, but some UI behavior (pane
resizing, zoom, drag interactions, window position memory, and similar
real-device/browser behavior) can only be verified by hand. If your change
touches the UI, run through
[`docs/qa/baseline-checklist.md`](docs/qa/baseline-checklist.md) — ideally
on both the native window and `--browser` mode, and in both light and dark
OS themes — before opening a PR.

## Commit messages

This project follows [Conventional
Commits](https://www.conventionalcommits.org/): `feat:`, `fix:`, `docs:`,
`chore:`, `refactor:`, `test:`, and so on, optionally with a scope (e.g.
`fix(macos): ...`).

## Pull request process

1. Fork the repository and create a branch off `main` for your change.
2. Keep pull requests small and focused — a PR that does one thing is much
   easier to review than one that bundles several unrelated changes.
3. Make sure `cargo fmt`, `cargo clippy`, and `cargo test` all pass
   locally, and run the manual QA checklist if applicable (see above).
4. Open a pull request against `main`. Describe what changed and why;
   link any related issue.

## Reporting security vulnerabilities

Please **do not** open a public issue for a security vulnerability.
Instead, follow the reporting instructions in
[`docs/SECURITY.md`](docs/SECURITY.md) (GitHub's private vulnerability
reporting, or an issue with no vulnerability details if that isn't
available to you).

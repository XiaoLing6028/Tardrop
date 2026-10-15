# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

TarDrop is a single-binary Rust/GTK4 desktop app that installs portable Linux application archives
(`.tar`, `.tar.gz`/`.tgz`, `.tar.xz`, `.tar.bz2`, `.zip`) into `~/Applications` and publishes an XDG
launcher into `~/.local/share/applications`. Everything is user-local; there is no root component.

## Commands

```bash
cargo run                 # development build + launch
cargo build --release     # release build (LTO, codegen-units=1, stripped)
./target/release/tardrop
cargo check               # fastest way to validate a change
cargo clippy
cargo test                # unit tests (needs the same GTK/libadwaita dev packages: it's a bin crate)
```

Requires GTK 4 and libadwaita **development** packages on the system (the bindings link against
them): `sudo dnf install gtk4-devel libadwaita-devel` on Fedora, `libgtk-4-dev libadwaita-1-dev`
on Debian/Ubuntu. Rust edition 2024, so a recent stable toolchain is needed.

Unit tests live in `#[cfg(test)] mod tests` blocks in `security`, `archive`, and `installer`.
`test_support` (test-only) builds real tar/zip archives with the same crates the extractor reads. Tests
that call `installer::install` use `with_sandbox_home`, which re-executes the test binary with `HOME`
pointed at a tempdir: `std::env::set_var` is `unsafe` in edition 2024 and the crate forbids unsafe, so
never mutate the environment in-process. `ui::tests` has one test that drives the real window through every decision dialog; it is
`#[ignore]`d because it needs a display, so run it with `cargo test -- --ignored` (under `xvfb-run`
to keep the window off screen). Otherwise verify UI changes by running the app.

## Architecture

`src/main.rs` is a 30-line shim: it builds an `adw::Application` (libadwaita, not plain GTK, so the
window inherits the desktop's light/dark preference and accent colour) and hands off to
`ui::build_ui`. It calls `run_with_args::<&str>(&[])` deliberately, so GTK never parses archive
paths as CLI switches. The crate is `#![forbid(unsafe_code)]`.

The central rule is a **security boundary**: `ui` does presentation only; all path handling,
extraction, and publication live in the installer modules. Keep it that way when adding features.

- `archive` — extension-based format detection and streaming extraction. Never shells out to
  `tar`/`unzip`. `safe_destination` rejects any component that is not `Component::Normal`, and only
  regular files and directories are written; symlinks, hardlinks, and device nodes are hard errors.
  `survey` is a read-only pass over member headers that collects every link and `install.sh` and
  decides whether each link escapes the package folder, following the archive's own symlinks.
- `installer` — the survey → extract → inspect → publish transaction (`install()`). Under
  `Policy::Enforce` a survey with links or `install.sh` refuses before any write, with a
  `RiskAssessment` attached to the `Rejected`. Extraction goes into a
  `.tardrop-*` tempdir *inside* `~/Applications` so the final publish is a same-filesystem
  `fs::rename`. Launcher selection is a scoring pass (`executable_candidates` / `candidate_penalty`)
  over root `AppRun`, `Exec=` targets from nearby desktop files, launcher scripts, and root
  executables; when the top two scores are within 10 points it returns
  `InstallResult::NeedsLauncherChoice` instead of guessing, and the UI asks the user.
- `security` — archive SHA-256, ELF magic check (`is_elf`; scripts are never launchers by design),
  `safe_desktop_value` for desktop-entry injection, and the risk tiers (`RiskAssessment::of`) plus the
  `Decision` → `Policy` mapping the override prompt uses.
- `desktop` — reads package `.desktop` metadata and writes the per-user launcher, then
  `refresh_integrations()`.
- `icons` — picks a PNG/SVG/XPM icon without following links; icons are *copied* into XDG data so
  they survive replacement of the install directory.
- `utils` — the only place that resolves `~/Applications` and the XDG dir, plus `sanitize_name`,
  `desktop_id`, and `direct_child` (which enforces that a computed path is an immediate child of the
  owned root — uninstall relies on this).
- `updates` — JSON record database at `~/.local/share/tardrop/installed-apps.json`, atomically
  replaced via write-temp-then-rename. Update sources are behind the `UpdateProvider` trait
  (`check_latest` / `download_latest`); adding a provider means adding a `ProviderKind` variant and
  an impl, not touching `installer`. Updates move the old install to a rollback location, reuse the
  normal validated install path, and restore on any failure. Version detection never executes the
  binary with `--version`.
- `ui` — ~1100 lines, the largest module. GTK owns the main loop and all widgets are main-thread
  only; installer and network work runs in `std::thread` workers that report back over `mpsc`, and a
  single GLib timeout (`tick`) drains those channels. Worker code therefore stays free of GTK types
  — do not pass widgets or `Rc<App>` into a thread. Shared UI state is `Rc<App>` with a
  `RefCell<State>`; blocking decisions (existing install, launcher choice, override) use
  `AdwAlertDialog` and pause the install queue via `hold_queue`, which settles on the dialog's
  `response` signal. Never use `closed` for that: libadwaita emits it *before* `response` when a
  button is clicked, so the queue would be released before the answer starts its work.

## Conventions

- Doc comments (`///`, `//!`) are on essentially every item and explain **why** a choice was made,
  especially the security rationale. Match this when adding code.
- The Rust style here is unusually dense: struct fields and short functions on one line, `use`
  blocks collapsed with braces. Follow the surrounding file rather than reformatting it.
- No colours or spacing are hardcoded in the UI; libadwaita style classes supply them.
- Strictness over convenience is intentional. Archives that ordinary tools accept are rejected here
  because the input is assumed untrusted — do not relax a validation to make an archive work.
  The one sanctioned escape hatch is user-initiated: content-check failures are raised as
  `security::Rejected`, and the UI offers a retry with `Policy::Override`. Survey refusals (links,
  `install.sh`) carry `Rejected::risk`, which the prompt shows as a level plus reasons. Traversal/absolute paths
  stay unwritable even then. New checks should return `Rejected` so they participate in this flow.

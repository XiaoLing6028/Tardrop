# TarDrop

TarDrop is a KDE-friendly, user-local installer for portable application archives. Drop a `.tar`, `.tar.gz`, `.tgz`, `.tar.xz`, `.tar.bz2`, or `.zip` archive onto its window. It extracts the archive beneath `~/Applications`, finds a native ELF application, and writes a per-user launcher to `~/.local/share/applications`.

## Safety model

TarDrop does not use `tar`, `unzip`, a shell, or `sudo`. Archives are extracted into a private staging directory first. Every member path is checked, and absolute paths, `..`, symlinks, hard links, device files, and overwrite attempts are rejected. Only regular files and directories are accepted. It never runs archive scripts; `install.sh` is not a launch candidate. A launcher is made only for an ELF binary, and is never automatically launched.

Existing installations are either replaced, given a separate numbered directory, or cancelled. Uninstall checks that it is deleting only an immediate child of `~/Applications` and the matching XDG launcher.

## Requirements

* GNU/Linux on x86_64
* Rust stable (edition 2024)
* A working Wayland or X11 session
* GTK 4 and libadwaita development files (`gtk4-devel libadwaita-devel` on Fedora,
  `libgtk-4-dev libadwaita-1-dev` on Debian/Ubuntu)
* A desktop opener (`xdg-open`) for the optional **Open folder** button
* `update-desktop-database` is optional; Plasma also discovers the user launcher directory directly

On Fedora you can install the build dependencies with:

```bash
sudo dnf install rust cargo gtk4-devel libadwaita-devel
```

GTK 4 with libadwaita is used for the GUI because it provides real native widgets, the system file dialog, and Wayland/X11 drag-and-drop from one desktop build. The window follows GNOME's human interface guidelines — an adaptive navigation split view, boxed lists, toasts, and `AdwAlertDialog` — and takes its light/dark preference and accent colour from the desktop rather than hardcoding a palette. KDE uses the normal XDG desktop-entry location, so installed apps appear in Application Launcher, Kickoff, and KRunner.

## Build and run

```bash
cargo build --release
./target/release/tardrop
```

For development, use `cargo run`. Cargo downloads the Rust dependencies on the first build. The GTK 4 and libadwaita development packages must be installed first, because the bindings link against the system libraries.

## Architecture

* `archive`: identifies formats and performs streaming, validated extraction.
* `security`: hashes archives, identifies ELF binaries, and validates desktop fields.
* `installer`: owns the extract → inspect → publish transaction and uninstall rules.
* `icons`: chooses a likely PNG, SVG, or XPM icon without following links.
* `desktop`: writes the per-user launcher and asks the desktop database to refresh.
* `ui`: drag-and-drop, queueing, dialogs, progress display, log, launch, and uninstall controls.
* `utils`: bounded user-directory and naming helpers.
* `updates`: JSON-backed installed-app records, update preferences, providers, and rollback-safe updates.

## Updates

TarDrop records each successful install in `~/.local/share/tardrop/installed-apps.json`, including its install and desktop paths, detected version, archive name, provider configuration, and check timestamps. The database is human-readable and atomically replaced on changes.

The built-in providers are GitHub Releases, Static URL, Website endpoint, and Manual. New providers implement the small `UpdateProvider` trait, without changing installer code. Applications installed from a local archive default to **Manual** because TarDrop never invents a network source. Provider configuration is stored in each record’s `source_url` and `custom_metadata`; this makes it possible to import or manage sources without trusting archive content.

An update is only downloaded after the user presses **Update**. TarDrop moves the existing application to a private rollback location, invokes the normal archive validation/install path, preserves the established launcher, icon, and root permissions, and restores the old application if validation or installation fails. For safety, version detection does not automatically execute `--version`; it uses desktop metadata, version files, archive filenames, and provider release metadata.

## Notes

TarDrop intentionally rejects archives that contain symlinks, hardlinks, or special files. This is stricter than ordinary archive tools because the product’s job is safely handling untrusted downloads. Launcher discovery scores root `AppRun`, safe `Exec=` targets from nearby desktop files, conventional launcher scripts, root executables, and name matches. Dependency, documentation, and known helper-binary trees are heavily penalized. When the two top candidates are within 10 points, TarDrop shows a chooser instead of guessing.

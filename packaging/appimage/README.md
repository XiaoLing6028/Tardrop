# TarDrop AppImage

`build.sh` produces `dist/TarDrop-<version>-x86_64.AppImage` and its `.sha256` checksum file:
a single executable that runs TarDrop on x86_64 Linux distributions whose glibc is 2.39 or newer,
without GTK 4 or libadwaita installed on the host.

```bash
packaging/appimage/build.sh          # build image (first run only), unit tests, AppImage
packaging/appimage/smoke-test.sh     # static checks + end-to-end runs on several distributions
```

## Building

### Host requirements

* x86_64 Linux with **Docker 23+** (BuildKit, the default) or **Podman 4+**, plus `bash` and `git`.
  Nothing else is installed on the host: the toolchain, GTK, and the AppImage tools live in the
  build image.
* About 6 GB of disk for the build image and caches. The first run builds the image (around
  10 minutes: apt, then GTK and libadwaita from source); later runs reuse it and take a few
  minutes, most of it the Rust release build with LTO.
* HTTPS access to `snapshot.ubuntu.com`, `download.gnome.org`, `gitlab.freedesktop.org`,
  `static.rust-lang.org`, `github.com` (release downloads), and `index.crates.io`/`static.crates.io`.
  The host's CA bundle is handed to the container (as a BuildKit secret during the image build,
  never stored in a layer), so builds also work behind a TLS-inspecting proxy. Override the
  bundle with `TARDROP_CA_BUNDLE=/path/to/bundle.pem`; pick the engine with
  `CONTAINER_ENGINE=podman`.

### What `build.sh` does

1. Builds the build image from `Containerfile`, tagged with a hash of its inputs, so it is rebuilt
   only when a pin or an image script changes:
   * Ubuntu 24.04, pinned by digest, with every package from the dated archive snapshot
     `UBUNTU_SNAPSHOT` (apt verifies it with the Ubuntu archive key, as for the live archive);
   * GTK 4.16.13 and libadwaita 1.6.10 built from upstream tarballs into `/opt/gtk`;
   * Rust from the official standalone component tarballs;
   * linuxdeploy, appimagetool, and the type 2 runtime, by release tag.

   Every download is checked against the SHA-256 in `pins.env` and the build stops on a mismatch.
   The runtime, the only downloaded code that ends up inside the AppImage, must also carry a
   valid signature by the type2-runtime release key (`keys/type2-runtime.asc`).
2. Runs `package.sh` in a fresh container with the checkout mounted read-only. It copies the
   source to an empty directory (no `target/` or `dist/` from a local build takes part) and:
   * runs `cargo test` and the `--ignored` window test (under Xvfb and a D-Bus session) against
     the GTK and libadwaita that are about to be bundled; `--skip-tests` skips this;
   * builds the release binary with `--locked`;
   * assembles the AppDir: the binary, `data/` desktop entry, AppStream metadata, and icons (SVG,
     plus a 256×256 PNG rendered from it), the Adwaita icon theme without cursors, GTK's compiled
     GSettings schemas, the gdk-pixbuf SVG loader, the shared-mime-info database, the dconf GIO
     module, and `AppRun`;
   * validates the desktop entry (`desktop-file-validate`) and metadata (`appstreamcli`);
   * runs linuxdeploy, which bundles the shared-library closure and rewrites every RUNPATH to an
     `$ORIGIN`-relative one, skipping the libraries the host must provide;
   * sets every timestamp to `SOURCE_DATE_EPOCH` (the last commit's time by default) and runs
     appimagetool with the pinned runtime.

The build is reproducible: building the same commit twice gives a bit-identical AppImage.

## What is bundled, and what comes from the host

| Bundled (from the Ubuntu 24.04 snapshot unless noted) | Host |
| --- | --- |
| GTK 4.16.13 and libadwaita 1.6.10 (built from source), GLib 2.80, Pango 1.52, fribidi, cairo, gdk-pixbuf with librsvg's SVG loader, graphene, epoxy, xkbcommon, X11 extension libraries (Xi, Xrandr, Xcursor, …), libwayland-egl, libappstream and its dependencies (libcurl, GnuTLS, Kerberos with com_err, ICU, libxml2, …), dconf's GIO module, the Adwaita and hicolor icon themes, GTK's GSettings schemas, the MIME database | glibc (≥ 2.39), libstdc++, libgcc_s, OpenGL and EGL (libglvnd or Mesa; OpenGL ES is used when present), libdrm/gbm, libX11, libxcb, libwayland-client, fontconfig, freetype, harfbuzz, zlib, expat, libuuid, libgpg-error, libgmp, and the fonts, cursors, CA certificates, and desktop portals themselves |

The host side follows the AppImage project's exclude list: those libraries are tied to the
graphics driver, the display server, or the C library, and are present on every desktop. Two
listed libraries are bundled anyway, because their only users in TarDrop's process are bundled
and minimal installs (and the Debian and openSUSE container images) lack them: fribidi, which the
list excludes because programs that also load the host's text-rendering plugins can end up with
two copies (TarDrop loads none), and com_err, used only by the Kerberos libraries that
libappstream's libcurl brings along. The smoke test's `minimal` target proves, from the running
process's memory map, that the GTK stack is loaded from the image even where the host has its own
copy of a library.

GTK is built without Vulkan (it renders with OpenGL, or cairo without OpenGL, so the host needs
no `libvulkan`), and without the media and print backends, which TarDrop never uses.

## Runtime behaviour

`AppRun` changes only what GTK needs to find the bundled files, and records each change:

| Variable | Why |
| --- | --- |
| `XDG_DATA_DIRS` (prepended) | the bundled icon themes (TarDrop pins Adwaita), GSettings schemas, and MIME database: gdk-pixbuf picks an image loader by the MIME type GIO guesses, so without a MIME database (shared-mime-info) no SVG icon loads |
| `GDK_PIXBUF_MODULE_FILE` | the bundled SVG loader instead of the host's loaders, which are built for the host's gdk-pixbuf |
| `GIO_MODULE_DIR` | the bundled dconf module instead of the host's GIO modules, which are built for the host's GLib |
| `GDK_DISABLE=gles-api` (only when the host has no `libGLESv2.so.2`) | GTK 4.16 renders with OpenGL ES first, and libepoxy aborts the process when `libGLESv2` is missing instead of letting GTK fall back; Debian and Ubuntu package it separately (`libgles2`). Without it, GTK uses desktop OpenGL |

It does **not** set `GDK_BACKEND` or `GTK_THEME` (common in GTK AppImages), so TarDrop keeps native
Wayland, including drag and drop, and follows the desktop's light/dark preference and accent
colour exactly as a native build.

Programs TarDrop starts must not inherit any of this: `xdg-open`, `update-desktop-database`,
`gtk-update-icon-cache`, `kbuildsycoca6`, and the applications it installs would otherwise load
TarDrop's private libraries and data, KDE would index TarDrop's internal launcher from a mount
that disappears, and an installed Electron app's updater could take TarDrop's `$APPIMAGE` for its
own. `AppRun` therefore saves each previous value as `TARDROP_HOST_<NAME>` and lists the changed
names (plus `APPIMAGE`, `APPDIR`, `ARGV0`, `OWD`, set by the runtime) in `TARDROP_HOST_ENV`;
`src/host.rs` restores them for every child process. Outside the AppImage that variable is never
set and `host::command` is a plain `std::process::Command`.

All security checks are unchanged; the AppImage runs the same code as a native build.

## Testing

`smoke-test.sh [APPIMAGE] [TARGET...]` writes reports to `target/appimage-smoke/`:

| Target | What it proves |
| --- | --- |
| `static` | file name, x86-64 ELF, executable bit, AppImage type 2 magic, checksum, `--appimage-extract`, desktop entry and AppStream validation, PNG `.DirIcon`, bundled GTK/libadwaita/schemas/SVG loader/dconf/icons (every icon name in `src/` present), every RUNPATH `$ORIGIN`-relative, every `NEEDED` library bundled or on the host list, nothing host-only bundled, newest glibc symbol ≤ 2.39, no build path in any text file; prints each bundled library's package and version |
| `fedora`, `debian`, `ubuntu`, `arch`, `opensuse` | TarDrop driven end to end through its accessibility tree on a host **without** GTK 4 or libadwaita (below) |
| `native` | the same scenario against `cargo build --release` on Fedora's own GTK and libadwaita; `compare.py` then requires identical check results, identical files written (paths, modes, contents, desktop entries, database records), and an identical technical log |
| `wayland` | the scenario under a headless Weston, with no X server at all |
| `fuse` | the AppImage mounted through FUSE by its runtime, instead of extracted |
| `minimal` | Ubuntu 24.04 with only the host libraries: no GTK, libadwaita, Python, MIME database, or OpenGL ES. Every bundled ELF file resolves its libraries; the window opens with every icon loaded (GTK falls back to desktop OpenGL); and `/proc/PID/maps` of the running TarDrop shows GTK, libadwaita, GLib, and the rest of their stack mapped from the image, never from the host (whose own GLib, pulled in by harfbuzz, is ignored). All of it three times: as built, as a renamed copy in a directory with spaces, and as an extracted AppDir moved elsewhere |
| `old-glibc` | Ubuntu 22.04 (glibc 2.35) refuses to start it with the loader's "GLIBC_2.3x not found" |

The end-to-end scenario (`tests/drive.py`) uses a stand-in for the file chooser portal
(`tests/mock-portal.py`), which GTK's `GtkFileDialog` talks to exactly as under GNOME or KDE, and
stubs for the desktop tools that record their arguments and environment. It installs a clean
`.tar.gz` (launcher, desktop entry, icon, database record), a `.tar.xz`, a `.zip`, replaces the
ZIP after the "already installed" prompt, refuses an unsupported file, answers the launcher
chooser, refuses a High-risk archive (an `install.sh` and an escaping symlink: nothing is written
and the script never runs), trusts a path-traversal archive (`../escape.txt` must still never be
written), checks for an update with no source configured, uninstalls, starts an installed
application and opens its folder (both must see the session's own environment, not the
AppImage's), and checks that no staging directory is ever left behind.

Limits of these tests: the distributions are containers sharing this machine's kernel, with
Xvfb or headless Weston and software rendering, not real desktop sessions on real hardware;
the portal is a stand-in; and AT-SPI can synthesize pointer and key events only on X11, so under
Wayland the steps that need a click (expanding rows, the launcher chooser, the log) are reported
as skipped, never as passed.

## Known limitations

* **glibc 2.39 or newer**: Ubuntu 24.04, Debian 13, Fedora 40, RHEL 10, openSUSE Leap 16, and
  later. Ubuntu 22.04, Debian 12, and RHEL 9 cannot run it. TarDrop needs libadwaita 1.6, which
  needs GTK ≥ 4.15; supporting older systems would mean rebuilding GLib, Pango, and harfbuzz too.
* **x86_64 only.**
* **Bundled libraries are frozen at build time.** Security fixes to GTK, GLib, librsvg, and the
  rest reach users only through a rebuilt AppImage: move `UBUNTU_SNAPSHOT` forward and rebuild
  for each release.
* **GTK's own translations are not loaded**, because GTK looks for them at its compile-time
  prefix. TarDrop's interface is English anyway; this affects only GTK's built-in file chooser
  (used when no portal is running).
* **Input-method modules on X11.** GTK input-method modules from the host (ibus or fcitx for
  GTK 4) are built for the host's GTK and are not loaded. Under Wayland, GTK uses the
  compositor's text-input protocol, which needs no module.
* **FUSE**: the runtime is static and needs no `libfuse`, but mounting as an ordinary user needs
  the `fusermount3` (or `fusermount`) helper, installed with FUSE 3 on every mainstream desktop.
  Without it, run `./TarDrop-*.AppImage --appimage-extract-and-run`.
* **Size** (about 35 MB): libadwaita 1.6 links libappstream, which brings libcurl, GnuTLS,
  Kerberos, ICU, and libxml2 along. They are loaded but never used by TarDrop.
* The AppImage contains no update information (AppImageUpdate) and is not signed.
* TarDrop itself does not add a menu entry for the AppImage; use AppImageLauncher or Gear Lever,
  or add one by hand.

## Updating the pins

Change a version and its checksum together in `pins.env`, after checking the new file:

* **Ubuntu snapshot**: any `YYYYMMDDTHHMMSSZ` timestamp; apt checks signatures itself.
* **Base image**: `docker pull ubuntu:24.04` and use the digest it reports.
* **GTK, libadwaita**: the `.sha256sum` next to each tarball on `download.gnome.org`. Moving past
  GTK 4.16 needs a newer Pango and harfbuzz than Ubuntu 24.04 has.
* **wayland-protocols**: verify the `.sig` beside the tarball with the releaser's key.
* **Rust**: `channel-rust-stable.toml` and its `.asc`, verified with the Rust release key
  (`108F 6620 5EAE B0AA A8DD 5E1C 85AB 96E6 FA1B E5FE`); its `xz_hash` values are the pins.
* **linuxdeploy**: its release `.zsync` lists the file's SHA-1; compare, then pin the SHA-256.
* **appimagetool**: publishes no digest; pin the SHA-256 of a release tag you have reviewed.
* **Runtime**: `gpg --verify runtime-x86_64.sig runtime-x86_64` with `keys/type2-runtime.asc`
  (`570C 77AC EA40 C0F1 B758 902C BF96 CCA5 6490 F695`); the build repeats this check every time.

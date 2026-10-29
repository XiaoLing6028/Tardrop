#!/bin/bash
# Runs inside the build image, started by build.sh. Tests TarDrop against the GTK and libadwaita it
# ships with, builds the release binary, assembles the AppDir, and writes
# /out/TarDrop-<version>-x86_64.AppImage with its .sha256 checksum file.
set -euo pipefail

packaging=/src/packaging/appimage
work=/work
appdir=$work/AppDir
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:?set by build.sh}

echo "==> Copying the source tree to a clean build directory"
mkdir -p "$work/src"
tar -C /src --exclude=./target --exclude=./dist --exclude=./.git -cf - . | tar -C "$work/src" -xf -
cd "$work/src"
version=$(cargo metadata --no-deps --format-version 1 | python3 -I -c 'import json, sys; print(json.load(sys.stdin)["packages"][0]["version"])')
artifact=TarDrop-$version-x86_64.AppImage

echo "==> Fetching crates pinned by Cargo.lock"
cargo fetch --locked

if [[ ${TARDROP_SKIP_TESTS:-0} != 1 ]]; then
    echo "==> Running the test suite against GTK $(pkg-config --modversion gtk4) and libadwaita $(pkg-config --modversion libadwaita-1)"
    cargo test --locked --offline
    # The window test needs a display and a session bus; dbus wants a machine id to start one.
    dbus-uuidgen --ensure
    xvfb-run --auto-servernum --server-args="-screen 0 1280x800x24" dbus-run-session -- cargo test --locked --offline -- --ignored
fi

echo "==> Building TarDrop $version"
cargo build --release --locked --offline

echo "==> Assembling the AppDir"
install -Dm755 target/release/tardrop "$appdir/usr/bin/tardrop"
install -Dm644 data/org.tardrop.TarDrop.desktop "$appdir/usr/share/applications/org.tardrop.TarDrop.desktop"
install -Dm644 data/org.tardrop.TarDrop.metainfo.xml "$appdir/usr/share/metainfo/org.tardrop.TarDrop.metainfo.xml"
install -Dm644 data/icons/hicolor/scalable/apps/org.tardrop.TarDrop.svg "$appdir/usr/share/icons/hicolor/scalable/apps/org.tardrop.TarDrop.svg"
mkdir -p "$appdir/usr/share/icons/hicolor/256x256/apps"
rsvg-convert --width 256 --height 256 data/icons/hicolor/scalable/apps/org.tardrop.TarDrop.svg --output "$appdir/usr/share/icons/hicolor/256x256/apps/org.tardrop.TarDrop.png"
desktop-file-validate "$appdir/usr/share/applications/org.tardrop.TarDrop.desktop"
appstreamcli validate --no-net "$appdir/usr/share/metainfo/org.tardrop.TarDrop.metainfo.xml"

# Icon themes. TarDrop pins Adwaita (ui.rs), which desktops other than GNOME may not install, and
# hicolor is the theme every lookup falls back to. Cursors stay the host's.
install -Dm644 /usr/share/icons/hicolor/index.theme "$appdir/usr/share/icons/hicolor/index.theme"
mkdir -p "$appdir/usr/share/icons/Adwaita"
tar -C /usr/share/icons/Adwaita --exclude=./cursors --exclude=./cursor.theme --exclude=./icon-theme.cache -cf - . | tar -C "$appdir/usr/share/icons/Adwaita" -xf -

# GTK's own settings schemas: its built-in file chooser, used when no desktop portal answers,
# aborts without them, and hosts without GTK 4 do not have them.
mkdir -p "$appdir/usr/share/glib-2.0/schemas"
cp /opt/gtk/share/glib-2.0/schemas/org.gtk.gtk4.*.gschema.xml "$appdir/usr/share/glib-2.0/schemas/"
glib-compile-schemas --strict "$appdir/usr/share/glib-2.0/schemas"

# The MIME database. gdk-pixbuf picks an image loader by the MIME type GIO guesses, and without
# shared-mime-info on the host it cannot tell an SVG icon from any other file.
install -Dm644 /usr/share/mime/mime.cache "$appdir/usr/share/mime/mime.cache"

# The SVG loader GTK 4.16 renders symbolic icons with. Its loaders.cache entry names the module
# without a directory, because the mount point is only known at run time: gdk-pixbuf passes the
# bare name to dlopen, which searches the RUNPATH ($ORIGIN) of the bundled libgmodule, i.e.
# usr/lib, where linuxdeploy places the loader below.
pixbuf_loaders=$(pkg-config --variable=gdk_pixbuf_moduledir gdk-pixbuf-2.0)
svg_loader=$pixbuf_loaders/libpixbufloader-svg.so
mkdir -p "$appdir/usr/lib/gdk-pixbuf-2.0/2.10.0"
"$(pkg-config --variable=gdk_pixbuf_query_loaders gdk-pixbuf-2.0)" "$svg_loader" | grep -v '^#' | sed "s|\"$pixbuf_loaders/|\"|" > "$appdir/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache"
grep -q '^"libpixbufloader-svg.so"' "$appdir/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache"

# dconf, GLib's GSettings backend on Linux desktops, so GTK's own preferences persist exactly as
# with a native build. AppRun points GIO_MODULE_DIR here so no host GIO module is ever loaded.
install -Dm644 "$(pkg-config --variable=giomoduledir gio-2.0)/libdconfsettings.so" "$appdir/usr/lib/gio/modules/libdconfsettings.so"

echo "==> Bundling shared libraries"
# linuxdeploy copies every dependency into usr/lib and rewrites RUNPATHs to $ORIGIN-relative ones,
# except the libraries on the AppImage exclude list (glibc, libstdc++, OpenGL/EGL, X11/xcb,
# Wayland client, fontconfig, freetype, harfbuzz, zlib, …), which must come from the host
# because they are tied to its drivers, display server, or C library.
linuxdeploy --appdir "$appdir" \
    --executable "$appdir/usr/bin/tardrop" \
    --desktop-file "$appdir/usr/share/applications/org.tardrop.TarDrop.desktop" \
    --icon-file "$appdir/usr/share/icons/hicolor/256x256/apps/org.tardrop.TarDrop.png" \
    --custom-apprun "$packaging/AppRun" \
    --library "$svg_loader" \
    --deploy-deps-only "$appdir/usr/lib/gio/modules"

# Two libraries on the exclude list whose only users in TarDrop's process are bundled, so they
# are bundled too rather than required from the host, where minimal installs may lack them:
# fribidi (excluded because programs that also load the host's text-rendering plugins can end up
# with two copies; TarDrop loads none, Pango is bundled) and com_err (assumed to be part of every
# base system, but used here only by the Kerberos libraries libappstream's libcurl brings along).
# Both depend on nothing but libc.
for library in libfribidi.so.0 libcom_err.so.2; do
    install -m644 "$(readlink -f "/usr/lib/x86_64-linux-gnu/$library")" "$appdir/usr/lib/$library"
done

# linuxdeploy links the image's own icon to the SVG once one is installed, but .DirIcon, which file
# managers and thumbnailers read, is expected to be a PNG. It also leaves empty size directories.
rm -f "$appdir/.DirIcon" "$appdir/org.tardrop.TarDrop.svg"
ln -s usr/share/icons/hicolor/256x256/apps/org.tardrop.TarDrop.png "$appdir/org.tardrop.TarDrop.png"
ln -s org.tardrop.TarDrop.png "$appdir/.DirIcon"
find "$appdir/usr/share/icons/hicolor" -type d -empty -delete

# Caches are written last, after every icon is in place.
gtk4-update-icon-cache --quiet --force --ignore-theme-index "$appdir/usr/share/icons/hicolor"
gtk4-update-icon-cache --quiet --force "$appdir/usr/share/icons/Adwaita"
# Equal timestamps everywhere make the image reproducible, and keep the icon caches valid: GTK
# ignores a cache older than its directory.
find "$appdir" -exec touch --no-dereference --date="@$SOURCE_DATE_EPOCH" {} +

echo "==> Writing $artifact"
ARCH=x86_64 appimagetool --no-appstream --runtime-file /opt/appimage/runtime-x86_64 "$appdir" "$work/$artifact"
cd "$work"
sha256sum "$artifact" > "$artifact.sha256"
install -m755 "$artifact" "/out/$artifact"
install -m644 "$artifact.sha256" "/out/$artifact.sha256"
if [[ -n ${HOST_OWNER:-} ]]; then chown "$HOST_OWNER" "/out/$artifact" "/out/$artifact.sha256"; fi
echo "==> Done: dist/$artifact ($(du -h "/out/$artifact" | cut -f1))"
cat "/out/$artifact.sha256"

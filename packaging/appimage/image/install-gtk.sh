#!/bin/bash
# Builds wayland-protocols, GTK, and libadwaita from their pinned release tarballs into /opt/gtk.
#
# /opt/gtk is deliberately a prefix no host uses: GTK and GLib compile their module and data
# directories in, and inside the AppImage those must never find a host's GTK modules, which are
# built for a different GTK and may rely on its private ABI.
set -euo pipefail
source /opt/tardrop/pins.env
source /opt/tardrop/image/fetch.sh

prefix=/opt/gtk
work=$(mktemp -d)
cd "$work"

fetch "https://gitlab.freedesktop.org/wayland/wayland-protocols/-/releases/$WAYLAND_PROTOCOLS_VERSION/downloads/wayland-protocols-$WAYLAND_PROTOCOLS_VERSION.tar.xz" "$WAYLAND_PROTOCOLS_SHA256" wayland-protocols.tar.xz
fetch "https://download.gnome.org/sources/gtk/${GTK_VERSION%.*}/gtk-$GTK_VERSION.tar.xz" "$GTK_SHA256" gtk.tar.xz
fetch "https://download.gnome.org/sources/libadwaita/${LIBADWAITA_VERSION%.*}/libadwaita-$LIBADWAITA_VERSION.tar.xz" "$LIBADWAITA_SHA256" libadwaita.tar.xz
for tarball in wayland-protocols gtk libadwaita; do mkdir "$tarball" && tar -xJf "$tarball.tar.xz" -C "$tarball" --strip-components=1; done

# Ubuntu's own hardened compiler flags, as its GTK packages use, without LTO or debug info.
eval "$(DEB_BUILD_MAINT_OPTIONS='hardening=+all optimize=-lto' DEB_CFLAGS_MAINT_STRIP=-g DEB_CXXFLAGS_MAINT_STRIP=-g dpkg-buildflags --export=sh)"
export PKG_CONFIG_PATH="$prefix/lib/pkgconfig:$prefix/share/pkgconfig"
# nodownload: a missing dependency fails the build rather than pulling an unpinned subproject.
setup=(meson setup --prefix="$prefix" --libdir=lib --buildtype=plain --wrap-mode=nodownload)

"${setup[@]}" build-wayland-protocols wayland-protocols -Dtests=false
meson install -C build-wayland-protocols

# TarDrop plays no media and prints nothing, so those backends are not built. Vulkan is off so the
# AppImage does not require libvulkan on the host; GTK renders with OpenGL, or with cairo when
# OpenGL is unavailable.
"${setup[@]}" build-gtk gtk \
    -Dx11-backend=true -Dwayland-backend=true -Dbroadway-backend=false \
    -Dmedia-gstreamer=disabled -Dprint-cups=disabled -Dprint-cpdb=disabled -Dvulkan=disabled \
    -Dcloudproviders=disabled -Dsysprof=disabled -Dtracker=disabled -Dcolord=disabled \
    -Dintrospection=disabled -Ddocumentation=false -Dscreenshots=false -Dman-pages=false \
    -Dbuild-demos=false -Dbuild-testsuite=false -Dbuild-examples=false -Dbuild-tests=false
meson install -C build-gtk

"${setup[@]}" build-libadwaita libadwaita -Dintrospection=disabled -Dvapi=false -Dgtk_doc=false -Dtests=false -Dexamples=false
meson install -C build-libadwaita

echo "$prefix/lib" > /etc/ld.so.conf.d/tardrop-gtk.conf
ldconfig
cd /
rm -rf "$work"

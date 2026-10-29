#!/bin/bash
# Runs the AppImage on a host that has only the libraries the AppImage leaves to it, and proves
# from /proc/PID/maps of the running TarDrop where every loaded library came from: GTK,
# libadwaita, GLib, and the rest of their stack must be mapped from the image, never from the
# host, even where the host has its own copy. (glibc loads one copy per soname: a host library such
# as harfbuzz that is loaded early can bring in its own copy of a small dependency the image also
# carries, like graphite2; those are listed.) It does so three times, which also covers
# relocation: as built, as a renamed copy in a directory with spaces, and as an extracted AppDir
# moved somewhere else.
#
#   host-libraries.sh APPIMAGE
set -uo pipefail

appimage=$1
failures=0
pass() { echo "PASS $*"; }
fail() { echo "FAIL $*"; failures=$((failures + 1)); }

export HOME=/tmp/home APPIMAGE_EXTRACT_AND_RUN=1 DISPLAY=:5
mkdir -p "$HOME"
Xvfb :5 -screen 0 1024x768x24 -nolisten tcp > /dev/null 2>&1 &
for _ in $(seq 100); do [[ -S /tmp/.X11-unix/X5 ]] && break; sleep 0.1; done

work=$(mktemp -d)
cd "$work" || exit 1
"$appimage" --appimage-extract > /dev/null
bundled=$(cd squashfs-root/usr/lib && ls)
[[ -d /usr/share/mime ]] && echo "     host has a MIME database" || echo "     host has no MIME database"
unresolved=$(find squashfs-root -type f \( -name '*.so*' -o -path '*/usr/bin/*' \) -exec ldd {} + 2> /dev/null | grep 'not found' | sort -u)
[[ -z $unresolved ]] && pass "every bundled ELF file resolves all its libraries on this host" || fail "unresolved: $unresolved"
for library in libglib-2.0.so.0 libgtk-4.so.1 libadwaita-1.so.0 libGLESv2.so.2; do
    if ldconfig -p | grep -q "$library"; then echo "     host has its own $library"; else echo "     host has no $library"; fi
done

# Must come from the image: what TarDrop's interface is built on.
core="libgtk-4.so.1 libadwaita-1.so.0 libglib-2.0.so.0 libgobject-2.0.so.0 libgio-2.0.so.0 libgmodule-2.0.so.0
libpango-1.0.so.0 libpangocairo-1.0.so.0 libpangoft2-1.0.so.0 libcairo.so.2 libcairo-gobject.so.2 libgdk_pixbuf-2.0.so.0
librsvg-2.so.2 libpixbufloader-svg.so libepoxy.so.0 libgraphene-1.0.so.0 libfribidi.so.0 libxkbcommon.so.0 libappstream.so.5"

# The bundled soname a host file provides, if any: libbz2.so.1.0.4 provides libbz2.so.1.0.
provides() { for soname in $bundled; do [[ $1 == "$soname" || $1 == "$soname".* ]] && { echo "$soname"; return; }; done; }

tardrop_pid() { for comm in /proc/[0-9]*/comm; do [[ $(< "$comm") == tardrop ]] 2> /dev/null && { dir=${comm%/comm}; echo "${dir#/proc/}"; return; }; done; }

run() {
    local name=$1; shift
    "$@" > "/tmp/$name.log" 2>&1 &
    local launcher=$! pid=""
    for _ in $(seq 150); do
        pid=$(tardrop_pid)
        [[ -n $pid ]] && xwininfo -root -tree 2> /dev/null | grep -q '"TarDrop"' && break
        sleep 0.2
    done
    if [[ -z $pid ]] || ! xwininfo -root -tree | grep -q '"TarDrop"'; then
        fail "$name: no TarDrop window"; cat "/tmp/$name.log"; kill "$launcher" 2> /dev/null; return
    fi
    pass "$name: window opens"
    sleep 1
    if grep -q "Failed to load icon" "/tmp/$name.log"; then fail "$name: icons fail to load: $(grep -m1 'Failed to load icon' "/tmp/$name.log")"; else pass "$name: every icon loads"; fi
    echo "     $name: GDK_DISABLE=$(tr '\0' '\n' < "/proc/$pid/environ" | sed -n 's/^GDK_DISABLE=//p')"
    local image_root from_host=() shared=() leaked=() soname
    image_root=$(dirname "$(dirname "$(dirname "$(readlink "/proc/$pid/exe")")")")
    grep -q " $image_root/usr/lib/libgtk-4.so.1$" "/proc/$pid/maps" && pass "$name: GTK is loaded from the image at $image_root" || fail "$name: GTK not loaded from the image"
    while read -r path; do
        [[ $path == "$image_root"/* ]] && continue
        soname=$(provides "${path##*/}")
        if [[ -z $soname ]]; then from_host+=("${path##*/}")
        elif grep -qw -- "$soname" <<< "$core"; then leaked+=("$path")
        else shared+=("$soname"); fi
    done < <(awk '$6 ~ /\.so/ {print $6}' "/proc/$pid/maps" | sort -u)
    [[ ${#leaked[@]} == 0 ]] && pass "$name: no part of the GTK/GLib stack comes from the host" || fail "$name: host copies loaded: ${leaked[*]}"
    echo "     $name: host-provided: ${from_host[*]}"
    echo "     $name: also bundled, but the host's copy came in first with a host library: ${shared[*]:-none}"
    kill "$pid" "$launcher" 2> /dev/null
    wait "$launcher" 2> /dev/null
    for _ in $(seq 100); do [[ -z $(tardrop_pid) ]] && ! xwininfo -root -tree 2> /dev/null | grep -q '"TarDrop"' && break; sleep 0.1; done
}

run as-built "$appimage"
mkdir -p "/tmp/some dir"
cp "$appimage" "/tmp/some dir/Renamed TarDrop.AppImage"
run renamed-copy "/tmp/some dir/Renamed TarDrop.AppImage"
mkdir -p "/tmp/moved dir"
mv squashfs-root "/tmp/moved dir/TarDrop AppDir"
run moved-appdir "/tmp/moved dir/TarDrop AppDir/AppRun"

echo
if [[ $failures == 0 ]]; then echo "ALL HOST-LIBRARY CHECKS PASSED"; else echo "$failures HOST-LIBRARY CHECK(S) FAILED"; fi
exit $(( failures > 0 ))

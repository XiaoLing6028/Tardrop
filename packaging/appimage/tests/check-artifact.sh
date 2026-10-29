#!/bin/bash
# Static checks of a built AppImage: format, architecture, permissions, checksum, desktop
# integration, bundled resources, shared-library closure, and relocatability. Runs in the build
# image (smoke-test.sh starts it), which has readelf, desktop-file-validate, and appstreamcli.
#
#   check-artifact.sh APPIMAGE SOURCE_DIR
set -uo pipefail

appimage=$(readlink -f "$1")
source_dir=$(readlink -f "$2")
failures=0
pass() { echo "PASS $*"; }
fail() { echo "FAIL $*"; failures=$((failures + 1)); }
expect() { local what=$1; shift; if "$@" > /dev/null 2>&1; then pass "$what"; else fail "$what"; fi; }

echo "== Artifact"
name=$(basename "$appimage")
[[ $name =~ ^TarDrop-[0-9]+\.[0-9]+\.[0-9]+.*-x86_64\.AppImage$ ]] && pass "versioned file name: $name" || fail "file name $name"
expect "executable permission" test -x "$appimage"
file_type=$(file -b "$appimage")
[[ $file_type == *"ELF 64-bit LSB"*"x86-64"* ]] && pass "x86-64 ELF: $file_type" || fail "file type: $file_type"
[[ $(od -An -tx1 -j8 -N3 "$appimage" | tr -d ' ') == 414902 ]] && pass "AppImage type 2 magic (AI\\x02 at offset 8)" || fail "AppImage magic"
if [[ -f $appimage.sha256 ]]; then
    expect "checksum file verifies" bash -c "cd '$(dirname "$appimage")' && sha256sum --check --status '$name.sha256'"
else
    fail "no $name.sha256 next to the artifact"
fi
echo "     size: $(stat -c %s "$appimage") bytes ($(du -h "$appimage" | cut -f1))"

work=$(mktemp -d)
cd "$work" || exit 1
"$appimage" --appimage-extract > /dev/null || { fail "--appimage-extract"; exit 1; }
root=$work/squashfs-root
pass "--appimage-extract unpacks the image"

echo "== Desktop integration"
expect "AppRun is the reviewed launcher script" cmp "$root/AppRun" "$source_dir/packaging/appimage/AppRun"
expect "AppRun is executable" test -x "$root/AppRun"
desktop=$root/org.tardrop.TarDrop.desktop
expect "desktop entry at the image root" test -f "$desktop"
expect "desktop entry validates" desktop-file-validate "$desktop"
expect "desktop entry matches data/" cmp "$desktop" "$source_dir/data/org.tardrop.TarDrop.desktop"
expect "Exec=tardrop" grep -qx "Exec=tardrop" "$desktop"
expect "Icon=org.tardrop.TarDrop (the GTK application id)" grep -qx "Icon=org.tardrop.TarDrop" "$desktop"
dir_icon=$(file -bL "$root/.DirIcon")
[[ $dir_icon == "PNG image data, 256 x 256"* ]] && pass ".DirIcon is a 256x256 PNG" || fail ".DirIcon: $dir_icon"
expect "scalable icon in hicolor" test -f "$root/usr/share/icons/hicolor/scalable/apps/org.tardrop.TarDrop.svg"
expect "256x256 icon in hicolor" test -f "$root/usr/share/icons/hicolor/256x256/apps/org.tardrop.TarDrop.png"
expect "AppStream metadata validates" appstreamcli validate --no-net "$root/usr/share/metainfo/org.tardrop.TarDrop.metainfo.xml"

echo "== Bundled resources"
expect "TarDrop binary" test -x "$root/usr/bin/tardrop"
for library in libgtk-4.so.1 libadwaita-1.so.0 libfribidi.so.0 libcom_err.so.2 libglib-2.0.so.0 libgdk_pixbuf-2.0.so.0 libpango-1.0.so.0 libcairo.so.2 librsvg-2.so.2; do
    expect "bundles $library" test -f "$root/usr/lib/$library"
done
source "$source_dir/packaging/appimage/pins.env"
cache=$root/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache
expect "pixbuf loaders.cache names the SVG loader without a directory" grep -qx '"libpixbufloader-svg.so"' "$cache"
expect "the SVG loader sits in usr/lib, beside libgmodule" test -f "$root/usr/lib/libpixbufloader-svg.so"
expect "dconf GIO module" test -f "$root/usr/lib/gio/modules/libdconfsettings.so"
expect "MIME database (gdk-pixbuf recognises SVG through it)" test -s "$root/usr/share/mime/mime.cache"
expect "GTK's file chooser schema compiles in" gsettings --schemadir "$root/usr/share/glib-2.0/schemas" list-keys org.gtk.gtk4.Settings.FileChooser
expect "Adwaita icon theme index" test -f "$root/usr/share/icons/Adwaita/index.theme"
expect "Adwaita icon cache" test -f "$root/usr/share/icons/Adwaita/icon-theme.cache"
expect "hicolor icon cache" test -f "$root/usr/share/icons/hicolor/icon-theme.cache"
expect "no cursor theme (cursors stay the host's)" test ! -e "$root/usr/share/icons/Adwaita/cursors"
missing=()
for icon in $(grep -ohE '"[a-z0-9-]+-symbolic"' "$source_dir"/src/*.rs | tr -d '"' | sort -u); do
    [[ -n $(find "$root/usr/share/icons/Adwaita" -name "$icon.svg" -print -quit) ]] || missing+=("$icon")
done
[[ ${#missing[@]} == 0 ]] && pass "every symbolic icon TarDrop names is bundled" || fail "icons missing: ${missing[*]}"

echo "== Shared libraries"
# What the AppImage expects the host to provide: the AppImage exclude list (C library, C++
# runtime, graphics drivers, X11/xcb, the Wayland client, the font stack, and a few
# ubiquitous low-level libraries), except fribidi and com_err, which package.sh bundles.
host_libraries="ld-linux-x86-64.so.2 libc.so.6 libm.so.6 libdl.so.2 libpthread.so.0 librt.so.1 libresolv.so.2 libutil.so.1
libstdc++.so.6 libgcc_s.so.1 libGL.so.1 libEGL.so.1 libGLESv2.so.2 libGLX.so.0 libGLdispatch.so.0 libOpenGL.so.0 libdrm.so.2 libgbm.so.1
libX11.so.6 libX11-xcb.so.1 libxcb.so.1 libwayland-client.so.0 libfontconfig.so.1 libfreetype.so.6 libharfbuzz.so.0
libz.so.1 libexpat.so.1 libgpg-error.so.0 libuuid.so.1 libgmp.so.10 libasound.so.2"
elf_files=$(find "$root" -type f -exec sh -c 'head -c4 "$1" | grep -q "ELF" && echo "$1"' _ {} \;)
bad_runpath=() unresolved=() wrong_arch=() bundled_host=()
for elf in $elf_files; do
    relative=${elf#"$root"/}
    readelf -h "$elf" | grep -q "Advanced Micro Devices X86-64" || wrong_arch+=("$relative")
    runpath=$(readelf -d "$elf" | sed -n 's/.*(\(RUNPATH\|RPATH\)).*\[\(.*\)\]/\2/p')
    IFS=: read -ra entries <<< "$runpath"
    for entry in "${entries[@]}"; do [[ $entry == '$ORIGIN'* ]] || bad_runpath+=("$relative:$entry"); done
    for needed in $(readelf -d "$elf" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'); do
        [[ -f $root/usr/lib/$needed ]] && continue
        grep -qw -- "$needed" <<< "$host_libraries" || unresolved+=("$relative needs $needed")
    done
done
for library in $host_libraries; do [[ -e $root/usr/lib/$library ]] && bundled_host+=("$library"); done
echo "     $(wc -w <<< "$elf_files") ELF files, $(find "$root/usr/lib" -maxdepth 1 -name '*.so*' | wc -l) bundled libraries"
[[ ${#wrong_arch[@]} == 0 ]] && pass "every ELF file is x86-64" || fail "not x86-64: ${wrong_arch[*]}"
[[ ${#bad_runpath[@]} == 0 ]] && pass "every RUNPATH is \$ORIGIN-relative (relocatable)" || fail "absolute RUNPATH: ${bad_runpath[*]}"
[[ ${#unresolved[@]} == 0 ]] && pass "every NEEDED library is bundled or host-provided" || fail "unresolved: ${unresolved[*]}"
[[ ${#bundled_host[@]} == 0 ]] && pass "no host-provided library is bundled" || fail "bundled but must come from the host: ${bundled_host[*]}"
glibc=$(for elf in $elf_files; do readelf -V "$elf" 2> /dev/null; done | grep -oE 'GLIBC_[0-9]+\.[0-9]+' | sort -uV | tail -1)
glibcxx=$(for elf in $elf_files; do readelf -V "$elf" 2> /dev/null; done | grep -oE 'GLIBCXX_[0-9.]+' | sort -uV | tail -1)
[[ $(printf '%s\n' "$glibc" GLIBC_2.39 | sort -V | tail -1) == GLIBC_2.39 ]] && pass "needs at most glibc 2.39 (newest symbol: $glibc)" || fail "needs $glibc"
echo "     newest libstdc++ symbol: ${glibcxx:-none}"

echo "== Relocatability"
leaks=$(grep -rIlE '/opt/gtk|/work/|/opt/cargo|/root/' "$root" 2> /dev/null | sed "s|$root/||")
[[ -z $leaks ]] && pass "no build-machine path in any text file" || fail "build paths in: $leaks"
compiled_in=$(grep -lac '/opt/gtk' "$root"/usr/lib/*.so* 2> /dev/null | sed "s|$root/||" | tr '\n' ' ')
echo "     compiled-in /opt/gtk defaults (fallbacks no host has; AppRun overrides those that matter): ${compiled_in:-none}"

echo "== Bundled library versions (Ubuntu $(. /etc/os-release && echo "$VERSION_ID") snapshot)"
for path in "$root"/usr/lib/*.so*; do
    library=${path##*/}
    case $library in
        libgtk-4.so.1) package="GTK (from source)" version=$GTK_VERSION ;;
        libadwaita-1.so.0) package="libadwaita (from source)" version=$LIBADWAITA_VERSION ;;
        *)  # dpkg records a library under /lib or /usr/lib depending on the package (usrmerge).
            package=$(for directory in /usr/lib/x86_64-linux-gnu /lib/x86_64-linux-gnu "$(pkg-config --variable=gdk_pixbuf_moduledir gdk-pixbuf-2.0)"; do
                [[ -e $directory/$library ]] && dpkg -S "$directory/$library" "$(readlink -f "$directory/$library")" 2> /dev/null; done | head -1 | cut -d: -f1)
            version=$( [[ -n $package ]] && dpkg-query -W -f='${Version}' "$package" || echo unknown) ;;
    esac
    printf '     %-36s %-28s %s\n' "$library" "$package" "$version"
done

echo
if [[ $failures == 0 ]]; then echo "ALL STATIC CHECKS PASSED"; else echo "$failures STATIC CHECK(S) FAILED"; fi
rm -rf "$work"
exit $(( failures > 0 ))

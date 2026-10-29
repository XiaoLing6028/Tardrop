#!/usr/bin/env bash
# Tests a built AppImage: static checks, then TarDrop driven end to end on several distributions,
# and the same scenario on a native build to compare behaviour. See README.md in this directory.
#
#   packaging/appimage/smoke-test.sh [APPIMAGE] [DISTRO...]
#
# APPIMAGE defaults to the newest dist/TarDrop-*-x86_64.AppImage. DISTRO is any of: fedora debian
# ubuntu arch opensuse (default: all), plus "native" (the Fedora native-build comparison),
# "wayland" (Fedora, headless Weston), "fuse" (mounted through FUSE rather than extracted),
# "minimal" (Ubuntu with only the host libraries: where each loaded library comes from, and
# relocation), and "old-glibc" (Ubuntu 22.04, which must refuse to start).
# Reports land in target/appimage-smoke/. Container engine and CA bundle as for build.sh.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/../.." && pwd)
engine=${CONTAINER_ENGINE:-$(command -v docker || command -v podman || true)}
[[ -n $engine ]] || { echo "error: neither docker nor podman is installed" >&2; exit 1; }
ca_bundle=${TARDROP_CA_BUNDLE:-${SSL_CERT_FILE:-}}
if [[ -z $ca_bundle ]]; then
    for candidate in /etc/ssl/certs/ca-certificates.crt /etc/pki/tls/certs/ca-bundle.crt /etc/ssl/ca-bundle.pem /etc/ssl/cert.pem; do
        if [[ -r $candidate ]]; then ca_bundle=$candidate; break; fi
    done
fi

appimage=""
if [[ ${1:-} == *.AppImage ]]; then appimage=$(readlink -f "$1"); shift; fi
[[ -n $appimage ]] || appimage=$(ls -t "$repo"/dist/TarDrop-*-x86_64.AppImage 2> /dev/null | head -1)
[[ -f $appimage ]] || { echo "error: no AppImage; run build.sh first or pass its path" >&2; exit 1; }
targets=("$@")
[[ ${#targets[@]} -gt 0 ]] || targets=(static fedora native wayland fuse debian ubuntu arch opensuse minimal old-glibc)
results=$repo/target/appimage-smoke
mkdir -p "$results"
summary=()

# Test images: only what a desktop session provides (display, D-Bus, accessibility, OpenGL,
# fonts), never GTK or libadwaita, plus Python with GObject for the driver.
image_for() {
    local distro=$1 base install bundle
    case $distro in
        fedora) base=registry.fedoraproject.org/fedora:44 bundle=/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem
            install="dnf -y -q install --setopt=install_weak_deps=False xorg-x11-server-Xvfb dbus-daemon dbus-tools at-spi2-core gsettings-desktop-schemas dbus-broker python3-gobject-base gobject-introspection mesa-dri-drivers mesa-libEGL mesa-libGL google-noto-sans-fonts fontconfig freetype harfbuzz libwayland-client libX11 libX11-xcb libxcb ImageMagick weston fuse3 procps-ng file findutils && dnf clean all" ;;
        debian|ubuntu|minimal|old-glibc)
            # Docker Hub's official images, through Google's mirror (no anonymous rate limit).
            case $distro in debian) base=mirror.gcr.io/library/debian:13 ;; ubuntu|minimal) base=mirror.gcr.io/library/ubuntu:24.04 ;; old-glibc) base=mirror.gcr.io/library/ubuntu:22.04 ;; esac
            bundle=/etc/ssl/certs/ca-certificates.crt
            # What every desktop has and the AppImage leaves to the host (see check-artifact.sh).
            local host="libgl1 libegl1 libfontconfig1 libfreetype6 libharfbuzz0b libwayland-client0 libx11-6 libx11-xcb1 libxcb1 libexpat1 zlib1g libcom-err2 libgpg-error0 libgmp10 libuuid1 fonts-dejavu-core"
            local packages="$host libgles2 xvfb dbus at-spi2-core gsettings-desktop-schemas python3-gi gir1.2-atspi-2.0 libgl1-mesa-dri imagemagick x11-utils procps file"
            # minimal and old-glibc: an X server and the host libraries, nothing else; not even
            # OpenGL ES or a MIME database, which Debian and Ubuntu package separately.
            [[ $distro == minimal || $distro == old-glibc ]] && packages="$host xvfb x11-utils"
            install="apt-get update -qq && apt-get install -y -qq --no-install-recommends $packages > /dev/null && rm -rf /var/lib/apt/lists/*" ;;
        arch) base=registry.archlinux.org/archlinux/archlinux-docker:base-master bundle=/etc/ca-certificates/extracted/tls-ca-bundle.pem
            install="pacman -Syu --noconfirm --needed xorg-server-xvfb dbus at-spi2-core gsettings-desktop-schemas python-gobject mesa libglvnd fontconfig freetype2 harfbuzz wayland libx11 libxcb ttf-dejavu imagemagick procps-ng file > /dev/null && pacman -Scc --noconfirm > /dev/null" ;;
        opensuse) base=registry.opensuse.org/opensuse/tumbleweed:latest bundle=/var/lib/ca-certificates/ca-bundle.pem
            install="(zypper -q -n modifyrepo --disable repo-openh264 || true) && zypper -q -n install --no-recommends xorg-x11-server-Xvfb dbus-1 dbus-1-daemon at-spi2-core gsettings-desktop-schemas python3-gobject typelib-1_0-Atspi-2_0 Mesa-dri Mesa-libEGL1 Mesa-libGL1 fontconfig libfreetype6 libharfbuzz0 libwayland-client0 libX11-6 libX11-xcb1 libxcb1 dejavu-fonts ImageMagick procps file && zypper -q clean -a" ;;
    esac
    local tag=localhost/tardrop-appimage-test:$distro
    if ! "$engine" image inspect "$tag" > /dev/null 2>&1; then
        echo "==> Preparing the $distro test image" >&2
        # The host's CA bundle is mounted over the image's own (its real file, not a symlink to
        # it) only while packages install, readable by the unprivileged downloaders of apt and
        # pacman.
        printf 'FROM %s\nRUN --mount=type=secret,id=cacert,target=%s,mode=0444 %s\n' "$base" "$bundle" "$install" \
            | "$engine" build --secret id=cacert,src="$ca_bundle" --tag "$tag" - >&2
    fi
    echo "$tag"
}

run_in() {
    # run_in IMAGE [engine options...] -- COMMAND...
    local image=$1 options=()
    shift
    while [[ $1 != -- ]]; do options+=("$1"); shift; done
    shift
    "$engine" run --rm --security-opt label=disable \
        --volume "$appimage:/artifact/$(basename "$appimage"):ro" \
        --volume "$appimage.sha256:/artifact/$(basename "$appimage").sha256:ro" \
        --volume "$repo:/src:ro" --volume "$results:/results" "${options[@]}" "$image" "$@"
}

record() { summary+=("$(printf '%-10s %s' "$1" "$2")"); echo "==> $1: $2"; }

session() {
    # session NAME IMAGE BACKEND [engine options] -- COMMAND...
    local name=$1 image=$2 backend=$3; shift 3
    local options=()
    while [[ $1 != -- ]]; do options+=("$1"); shift; done
    shift
    rm -rf "${results:?}/$name"
    if run_in "$image" "${options[@]}" -- /src/packaging/appimage/tests/session.sh "$backend" "/results/$name" "$@"; then
        record "$name" "PASS ($(grep -c '"passed": true' "$results/$name/report.json") checks)"
    else
        record "$name" "FAIL (see target/appimage-smoke/$name/drive.log)"
    fi
}

appimage_in_container=/artifact/$(basename "$appimage")
for target in "${targets[@]}"; do
    case $target in
        static)
            build_image=$("$engine" images --format '{{.Repository}}:{{.Tag}}' | grep -m1 tardrop-appimage-build || true)
            [[ -n $build_image ]] || { echo "error: build image missing; run build.sh first" >&2; exit 1; }
            if run_in "$build_image" -- /src/packaging/appimage/tests/check-artifact.sh "$appimage_in_container" /src | tee "$results/static.log"; then
                record static PASS; else record static FAIL; fi ;;
        fedora|debian|ubuntu|arch|opensuse)
            session "$target" "$(image_for "$target")" x11 --env APPIMAGE_EXTRACT_AND_RUN=1 -- "$appimage_in_container" ;;
        wayland)
            session wayland "$(image_for fedora)" wayland --env APPIMAGE_EXTRACT_AND_RUN=1 -- "$appimage_in_container" ;;
        fuse)
            # The real path: the runtime mounts the image with FUSE instead of extracting it.
            # AppArmor's default container profile forbids mount(2), which FUSE needs.
            session fuse "$(image_for fedora)" x11 --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined -- "$appimage_in_container" ;;
        native)
            # The same scenario against a native build of the same source on Fedora's own GTK.
            native=localhost/tardrop-appimage-test:native
            if ! "$engine" image inspect "$native" > /dev/null 2>&1; then
                printf 'FROM %s\nRUN --mount=type=secret,id=cacert,target=/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem,mode=0444 dnf -y -q install rust cargo gtk4-devel libadwaita-devel && dnf clean all\n' "$(image_for fedora)" \
                    | "$engine" build --secret id=cacert,src="$ca_bundle" --tag "$native" -
            fi
            run_in "$native" --volume "$ca_bundle:/etc/tardrop/ca-bundle.crt:ro" --env CARGO_HTTP_CAINFO=/etc/tardrop/ca-bundle.crt --volume tardrop-cargo-native:/root/.cargo/registry -- \
                bash -c 'mkdir /build && tar -C /src --exclude=./target --exclude=./dist --exclude=./.git -cf - . | tar -C /build -xf - && cargo build --quiet --release --locked --manifest-path /build/Cargo.toml && cp /build/target/release/tardrop /results/tardrop-native'
            session native "$native" x11 -- /results/tardrop-native
            if [[ -f $results/fedora/report.json && -f $results/native/report.json ]]; then
                if python3 -I "$here/tests/compare.py" "$results/native/report.json" "$results/fedora/report.json" | tee "$results/native-vs-appimage.txt"; then
                    record compare "PASS (native and AppImage behave identically)"; else record compare "FAIL (see target/appimage-smoke/native-vs-appimage.txt)"; fi
            fi ;;
        minimal)
            if run_in "$(image_for minimal)" -- /src/packaging/appimage/tests/host-libraries.sh "$appimage_in_container" > "$results/minimal.txt" 2>&1; then
                record minimal "PASS (bundled libraries always load from the image; relocation works)"; else record minimal "FAIL (see target/appimage-smoke/minimal.txt)"; fi ;;
        old-glibc)
            image=$(image_for old-glibc)
            run_in "$image" --env APPIMAGE_EXTRACT_AND_RUN=1 -- "$appimage_in_container" > "$results/old-glibc.txt" 2>&1 && status=0 || status=$?
            if [[ $status != 0 ]] && grep -q "GLIBC_2\.3[6-9]' not found" "$results/old-glibc.txt"; then
                record old-glibc "PASS (refuses to start on glibc 2.35, with a clear loader error)"; else record old-glibc "FAIL (unexpected result, see target/appimage-smoke/old-glibc.txt)"; fi ;;
        *) echo "unknown target $target" >&2; exit 2 ;;
    esac
done

echo
echo "Smoke test of $(basename "$appimage")"
printf '  %s\n' "${summary[@]}"
! printf '%s\n' "${summary[@]}" | grep -q FAIL

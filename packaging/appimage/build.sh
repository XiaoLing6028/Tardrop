#!/usr/bin/env bash
# Builds TarDrop's x86_64 AppImage in the pinned build container and writes it to dist/, next to
# its .sha256 checksum file. See README.md in this directory.
#
#   packaging/appimage/build.sh [--skip-tests] [--image-only]
#
# Environment:
#   CONTAINER_ENGINE   docker or podman (default: docker if installed, else podman)
#   TARDROP_CA_BUNDLE  CA bundle for the container's downloads (default: the host's trust store)
#   SOURCE_DATE_EPOCH  timestamp for the image contents (default: the last commit's time)
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/../.." && pwd)
skip_tests=0
image_only=0
for argument in "$@"; do
    case $argument in
        --skip-tests) skip_tests=1 ;;
        --image-only) image_only=1 ;;
        -h|--help) sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option: $argument" >&2; exit 2 ;;
    esac
done

engine=${CONTAINER_ENGINE:-$(command -v docker || command -v podman || true)}
[[ -n $engine ]] || { echo "error: neither docker nor podman is installed" >&2; exit 1; }

ca_bundle=${TARDROP_CA_BUNDLE:-${SSL_CERT_FILE:-}}
if [[ -z $ca_bundle ]]; then
    for candidate in /etc/ssl/certs/ca-certificates.crt /etc/pki/tls/certs/ca-bundle.crt /etc/ssl/ca-bundle.pem /etc/ssl/cert.pem; do
        if [[ -r $candidate ]]; then ca_bundle=$candidate; break; fi
    done
fi
[[ -r $ca_bundle ]] || { echo "error: no CA bundle found; set TARDROP_CA_BUNDLE" >&2; exit 1; }

# The image is named after everything that goes into it, so a changed pin or script rebuilds it
# and an unchanged one reuses the cached image.
source "$here/pins.env"
inputs=$(cd "$here" && cat Containerfile pins.env image/*.sh keys/*.asc | sha256sum | cut -c1-16)
image=localhost/tardrop-appimage-build:$inputs
if ! "$engine" image inspect "$image" > /dev/null 2>&1; then
    echo "==> Building the build image $image (GTK and libadwaita compile once; this takes a while)"
    "$engine" build --file "$here/Containerfile" --build-arg BASE_IMAGE="$BASE_IMAGE" \
        --secret id=cacert,src="$ca_bundle" --tag "$image" "$here"
fi
[[ $image_only == 1 ]] && exit 0

epoch=${SOURCE_DATE_EPOCH:-$(git -C "$repo" log -1 --format=%ct 2> /dev/null || date +%s)}
mkdir -p "$repo/dist"
echo "==> Building the AppImage into $repo/dist"
# The checkout is mounted read-only and copied to a fresh directory inside the container, so
# nothing from an earlier local build (target/, dist/) takes part. label=disable lets the mounts
# work under SELinux without relabelling the checkout.
"$engine" run --rm --security-opt label=disable \
    --volume "$repo:/src:ro" \
    --volume "$repo/dist:/out" \
    --volume "$ca_bundle:/etc/tardrop/ca-bundle.crt:ro" \
    --volume tardrop-cargo-registry:/opt/cargo/registry \
    --env SSL_CERT_FILE=/etc/tardrop/ca-bundle.crt \
    --env CARGO_HTTP_CAINFO=/etc/tardrop/ca-bundle.crt \
    --env SOURCE_DATE_EPOCH="$epoch" \
    --env TARDROP_SKIP_TESTS="$skip_tests" \
    --env HOST_OWNER="$(id -u):$(id -g)" \
    "$image" /src/packaging/appimage/package.sh

#!/bin/bash
# Installs the pinned AppImage tooling: linuxdeploy (bundles shared-library dependencies and makes
# them relocatable), appimagetool (writes the image), and the type 2 runtime the image starts with.
set -euo pipefail
source /opt/tardrop/pins.env
source /opt/tardrop/image/fetch.sh

mkdir -p /opt/appimage
cd /opt/appimage
fetch "https://github.com/linuxdeploy/linuxdeploy/releases/download/$LINUXDEPLOY_TAG/linuxdeploy-x86_64.AppImage" "$LINUXDEPLOY_SHA256" linuxdeploy.AppImage
fetch "https://github.com/AppImage/appimagetool/releases/download/$APPIMAGETOOL_TAG/appimagetool-x86_64.AppImage" "$APPIMAGETOOL_SHA256" appimagetool.AppImage
fetch "https://github.com/AppImage/type2-runtime/releases/download/$RUNTIME_TAG/runtime-x86_64" "$RUNTIME_SHA256" runtime-x86_64

# The runtime is the only downloaded code that ships inside TarDrop's AppImage, so beyond the
# pinned hash it must carry a valid signature by the pinned type2-runtime release key.
curl --fail --location --silent --show-error --retry 4 --retry-all-errors --proto '=https' --tlsv1.2 \
    --cacert /run/secrets/cacert --output runtime-x86_64.sig \
    "https://github.com/AppImage/type2-runtime/releases/download/$RUNTIME_TAG/runtime-x86_64.sig"
GNUPGHOME=$(mktemp -d)
export GNUPGHOME
gpg --batch --quiet --import /opt/tardrop/keys/type2-runtime.asc
# VALIDSIG ends with the primary key's fingerprint, also when a subkey made the signature.
if ! gpg --batch --status-fd 1 --verify runtime-x86_64.sig runtime-x86_64 2> /dev/null | grep -Eq "^\[GNUPG:\] VALIDSIG .* $RUNTIME_SIGNING_KEY\$"; then
    echo "error: runtime-x86_64 is not signed by $RUNTIME_SIGNING_KEY" >&2
    exit 1
fi
rm -rf "$GNUPGHOME" runtime-x86_64.sig

# Unpacked once here, so the tools need neither FUSE nor a per-call extraction in the container.
for tool in linuxdeploy appimagetool; do
    chmod +x "$tool.AppImage"
    "./$tool.AppImage" --appimage-extract > /dev/null
    mv squashfs-root "$tool"
    rm "$tool.AppImage"
    ln -s "/opt/appimage/$tool/AppRun" "/usr/local/bin/$tool"
done

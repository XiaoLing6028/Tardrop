#!/bin/bash
# Installs the pinned Rust toolchain (rustc, the x86_64 standard library, and cargo) into /opt/rust
# from the official standalone component tarballs.
set -euo pipefail
source /opt/tardrop/pins.env
source /opt/tardrop/image/fetch.sh

work=$(mktemp -d)
cd "$work"
target=x86_64-unknown-linux-gnu
for component in rustc:$RUSTC_SHA256 rust-std:$RUST_STD_SHA256 cargo:$CARGO_SHA256; do
    name=${component%%:*}
    fetch "https://static.rust-lang.org/dist/$name-$RUST_VERSION-$target.tar.xz" "${component#*:}" "$name.tar.xz"
    tar -xJf "$name.tar.xz"
    "./$name-$RUST_VERSION-$target/install.sh" --prefix=/opt/rust --disable-ldconfig > /dev/null
done
cd /
rm -rf "$work"
/opt/rust/bin/rustc --version
/opt/rust/bin/cargo --version

# Download helper for the image scripts; sourced, not run.
#
# fetch URL SHA256 DEST downloads over HTTPS only and keeps DEST only when its SHA-256 is exactly
# SHA256, so a changed or truncated upstream file stops the build instead of entering the image.
# The CA bundle is the build secret build.sh passes in (the host's own trust store).

fetch() {
    local url=$1 sha256=$2 dest=$3
    curl --fail --location --silent --show-error --retry 4 --retry-all-errors --proto '=https' --tlsv1.2 \
        --cacert /run/secrets/cacert --output "$dest.part" "$url"
    if ! printf '%s  %s\n' "$sha256" "$dest.part" | sha256sum --check --status; then
        echo "error: checksum mismatch for $url" >&2
        echo "  expected $sha256" >&2
        echo "  received $(sha256sum "$dest.part" | cut -d' ' -f1)" >&2
        rm -f "$dest.part"
        return 1
    fi
    mv "$dest.part" "$dest"
}

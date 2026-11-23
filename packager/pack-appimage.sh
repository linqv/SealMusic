#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd)"
RELEASE_DIR="$PROJECT_ROOT/target/release"
CONFIG="$SCRIPT_DIR/Packager.linux.toml"
cd "$PROJECT_ROOT"

VERSION="$(awk -F= '
    /^\[package\][[:space:]]*$/ { in_package = 1; next }
    /^\[/ { in_package = 0 }
    in_package && $1 ~ /^[[:space:]]*version[[:space:]]*$/ {
        gsub(/[[:space:]"]/, "", $2)
        print $2
        exit
    }
' Cargo.toml)"
if [[ -z "$VERSION" ]]; then
    printf 'Could not read the package version from Cargo.toml\n' >&2
    exit 1
fi

MACHINE_ARCH="$(uname -m)"
case "$MACHINE_ARCH" in
    x86_64) APPIMAGE_ARCH="x86_64"; OUTPUT_ARCH="amd64" ;;
    aarch64|arm64) APPIMAGE_ARCH="aarch64"; OUTPUT_ARCH="arm64" ;;
    *)
        printf 'Unsupported AppImage architecture: %s\n' "$MACHINE_ARCH" >&2
        exit 1
        ;;
esac

VERSIONED_CONFIG="$(mktemp --suffix=.toml "$SCRIPT_DIR/.Packager.linux.XXXXXX")"
trap 'rm -f -- "$VERSIONED_CONFIG"' EXIT
{
    printf 'version = "%s"\n' "$VERSION"
    cat "$CONFIG"
} > "$VERSIONED_CONFIG"

GENERATED_APPIMAGE="$RELEASE_DIR/sealmusic_${VERSION}_${APPIMAGE_ARCH}.AppImage"
OUTPUT_APPIMAGE="$RELEASE_DIR/SealMusic_${VERSION}_${OUTPUT_ARCH}.AppImage"
printf 'Building SealMusic %s AppImage (%s)...\n' "$VERSION" "$OUTPUT_ARCH"
# The release binary is already stripped. Older linuxdeploy strip cannot read modern ELF RELR sections.
NO_STRIP=1 cargo packager --config "$VERSIONED_CONFIG" --formats appimage

if [[ ! -f "$GENERATED_APPIMAGE" ]]; then
    printf 'Expected AppImage was not created: %s\n' "$GENERATED_APPIMAGE" >&2
    exit 1
fi
mv -f -- "$GENERATED_APPIMAGE" "$OUTPUT_APPIMAGE"

printf 'Package ready: %s\n' "$OUTPUT_APPIMAGE"

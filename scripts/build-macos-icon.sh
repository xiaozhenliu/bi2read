#!/usr/bin/env bash
set -euo pipefail

repo_root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
source_png="${1:-$repo_root/packaging/macos/assets/AppIcon-v1-transparent.png}"
output_icns="${2:-$repo_root/packaging/macos/assets/AppIcon.icns}"

for command_name in iconutil sips; do
    command -v "$command_name" >/dev/null 2>&1 || {
        printf 'Required command is unavailable: %s\n' "$command_name" >&2
        exit 1
    }
done

[ -f "$source_png" ] || {
    printf 'Icon source is missing: %s\n' "$source_png" >&2
    exit 1
}
case "$output_icns" in /*) ;; *) printf 'Output path must be absolute.\n' >&2; exit 2 ;; esac

width=$(sips -g pixelWidth "$source_png" | awk '/pixelWidth/{print $2}')
height=$(sips -g pixelHeight "$source_png" | awk '/pixelHeight/{print $2}')
[ "$width" = "$height" ] || {
    printf 'Icon source must be square; found %sx%s.\n' "$width" "$height" >&2
    exit 1
}
[ "$width" -ge 1024 ] || {
    printf 'Icon source must be at least 1024x1024; found %sx%s.\n' "$width" "$height" >&2
    exit 1
}

temporary_root=$(mktemp -d "${TMPDIR:-/tmp}/bi2read-icon.XXXXXX")
trap 'rm -rf "$temporary_root"' EXIT
iconset="$temporary_root/AppIcon.iconset"
mkdir -p "$iconset" "$(dirname "$output_icns")"

render() {
    size=$1
    name=$2
    sips -z "$size" "$size" "$source_png" --out "$iconset/$name" >/dev/null
}

render 16 icon_16x16.png
render 32 icon_16x16@2x.png
render 32 icon_32x32.png
render 64 icon_32x32@2x.png
render 128 icon_128x128.png
render 256 icon_128x128@2x.png
render 256 icon_256x256.png
render 512 icon_256x256@2x.png
render 512 icon_512x512.png
render 1024 icon_512x512@2x.png

iconutil --convert icns --output "$output_icns" "$iconset"
printf 'Created macOS icon: %s\n' "$output_icns"

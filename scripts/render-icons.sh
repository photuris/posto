#!/usr/bin/env bash
# Renders the Posto icon rasters from the SVG masters in assets/icon.
# Needs rsvg-convert; the preview also needs magick.
# Usage: scripts/render-icons.sh [--preview contact-sheet.png]
set -euo pipefail

cd "$(dirname "$0")/.."

preview=""
case "${1:-}" in
    "") ;;
    --preview) preview="${2:?--preview needs an output path}" ;;
    *) echo "usage: render-icons.sh [--preview out.png]" >&2; exit 2 ;;
esac

src=assets/icon
hicolor=packaging/linux/icons/hicolor

render() {
    local svg=$1 px=$2 out=$3
    mkdir -p "$(dirname "$out")"
    rsvg-convert -w "$px" -h "$px" "$svg" -o "$out"
}

# Linux hicolor theme: the small glyph below 48 px, the master from 48 up.
for n in 16 24 32; do
    render "$src/posto-small.svg" "$n" "$hicolor/${n}x${n}/apps/posto.png"
done
for n in 48 64 128 256 512; do
    render "$src/posto.svg" "$n" "$hicolor/${n}x${n}/apps/posto.png"
done
mkdir -p "$hicolor/scalable/apps"
cp "$src/posto.svg" "$hicolor/scalable/apps/posto.svg"

# Runtime window icon.
render "$src/posto.svg" 256 crates/posto-gui/assets/icon-256.png

# Contact sheet on white then on a dark panel. Not committed.
if [[ -n "$preview" ]]; then
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    rows=()
    for bg in '#FFFFFF' '#1E1E2E'; do
        tiles=()
        for n in 16 24 32 48 128 256; do
            svg=$src/posto.svg
            (( n < 48 )) && svg=$src/posto-small.svg
            render "$svg" "$n" "$tmp/p-$n.png"
            magick -size 288x288 "xc:$bg" "$tmp/p-$n.png" -gravity center -composite "$tmp/t-${bg#\#}-$n.png"
            tiles+=("$tmp/t-${bg#\#}-$n.png")
        done
        magick "${tiles[@]}" +append "$tmp/row-${bg#\#}.png"
        rows+=("$tmp/row-${bg#\#}.png")
    done
    magick "${rows[@]}" -append "$preview"
    echo "wrote $preview"
fi

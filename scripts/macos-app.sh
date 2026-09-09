#!/bin/sh
# Assemble FileMan.app from a release binary and etc/macos/icon.png.
# Usage: macos-app.sh <fileman-binary> <FileMan.app>
set -eu

if [ $# -ne 2 ]; then
    echo "usage: $0 <fileman-binary> <FileMan.app>" >&2
    exit 2
fi

binary=$1
dest=$2
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$root/Cargo.toml" | head -n1)
if [ -z "$version" ]; then
    echo "$0: could not read version from Cargo.toml" >&2
    exit 1
fi
if [ ! -f "$binary" ]; then
    echo "$0: missing binary: $binary" >&2
    exit 1
fi
icon=$root/etc/macos/icon.png
if [ ! -f "$icon" ]; then
    echo "$0: missing icon: $icon" >&2
    exit 1
fi

stage=$(mktemp -d "${TMPDIR:-/tmp}/fileman-app.XXXXXX")
trap 'rm -rf "$stage"' EXIT
app=$stage/FileMan.app
macos=$app/Contents/MacOS
resources=$app/Contents/Resources
iconset=$stage/FileMan.iconset

mkdir -p "$macos" "$resources" "$iconset"
install -m 755 "$binary" "$macos/fileman"
sed "s/@VERSION@/$version/g" "$root/etc/macos/Info.plist" >"$app/Contents/Info.plist"
printf 'APPL????' >"$app/Contents/PkgInfo"

# iconutil names are pixel size and density. Source icon.png is 256×256.
sips -z 16 16 "$icon" --out "$iconset/icon_16x16.png" >/dev/null
sips -z 32 32 "$icon" --out "$iconset/icon_16x16@2x.png" >/dev/null
sips -z 32 32 "$icon" --out "$iconset/icon_32x32.png" >/dev/null
sips -z 64 64 "$icon" --out "$iconset/icon_32x32@2x.png" >/dev/null
sips -z 128 128 "$icon" --out "$iconset/icon_128x128.png" >/dev/null
sips -z 256 256 "$icon" --out "$iconset/icon_128x128@2x.png" >/dev/null
sips -z 256 256 "$icon" --out "$iconset/icon_256x256.png" >/dev/null
sips -z 512 512 "$icon" --out "$iconset/icon_256x256@2x.png" >/dev/null
sips -z 512 512 "$icon" --out "$iconset/icon_512x512.png" >/dev/null
sips -z 1024 1024 "$icon" --out "$iconset/icon_512x512@2x.png" >/dev/null
iconutil -c icns "$iconset" -o "$resources/AppIcon.icns"
rm -rf "$iconset"

codesign --sign - --force --deep "$app"

mkdir -p "$(dirname -- "$dest")"
rm -rf "$dest"
ditto "$app" "$dest"

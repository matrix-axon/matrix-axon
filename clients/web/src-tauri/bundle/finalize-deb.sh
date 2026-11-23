#!/bin/sh
set -eu
umask 022

if [ "$#" -ne 2 ]; then
    printf 'Usage: %s TAURI_DEB OUTPUT_DEB\n' "$0" >&2
    exit 2
fi

bundle_dir=$(dirname -- "$(readlink -f -- "$0")")
stage=$(mktemp -d)
trap 'rm -rf -- "$stage"' EXIT

dpkg-deb --raw-extract "$1" "$stage"
chmod 0755 "$stage" "$stage/DEBIAN"
mkdir -p "$stage/usr/lib/axon"
mv "$stage/usr/bin/axon" "$stage/usr/lib/axon/axon"
install -m 0755 "$bundle_dir/axon-launcher.sh" "$stage/usr/bin/axon"

# Tauri also uses the desktop template for AppImage, so set the launcher
# flag in the Debian package and preserve the callback argument.
desktop_entry=$stage/usr/share/applications/Axon.desktop
if ! grep -Eq '^Exec=.+ %u$' "$desktop_entry"; then
    printf 'The Tauri desktop entry is missing the callback URL argument.\n' >&2
    exit 1
fi
sed -i 's/^Exec=/Exec=env AXON_DESKTOP_LAUNCH=1 /' "$desktop_entry"

# Tauri appends an unversioned WebKitGTK dependency to the versioned one
# from tauri.conf.json. Zenity belongs to the package with the launcher.
if ! grep -Eq '^Depends: .*libwebkit2gtk-4[.]1-0 \(>= ' "$stage/DEBIAN/control"; then
    printf 'The Tauri bundle is missing the minimum WebKitGTK dependency.\n' >&2
    exit 1
fi
sed -Ei '/^Depends: /{ s/, libwebkit2gtk-4[.]1-0(,|$)/\1/; s/$/, zenity/; }' "$stage/DEBIAN/control"

(
    cd "$stage"
    # Regenerate checksums after relocating the binary and installing the wrapper.
    find . -path ./DEBIAN -prune -o -type f -printf '%P\0' |
        LC_ALL=C sort -z | xargs -0 md5sum > DEBIAN/md5sums
)
dpkg-deb --root-owner-group --build "$stage" "$2"

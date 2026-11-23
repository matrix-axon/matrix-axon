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
# current_exe() will see the relocated binary. Keep Tauri's packaged
# resources in its installed fallback directory /usr/lib/Axon and use
# Tauri's resource resolver for any future runtime resource lookups.
mkdir -p "$stage/usr/lib/axon"
mv "$stage/usr/bin/axon" "$stage/usr/lib/axon/axon"
install -m 0755 "$bundle_dir/axon-launcher.sh" "$stage/usr/bin/axon"

# Tauri also uses the desktop template for AppImage, so set the launcher
# flag in the Debian package and preserve the callback argument.
desktop_entry=$stage/usr/share/applications/Axon.desktop
if ! awk '
    /^\[/ { main_entry = ($0 == "[Desktop Entry]") }
    main_entry && /^Exec=/ {
        if (++commands != 1 || $0 !~ /^Exec=.+ %u$/)
            exit 1
        sub(/^Exec=/, "Exec=env AXON_DESKTOP_LAUNCH=1 ")
    }
    { print }
    END { if (commands != 1) exit 1 }
' "$desktop_entry" > "$desktop_entry.new"; then
    printf 'Expected one Exec command with a callback URL argument in [Desktop Entry].\n' >&2
    exit 1
fi
mv "$desktop_entry.new" "$desktop_entry"

# Tauri appends an unversioned WebKitGTK dependency to the versioned one
# from tauri.conf.json. Zenity belongs to the package with the launcher.
control=$stage/DEBIAN/control
if ! awk '
    /^Depends: / {
        count = split(substr($0, length("Depends: ") + 1), dependencies, /,[[:blank:]]*/)
        printf "Depends: "
        for (i = 1; i <= count; i++) {
            dependency = dependencies[i]
            if (dependency == "libwebkit2gtk-4.1-0")
                continue
            if (dependency ~ /^libwebkit2gtk-4[.]1-0 \(>= /)
                minimum_found = 1
            printf "%s, ", dependency
        }
        print "zenity"
        next
    }
    { print }
    END { if (!minimum_found) exit 1 }
' "$control" > "$control.new"; then
    printf 'The Tauri bundle is missing the minimum WebKitGTK dependency.\n' >&2
    exit 1
fi
mv "$control.new" "$control"

(
    cd "$stage"
    # Regenerate checksums after relocating the binary and installing the wrapper.
    find . -path ./DEBIAN -prune -o -type f -printf '%P\0' |
        LC_ALL=C sort -z | xargs -0 md5sum > DEBIAN/md5sums
)
dpkg-deb --root-owner-group --build "$stage" "$2"

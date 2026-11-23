#!/bin/sh

launcher=$(readlink -f -- "$0") || exit 1
app=${launcher%/*}/../lib/axon/axon
desktop_launch=${AXON_DESKTOP_LAUNCH:-}
unset AXON_DESKTOP_LAUNCH

probe() {
    # ldd -r asks glibc to resolve function symbols without entering the app.
    # It can report missing symbols with a zero exit status.
    LC_ALL=C /usr/bin/ldd -r "$app" 2>&1
}

webkit_failure() {
    printf '%s\n' "$1" | grep -Eq \
        -e 'undefined symbol: (webkit_|jsc_|JS[A-Z])' \
        -e 'undefined symbol: .*[[:space:]]+\(.*/lib(webkit2gtk|javascriptcoregtk)-4[.]1[.]so([.][0-9]+)*\)[[:space:]]*$' \
        -e 'lib(webkit2gtk|javascriptcoregtk)-4[.]1[.]so[.]0 => not found' \
        -e 'lib(webkit2gtk|javascriptcoregtk)-4[.]1[.]so[.]0: (cannot open|wrong ELF|invalid ELF)' \
        -e 'not found \(required by .*lib(webkit2gtk|javascriptcoregtk)-4[.]1[.]so'
}

loaded_library() {
    printf '%s\n' "$1" | awk -v name="$2" '
        {
            sub(/^[[:space:]]*/, "")
            sub(/ \(0x[[:xdigit:]]+\)$/, "")
            if (index($0, name " => /") == 1) {
                print substr($0, length(name) + 5)
                exit
            }
            if ($0 ~ /^\// && substr($0, length($0) - length(name)) == "/" name) {
                print
                exit
            }
        }
    '
}

shadowed_library() {
    [ -n "$1" ] && resolved=$(readlink -f -- "$1") || return 1
    case "$resolved" in
        "$2"/*) return 1 ;;
        *) return 0 ;;
    esac
}

linkage=$(probe) || :
if webkit_failure "$linkage"; then
    missing_dependency_only=no
elif printf '%s\n' "$linkage" | grep -Fq '=> not found'; then
    missing_dependency_only=yes
else
    exec "$app" "$@"
fi

package=libwebkit2gtk-4.1-0
minimum=2.42
native_package=$package:$(dpkg --print-architecture)
system_dir=$(dpkg-query -L "$native_package" 2>/dev/null |
    awk '/^\/(usr\/)?lib\/[^/]+\/libwebkit2gtk-4[.]1[.]so[.]0$/ { sub(/\/[^/]+$/, ""); print; exit }')

webkit=$(loaded_library "$linkage" libwebkit2gtk-4.1.so.0)
javascriptcore=$(loaded_library "$linkage" libjavascriptcoregtk-4.1.so.0)
# ldd does not say which object needed a missing library. Try the distro
# lookup only if the first lookup found a WebKit library outside that directory.
if [ "$missing_dependency_only" = yes ]; then
    distro_dir=$(readlink -f -- "$system_dir") || exec "$app" "$@"
    if ! shadowed_library "$webkit" "$distro_dir" && ! shadowed_library "$javascriptcore" "$distro_dir"; then
        exec "$app" "$@"
    fi
fi

fallback_path="$system_dir${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
if [ -n "$system_dir" ] &&
    [ -r "$system_dir/libwebkit2gtk-4.1.so.0" ] &&
    [ -r "$system_dir/libjavascriptcoregtk-4.1.so.0" ] &&
    fallback=$(LD_LIBRARY_PATH="$fallback_path" probe) &&
    ! printf '%s\n' "$fallback" | grep -Eq 'undefined symbol:|=> not found|not found \(required by|error while loading shared libraries:' &&
    [ "$(loaded_library "$fallback" libwebkit2gtk-4.1.so.0)" = "$system_dir/libwebkit2gtk-4.1.so.0" ] &&
    [ "$(loaded_library "$fallback" libjavascriptcoregtk-4.1.so.0)" = "$system_dir/libjavascriptcoregtk-4.1.so.0" ]; then
    printf 'Axon: using the distro WebKitGTK and JavaScriptCore from %s.\n' "$system_dir" >&2
    LD_LIBRARY_PATH=$fallback_path exec "$app" "$@"
fi

# An unnamed missing dependency may belong to the app or another library.
# If the retry did not fix it, let the loader name the original failure.
if [ "$missing_dependency_only" = yes ]; then
    exec "$app" "$@"
fi

installed=$(dpkg-query -W -f='${db:Status-Status} ${Version}' "$native_package" 2>/dev/null) || installed=
distro_version=${installed#installed }
if [ "$distro_version" = "$installed" ]; then
    distro_version='not installed'
fi

if [ "$distro_version" = 'not installed' ] ||
    dpkg --compare-versions "$distro_version" lt "$minimum"; then
    guidance="Install or update $package to version $minimum or newer from your distro's repositories."
else
    guidance="If either library path points to a local installation, update or remove that installation.
Otherwise, reinstall the distro's WebKitGTK and JavaScriptCore packages."
fi

message="Axon could not start because it could not load a compatible WebKitGTK 4.1 runtime.

WebKitGTK: ${webkit:-path unavailable}
JavaScriptCore: ${javascriptcore:-path unavailable}
$package: $distro_version

$guidance"
printf '%s\n' "$message" >&2

# Zenity can report the failure without loading WebKitGTK.
if [ "$desktop_launch" = 1 ]; then
    zenity --error --no-markup --title='Axon could not start' --text="$message" || :
fi
exit 1

#!/bin/sh
set -eu
umask 022

bundle_dir=$(dirname -- "$(readlink -f -- "$0")")
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT

input=$work/input
mkdir -p "$input/DEBIAN" "$input/usr/bin" "$input/usr/share/applications" \
    "$work/old" "$work/bad-symbol" "$work/missing-library" "$work/local" "$work/vendor" "$work/tools"
cat > "$input/DEBIAN/control" <<EOF
Package: axon
Version: 1.0
Architecture: $(dpkg --print-architecture)
Maintainer: Axon
Depends: libwebkit2gtk-4.1-0, libwebkit2gtk-4.1-0 (>= 2.42), libgtk-3-0
Recommends: hunspell-dictionary, zenity
Description: Axon desktop launcher test
EOF
cat > "$input/usr/share/applications/Axon.desktop" <<'EOF'
[Desktop Entry]
Exec=axon %u

[Desktop Action Help]
Exec=axon-help %u
EOF

# Use the same relocation as Axon without starting a webview or needing a
# display. The callback argument and exit status must survive the launcher.
cat > "$work/app.c" <<'EOF'
#include <stdio.h>
#include <stdlib.h>

extern void webkit_cookie_manager_get_all_cookies(void *, void *, void *, void *);
extern unsigned int jsc_get_major_version(void);

int main(int argc, char **argv) {
    if (argc != 2)
        return 1;
    if (!jsc_get_major_version())
        webkit_cookie_manager_get_all_cookies(NULL, NULL, NULL, NULL);
    puts(argv[1]);
    const char *path = getenv("LD_LIBRARY_PATH");
    puts(path ? path : "default");
    return 23;
}
EOF
build_app() {
    cc -Wl,-z,now "$work/app.c" -lwebkit2gtk-4.1 -ljavascriptcoregtk-4.1 "$@"
}
build_app -o "$input/usr/bin/axon"

cc -shared -fPIC -Wl,-soname,libwebkit2gtk-4.1.so.0 -x c - -o "$work/old/libwebkit2gtk-4.1.so.0" <<'EOF'
void axon_test_old_webkit(void) {}
EOF
cat > "$work/webkit.c" <<'EOF'
void webkit_cookie_manager_get_all_cookies(void) {}
EOF
cc -shared -fPIC -Wl,-soname,libwebkit2gtk-4.1.so.0 "$work/webkit.c" \
    -o "$work/local/libwebkit2gtk-4.1.so.0"
cc -shared -fPIC -Wl,-z,now,-soname,libwebkit2gtk-4.1.so.0 "$work/webkit.c" -x c - \
    -o "$work/bad-symbol/libwebkit2gtk-4.1.so.0" <<'EOF'
extern void axon_fixture_missing_symbol(void);
void fixture_uses_dependency(void) { axon_fixture_missing_symbol(); }
EOF
cc -shared -fPIC -Wl,-soname,libaxon-fixture-dependency.so.0 -x c - \
    -o "$work/libaxon-fixture-dependency.so.0" <<'EOF'
void fixture_dependency(void) {}
EOF
# Record a needed library without a relocation, so ldd only reports
# "=> not found" for this fixture. The library sits outside its search path.
cc -shared -fPIC -Wl,-z,now,-soname,libwebkit2gtk-4.1.so.0 "$work/webkit.c" \
    -Wl,--no-as-needed "$work/libaxon-fixture-dependency.so.0" \
    -o "$work/missing-library/libwebkit2gtk-4.1.so.0"

dpkg-deb --root-owner-group --build "$input" "$work/input.deb"
"$bundle_dir/finalize-deb.sh" "$work/input.deb" "$work/axon.deb"
test "$(dpkg-deb --field "$work/axon.deb" Depends)" = 'libwebkit2gtk-4.1-0 (>= 2.42), libgtk-3-0'
test "$(dpkg-deb --field "$work/axon.deb" Recommends)" = 'hunspell-dictionary, zenity'
dpkg-deb --raw-extract "$work/axon.deb" "$work/installed"
(cd "$work/installed" && md5sum --check DEBIAN/md5sums)
grep -Fx 'Exec=env AXON_DESKTOP_LAUNCH=1 axon %u' "$work/installed/usr/share/applications/Axon.desktop"
grep -Fx 'Exec=axon-help %u' "$work/installed/usr/share/applications/Axon.desktop"

axon=$work/installed/usr/bin/axon
callback='org.matrixaxon.axon:/oauth/callback?code=test%20code&state=test'
unset LD_LIBRARY_PATH LD_PRELOAD AXON_DESKTOP_LAUNCH

status=0
"$axon" "$callback" > "$work/out" 2> "$work/error" || status=$?
test "$status" -eq 23
printf '%s\ndefault\n' "$callback" | diff - "$work/out"
test ! -s "$work/error"

expect_recovery() {
    status=0
    LD_LIBRARY_PATH=$1 "$axon" "$callback" > "$work/out" 2> "$work/error" || status=$?
    if [ "$status" -ne 23 ]; then
        cat "$work/error" >&2
        return 1
    fi
    printf '%s\n%s:%s\n' "$callback" "$(pkg-config --variable=libdir webkit2gtk-4.1)" "$1" | diff - "$work/out"
    grep -F 'using the distro WebKitGTK and JavaScriptCore' "$work/error"
}
expect_recovery "$work/old"
expect_recovery "$work/bad-symbol"
expect_recovery "$work/missing-library"

# A preload takes precedence even on the distro lookup. Exercise failure
# without removing or downgrading anything on the test runner.
cat > "$work/tools/zenity" <<'EOF'
#!/bin/sh
printf '%s\n' "$@" > "$AXON_TEST_DIALOG"
EOF
chmod +x "$work/tools/zenity"
export PATH="$work/tools:$PATH" AXON_TEST_DIALOG="$work/dialog"
status=0
LD_PRELOAD=$work/old/libwebkit2gtk-4.1.so.0 "$axon" > "$work/out" 2> "$work/error" || status=$?
test "$status" -eq 1
test ! -s "$work/out"
test ! -e "$work/dialog"
grep -F "WebKitGTK: $work/old/libwebkit2gtk-4.1.so.0" "$work/error"
grep -F 'update or remove that installation' "$work/error"

status=0
AXON_DESKTOP_LAUNCH=1 LD_PRELOAD=$work/old/libwebkit2gtk-4.1.so.0 \
    "$axon" > "$work/out" 2> "$work/error" || status=$?
test "$status" -eq 1
grep -F -- '--title=Axon could not start' "$work/dialog"
grep -F "WebKitGTK: $work/old/libwebkit2gtk-4.1.so.0" "$work/dialog"

# Minimal installs can omit Zenity and still get the same stderr diagnostic.
mkdir "$work/minimal-path"
for tool in awk dpkg dpkg-query grep readlink; do
    ln -s "$(command -v "$tool")" "$work/minimal-path/$tool"
done
status=0
PATH=$work/minimal-path AXON_DESKTOP_LAUNCH=1 LD_PRELOAD=$work/old/libwebkit2gtk-4.1.so.0 \
    "$axon" > "$work/out" 2> "$work/minimal-error" || status=$?
test "$status" -eq 1
cmp "$work/error" "$work/minimal-error"

# The upgrade guidance uses the installed package version even when
# the selected library comes from a different installation.
cat > "$work/tools/dpkg-query" <<'EOF'
#!/bin/sh
if [ "$1" = -W ]; then
    printf 'installed 2.40.5\n'
    exit
fi
exec /usr/bin/dpkg-query "$@"
EOF
chmod +x "$work/tools/dpkg-query"
status=0
LD_PRELOAD=$work/old/libwebkit2gtk-4.1.so.0 "$axon" > "$work/out" 2> "$work/error" || status=$?
test "$status" -eq 1
grep -F 'libwebkit2gtk-4.1-0: 2.40.5' "$work/error"
grep -F 'Install or update libwebkit2gtk-4.1-0 to version 2.42 or newer' "$work/error"

# The probe and the recovered app both need the caller's vendor directory.
build_app -Wl,--no-as-needed "$work/libaxon-fixture-dependency.so.0" \
    -o "$work/installed/usr/lib/axon/axon"
cp "$work/libaxon-fixture-dependency.so.0" "$work/vendor/"
expect_recovery "$work/vendor:$work/old"

# A missing app dependency should report the loader's own error even
# when the initial lookup selected a local WebKit library.
rm -f "$work/dialog"
for library_path in "" "$work/local"; do
    status=0
    AXON_DESKTOP_LAUNCH=1 LD_LIBRARY_PATH=$library_path "$axon" "$callback" > "$work/out" 2> "$work/error" || status=$?
    test "$status" -eq 127
    grep -F 'error while loading shared libraries: libaxon-fixture-dependency.so.0' "$work/error"
    test ! -s "$work/out"
    test ! -e "$work/dialog"
done

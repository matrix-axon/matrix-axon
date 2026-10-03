#!/usr/bin/env bash
#
# Build, and optionally install, the Android app (ADR 0102, M-W13).
#
# `tauri android build` alone fails or misleads in ways that do not announce
# themselves. This wraps it with the things it gets wrong:
#
#   * The JDK. The Gradle wrapper in the generated project is 8.14.x, which
#     runs on Java 8 through 24. Anything newer fails at the first Gradle line
#     with `Unsupported class file major version 70` (Java 26) and no mention
#     of Java. Distributions now default to a newer JDK than that, and often
#     ship a JRE with no `javac` for the older one. This finds a JDK 17-21
#     that has a compiler and exports it as JAVA_HOME, rather than making the
#     caller set it; Android's tooling is qualified against 17 and 21.
#
#   * The generated project is sticky, as `gen/apple` is: `tauri android init`
#     writes `gen/android` once and never revisits it. It is regenerated every
#     run, so tauri.conf.json is the single source of truth for what is built.
#
#   * The ABI. An emulator on an x86_64 host needs the x86_64 build; a device
#     needs arm64. An APK built for the wrong one installs and then fails with
#     INSTALL_FAILED_NO_MATCHING_ABIS, so say which with --target.
#
# Not a gate and not run by CI. Store signing is not handled here yet: this
# produces the APK used for testing.
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/package-android.sh [options]

  --target <abi>       aarch64 (default, devices) | x86_64 (emulator) |
                       armv7 | i686
  --debug              build a debug APK (default: release)
  --install            adb install the built APK (uses ANDROID_SERIAL or the
                       only connected device/emulator)
  -h, --help           this

Examples:
  scripts/package-android.sh --target x86_64 --debug --install
USAGE
}

target="aarch64"
debug=0
install_app=0

while [ $# -gt 0 ]; do
  case "$1" in
    --target) target="${2:?--target needs a value}"; shift ;;
    --debug) debug=1 ;;
    --install) install_app=1 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

case "$target" in
  aarch64|x86_64|armv7|i686) ;;
  *) echo "error: unknown --target '$target'" >&2; usage >&2; exit 2 ;;
esac

# Major version of the JDK at $1, or nothing if it has no javac. `javac
# -version` prints "javac 21.0.5"; Java 8 prints "javac 1.8.0_x".
jdk_major() {
  [ -x "$1/bin/javac" ] || return 0
  "$1/bin/javac" -version 2>&1 | sed -n 's/^javac \(1\.\)\{0,1\}\([0-9]*\).*/\2/p'
}

acceptable_jdk() {
  local major
  major=$(jdk_major "$1")
  [ -n "$major" ] && [ "$major" -ge 17 ] && [ "$major" -le 21 ]
}

# Prints the JAVA_HOME to use, or nothing. Honours an acceptable JAVA_HOME,
# otherwise takes the newest acceptable JDK the system has.
pick_jdk() {
  if [ -n "${JAVA_HOME:-}" ] && acceptable_jdk "$JAVA_HOME"; then
    echo "$JAVA_HOME"
    return 0
  fi
  local dir best="" best_major=0 major
  for dir in /usr/lib/jvm/* /Library/Java/JavaVirtualMachines/*/Contents/Home; do
    [ -d "$dir" ] || continue
    acceptable_jdk "$dir" || continue
    major=$(jdk_major "$dir")
    if [ "$major" -gt "$best_major" ]; then best="$dir"; best_major=$major; fi
  done
  if [ -n "$best" ]; then echo "$best"; fi
  return 0
}

jdk=$(pick_jdk)
if [ -z "$jdk" ]; then
  cat >&2 <<MSG
error: no JDK 17-21 with a compiler (javac) was found.

  JAVA_HOME=${JAVA_HOME:-(unset)}

Gradle 8.14 cannot run on Java 25 or newer, and a JRE cannot compile. Install
a full JDK 21, e.g.:

  sudo apt install openjdk-21-jdk-headless
MSG
  exit 1
fi
export JAVA_HOME="$jdk"
export PATH="$JAVA_HOME/bin:$PATH"
echo "==> JDK $(jdk_major "$JAVA_HOME") at $JAVA_HOME"

: "${ANDROID_HOME:?set ANDROID_HOME to the Android SDK}"
: "${NDK_HOME:?set NDK_HOME to an NDK under \$ANDROID_HOME/ndk}"
export PATH="$HOME/.cargo/bin:$PATH"

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
web_dir="$repo_root/clients/web"
tauri_dir="$web_dir/src-tauri"

cd "$web_dir"

echo "==> regenerating the Android project"
rm -rf "$tauri_dir/gen/android"
pnpm tauri android init --ci

# `init` writes a MainActivity that turns on edge-to-edge and never handles the
# insets that implies; ours does (see the header of the file). Copied rather
# than generated for the same reason the iOS icons are: `gen/` is regenerated
# and gitignored, so nothing edited there survives.
echo "==> installing android/MainActivity.kt"
cp "$tauri_dir/android/MainActivity.kt" \
  "$tauri_dir/gen/android/app/src/main/java/org/matrixaxon/axon/MainActivity.kt"

build_args=(tauri android build --apk --target "$target")
if [ "$debug" -eq 1 ]; then build_args+=(--debug); fi

echo "==> building ($target)"
pnpm "${build_args[@]}"

# `|| true`: under pipefail a missing APK would abort here silently, before
# the diagnostic written for exactly that case.
apk=$(find "$tauri_dir/gen/android/app/build/outputs/apk" -name '*.apk' -print0 2>/dev/null \
  | xargs -0 ls -t 2>/dev/null | head -1 || true)
if [ -z "$apk" ]; then
  echo "error: the build reported success but produced no .apk" >&2
  exit 1
fi
echo "==> built $apk"

if [ "$install_app" -eq 1 ]; then
  echo "==> installing"
  adb install -r "$apk"
fi

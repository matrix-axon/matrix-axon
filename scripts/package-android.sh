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
#   * Signing. Nothing here touches Gradle's signing config: the generated
#     `build.gradle.kts` is regenerated every run, so a config edited into it
#     would not survive. The build is left unsigned and then signed afterwards
#     with `jarsigner` (an .aab, which is what Play takes) or `zipalign` +
#     `apksigner` (an .apk). The key is an *upload* key (ADR 0110): Play
#     re-signs with its own, so this key is only how Google knows an upload is
#     ours, and it can be reset if it is lost. Its location and passwords come
#     from the environment, never from arguments, which land in `ps` and in
#     shell history.
#
# Not a gate and not run by CI (a lane would need the key as a secret).
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/package-android.sh [options]

  --target <abi>       aarch64 (default, devices) | x86_64 (emulator) |
                       armv7 | i686
  --debug              build a debug APK (default: release)
  --aab                build an Android App Bundle (what Play takes) instead
                       of an APK; release only
  --sign               sign a release build with the upload key named in the
                       environment (below); needs --aab or a release APK
  --version-code <n>   Play rejects a reused versionCode; default is derived
                       from the version (0.1.3 -> 1003), so pass a larger
                       number to upload the same version twice
  --install            adb install the built APK (uses ANDROID_SERIAL or the
                       only connected device/emulator). Needs --debug or
                       --sign: an unsigned release APK is refused.
  -h, --help           this

Environment for --sign:
  ANDROID_KEYSTORE            path to the upload keystore (never in the repo)
  ANDROID_KEY_ALIAS           the key's alias in it
  ANDROID_KEYSTORE_PASSWORD   its password (the key must share it, which a
                              PKCS12 keystore, keytool's default, does)

Examples:
  scripts/package-android.sh --target x86_64 --debug --install
  scripts/package-android.sh --target aarch64 --aab --sign --version-code 1004
USAGE
}

target="aarch64"
debug=0
aab=0
sign=0
version_code=""
install_app=0

while [ $# -gt 0 ]; do
  case "$1" in
    --target) target="${2:?--target needs a value}"; shift ;;
    --debug) debug=1 ;;
    --aab) aab=1 ;;
    --sign) sign=1 ;;
    --version-code) version_code="${2:?--version-code needs a value}"; shift ;;
    --install) install_app=1 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

if [ "$install_app" -eq 1 ] && [ "$debug" -eq 0 ] && [ "$sign" -eq 0 ]; then
  echo "error: --install needs --debug or --sign; an unsigned release APK fails with INSTALL_PARSE_FAILED_NO_CERTIFICATES" >&2
  exit 2
fi

case "$target" in
  aarch64|x86_64|armv7|i686) ;;
  *) echo "error: unknown --target '$target'" >&2; usage >&2; exit 2 ;;
esac

# All of this is checked before the build rather than at the signing step: a
# forgotten variable should not cost a full release build before it is
# mentioned.
if [ "$aab" -eq 1 ] && [ "$debug" -eq 1 ]; then
  echo "error: --aab is a release artifact; drop --debug" >&2
  exit 2
fi
if [ "$sign" -eq 1 ] && [ "$debug" -eq 1 ]; then
  echo "error: --debug builds are already signed with the debug key; --sign is for release" >&2
  exit 2
fi
if [ "$aab" -eq 1 ] && [ "$install_app" -eq 1 ]; then
  echo "error: adb cannot install an .aab; build an APK to install" >&2
  exit 2
fi
# Spliced into a JSON `--config` override below, so anything but digits would
# either close the object early or reach `tauri` as a config it reports
# obscurely.
if [ -n "$version_code" ] && ! [[ $version_code =~ ^[1-9][0-9]{0,8}$ ]]; then
  echo "error: --version-code must be a positive integer of at most nine digits; got '$version_code'" >&2
  exit 2
fi
if [ "$sign" -eq 1 ]; then
  : "${ANDROID_KEYSTORE:?set ANDROID_KEYSTORE to the upload keystore path (keep it outside the repo)}"
  : "${ANDROID_KEY_ALIAS:?set ANDROID_KEY_ALIAS to the key alias}"
  : "${ANDROID_KEYSTORE_PASSWORD:?set ANDROID_KEYSTORE_PASSWORD}"
  if [ ! -f "$ANDROID_KEYSTORE" ]; then
    echo "error: ANDROID_KEYSTORE '$ANDROID_KEYSTORE' does not exist" >&2
    exit 2
  fi
fi

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

if [ "$aab" -eq 1 ]; then format=--aab; else format=--apk; fi
build_args=(tauri android build "$format" --target "$target")
if [ "$debug" -eq 1 ]; then build_args+=(--debug); fi
# Carried as a config override rather than written into the project, which is
# regenerated above; `tauri.properties` picks it up at build time.
if [ -n "$version_code" ]; then
  build_args+=(--config "{\"bundle\":{\"android\":{\"versionCode\":$version_code}}}")
fi

echo "==> building ($target, $([ "$aab" -eq 1 ] && echo aab || echo apk))"
pnpm "${build_args[@]}"

# `|| true`: under pipefail a missing artifact would abort here silently,
# before the diagnostic written for exactly that case. `find -exec ... +`
# rather than `| xargs ls -t`: with no match xargs still runs `ls` once with no
# arguments, which lists the current directory and "finds" a file that is not
# the artifact.
out_dir="$tauri_dir/gen/android/app/build/outputs"
if [ "$aab" -eq 1 ]; then ext=aab; else ext=apk; fi
artifact=$(find "$out_dir" -name "*.$ext" -exec ls -t -- {} + 2>/dev/null \
  | head -1 || true)
if [ -z "$artifact" ]; then
  echo "error: the build reported success but produced no .$ext" >&2
  exit 1
fi
echo "==> built $artifact"

if [ "$sign" -eq 1 ]; then
  base="${artifact%.$ext}"
  signed="${base%-unsigned}-signed.$ext"
  rm -f "$signed"
  if [ "$aab" -eq 1 ]; then
    # `-storepass:env` and not `-storepass`: an argument is world-readable in
    # `ps`. RSA is what the upload key is generated as (ADR 0110).
    jarsigner -keystore "$ANDROID_KEYSTORE" -storepass:env ANDROID_KEYSTORE_PASSWORD \
      -sigalg SHA256withRSA -digestalg SHA-256 \
      -signedjar "$signed" "$artifact" "$ANDROID_KEY_ALIAS"
    # Non-zero for an unsigned or tampered file (measured: an unsigned .aab
    # exits 1), so `set -e` stops the script rather than reporting "signed".
    jarsigner -verify "$signed" >/dev/null
  else
    build_tools=$(ls -d "$ANDROID_HOME"/build-tools/*/ 2>/dev/null | sort -V | tail -1 || true)
    if [ -z "$build_tools" ] || [ ! -x "${build_tools}apksigner" ]; then
      echo "error: no build-tools with apksigner under $ANDROID_HOME/build-tools" >&2
      exit 1
    fi
    # Aligned before signing: apksigner's v2+ signature covers the whole file,
    # so aligning afterwards would break it.
    aligned="${base%-unsigned}-aligned.$ext"
    rm -f "$aligned"
    "${build_tools}zipalign" -f -p 4 "$artifact" "$aligned"
    "${build_tools}apksigner" sign --ks "$ANDROID_KEYSTORE" \
      --ks-key-alias "$ANDROID_KEY_ALIAS" --ks-pass env:ANDROID_KEYSTORE_PASSWORD \
      --out "$signed" "$aligned"
    "${build_tools}apksigner" verify "$signed"
    rm -f "$aligned" "$signed.idsig"
  fi
  artifact="$signed"
  echo "==> signed $artifact"
  # What Play Console asks for when the upload key is registered, and what to
  # compare if an upload is refused for a signature mismatch.
  fingerprint=$(keytool -list -keystore "$ANDROID_KEYSTORE" -alias "$ANDROID_KEY_ALIAS" \
    -storepass:env ANDROID_KEYSTORE_PASSWORD 2>/dev/null \
    | sed -n 's/^Certificate fingerprint (SHA-256): /    upload key SHA-256: /p; s/^Certificate fingerprint (SHA256): /    upload key SHA-256: /p' || true)
  if [ -n "$fingerprint" ]; then
    echo "$fingerprint"
  else
    # Silent here would read as "nothing to compare". keytool failed (the
    # alias, the password) or labels the line differently on this JDK.
    echo "warning: could not read the upload key's SHA-256 fingerprint; run keytool -list -v -keystore \"\$ANDROID_KEYSTORE\" -alias \"\$ANDROID_KEY_ALIAS\" to compare it with Play Console" >&2
  fi
elif [ "$debug" -eq 0 ] && [ "$aab" -eq 0 ]; then
  echo "warning: a release APK without --sign is unsigned and will not install" >&2
fi

if [ "$install_app" -eq 1 ]; then
  echo "==> installing"
  adb install -r "$artifact"
fi

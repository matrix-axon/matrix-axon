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
  --version-code <n|auto>
                       Play rejects a reused versionCode; default is derived
                       from the version (0.1.3 -> 1003), so pass a larger
                       number to upload the same version twice. `auto` asks
                       Google Play for the highest it has seen and adds one
                       (needs --aab and the environment below)
  --upload <mode>      after signing, send the bundle and its native debug
                       symbols to the Play internal track; needs --aab --sign.
                       check    upload and let Play validate it, then discard the
                                edit: nothing is published and the versionCode
                                stays free (what to try first)
                       draft    leave it as a DRAFT release; testers see nothing
                                until it is completed in Play Console
                       release  roll it out to the track
  --install            adb install the built APK (uses ANDROID_SERIAL or the
                       only connected device/emulator). Needs --debug or
                       --sign: an unsigned release APK is refused.
  -h, --help           this

Environment for --version-code auto and --upload:
  PLAY_SERVICE_ACCOUNT_JSON   path to a Google Cloud service-account key (JSON)
                              invited in Play Console with permission to view
                              the app and its releases (and, for --upload, to
                              release to testing tracks); never in the repo

Environment for --sign:
  ANDROID_KEYSTORE            path to the upload keystore (never in the repo)
  ANDROID_KEY_ALIAS           the key's alias in it
  ANDROID_KEYSTORE_PASSWORD   its password (the key must share it, which a
                              PKCS12 keystore, keytool's default, does)

Examples:
  scripts/package-android.sh --target x86_64 --debug --install
  scripts/package-android.sh --target aarch64 --aab --sign --version-code 1004
  scripts/package-android.sh --target aarch64 --aab --sign --version-code auto
  scripts/package-android.sh --target aarch64 --aab --sign --version-code auto --upload check
USAGE
}

target="aarch64"
debug=0
aab=0
sign=0
version_code=""
upload_mode=""
install_app=0

while [ $# -gt 0 ]; do
  case "$1" in
    --target) target="${2:?--target needs a value}"; shift ;;
    --debug) debug=1 ;;
    --aab) aab=1 ;;
    --sign) sign=1 ;;
    --version-code) version_code="${2:?--version-code needs a value}"; shift ;;
    --upload) upload_mode="${2:?--upload needs check, draft or release}"; shift ;;
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
# The symbols are packaged with `zip` after the build, under `set -e`, so a
# missing one would otherwise end a long build with a bare "command not found".
if [ "$aab" -eq 1 ] && ! command -v zip >/dev/null 2>&1; then
  echo "error: --aab packages the native debug symbols with zip, which is not installed (e.g. sudo apt install zip)" >&2
  exit 2
fi
# Spliced into a JSON `--config` override below, so anything but digits would
# either close the object early or reach `tauri` as a config it reports
# obscurely.
if [ -n "$version_code" ] && [ "$version_code" != "auto" ] && ! [[ $version_code =~ ^[1-9][0-9]{0,8}$ ]]; then
  echo "error: --version-code must be a positive integer of at most nine digits, or auto; got '$version_code'" >&2
  exit 2
fi
# `auto` talks to Google Play, so it needs what an upload needs and only makes
# sense for a build that is going there. Checked now, before the multi-minute
# build, so a missing credential costs seconds.
if [ "$version_code" = "auto" ]; then
  if [ "$aab" -ne 1 ]; then
    echo "error: --version-code auto asks Google Play for the next number, so it needs --aab (a bundle for Play)" >&2
    exit 2
  fi
  : "${PLAY_SERVICE_ACCOUNT_JSON:?set PLAY_SERVICE_ACCOUNT_JSON to the path of a Play service-account key (JSON); see scripts/lib/play-next-version-code.py}"
  if [ ! -f "$PLAY_SERVICE_ACCOUNT_JSON" ]; then
    echo "error: PLAY_SERVICE_ACCOUNT_JSON '$PLAY_SERVICE_ACCOUNT_JSON' does not exist" >&2
    exit 2
  fi
  for tool in python3 openssl; do
    if ! command -v "$tool" >/dev/null 2>&1; then
      echo "error: --version-code auto needs $tool" >&2
      exit 2
    fi
  done
fi
if [ -n "$upload_mode" ]; then
  case "$upload_mode" in
    check|draft|release) ;;
    *) echo "error: --upload takes check, draft or release; got '$upload_mode'" >&2; exit 2 ;;
  esac
  if [ "$aab" -ne 1 ] || [ "$sign" -ne 1 ]; then
    echo "error: --upload sends a signed bundle to Play, so it needs --aab and --sign" >&2
    exit 2
  fi
  : "${PLAY_SERVICE_ACCOUNT_JSON:?set PLAY_SERVICE_ACCOUNT_JSON to the path of a Play service-account key (JSON); see scripts/lib/play-upload.py}"
  if [ ! -f "$PLAY_SERVICE_ACCOUNT_JSON" ]; then
    echo "error: PLAY_SERVICE_ACCOUNT_JSON '$PLAY_SERVICE_ACCOUNT_JSON' does not exist" >&2
    exit 2
  fi
  for tool in python3 openssl; do
    if ! command -v "$tool" >/dev/null 2>&1; then
      echo "error: --upload needs $tool" >&2
      exit 2
    fi
  done
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

# The cargo target and the ABI directory Android and Play name it by.
case "$target" in
  aarch64) rust_triple=aarch64-linux-android;    abi=arm64-v8a ;;
  armv7)   rust_triple=armv7-linux-androideabi;  abi=armeabi-v7a ;;
  x86_64)  rust_triple=x86_64-linux-android;     abi=x86_64 ;;
  i686)    rust_triple=i686-linux-android;       abi=x86 ;;
  *) echo "error: no cargo target for --target '$target'" >&2; exit 2 ;;
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

# Say which NDK is used, and say so if it is a prerelease: NDK_HOME is usually
# a `latest` symlink, so what it points at changes when an NDK is installed, and
# a store build should not come from a beta (`29.0.13846066-beta3` was the
# default on the machine this was written on).
ndk_revision=$(sed -n 's/^Pkg.Revision *= *//p' "$NDK_HOME/source.properties" 2>/dev/null | head -1 || true)
echo "==> NDK ${ndk_revision:-unknown} at $(readlink -f "$NDK_HOME")"
case "$ndk_revision" in
  *beta*|*rc*|*alpha*)
    echo "warning: NDK $ndk_revision is a prerelease; use a stable NDK for a build that ships" >&2 ;;
esac

# Cargo does not treat a different NDK as a reason to rebuild: the linker path
# is not part of a crate's fingerprint, so C objects compiled by one NDK's clang
# stay in `target/` and are linked next to the new one's. Measured: after moving
# `latest` from a 29 beta to 30, the library still carried the beta compiler's
# build stamp (13818152 in `strings`) alongside 30's; after clearing `target/`
# it was gone. A build that ships should come from one toolchain, so when the
# NDK differs from the one that last built here, start the Android targets over.
tauri_target="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/clients/web/src-tauri/target"
ndk_stamp="$tauri_target/.android-ndk-revision"
if [ -d "$tauri_target" ] && [ "$(cat "$ndk_stamp" 2>/dev/null || true)" != "$ndk_revision" ]; then
  echo "==> NDK changed since the last build here; clearing the Android build artifacts"
  rm -rf "$tauri_target"/*-linux-android*
fi
export PATH="$HOME/.cargo/bin:$PATH"

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
web_dir="$repo_root/clients/web"
tauri_dir="$web_dir/src-tauri"

cd "$web_dir"

# Resolve `auto` now, before `rm -rf gen/android` and a multi-minute build, so a
# bad credential or an unreachable Play costs seconds. After this `version_code`
# is an ordinary number, spliced into the JSON override below, so it is checked
# again: whatever the helper printed must be digits and nothing else. The
# package is read from tauri.conf.json so it cannot drift from the app's own.
if [ "$version_code" = "auto" ] || [ -n "$upload_mode" ]; then
  package=$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["identifier"])' "$tauri_dir/tauri.conf.json")
fi
if [ "$version_code" = "auto" ]; then
  version_code=$(python3 "$repo_root/scripts/lib/play-next-version-code.py" "$package") || exit 1
  if ! [[ $version_code =~ ^[1-9][0-9]{0,9}$ ]]; then
    echo "error: the Play helper returned '$version_code', which is not a versionCode" >&2
    exit 1
  fi
  echo "==> versionCode $version_code (the highest Google Play lists for $package, plus one)"
fi

echo "==> regenerating the Android project"
rm -rf "$tauri_dir/gen/android"
pnpm tauri android init --ci

# The launcher icon. `init` writes Tauri's own artwork into `gen/android` and
# never revisits it, so without this the app ships with the Tauri logo. The
# committed set under `icons/android/` is the artwork of record, and includes an
# adaptive-icon definition the generated project lacks, so it is overlaid
# rather than used to replace single files.
echo "==> syncing launcher icons from icons/android"
cp -R "$tauri_dir/icons/android/." "$tauri_dir/gen/android/app/src/main/res/"

# `init` writes a MainActivity that turns on edge-to-edge and never handles the
# insets that implies; ours does (see the header of the file). Copied rather
# than generated for the same reason as the icons, and the iOS ones: `gen/` is
# regenerated and gitignored, so nothing edited there survives.
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
# Recorded only once the build has succeeded, so a failed one is redone clean.
mkdir -p "$tauri_target" && printf '%s\n' "$ndk_revision" > "$ndk_stamp"

echo "==> built $artifact"

# Native debug symbols, for a bundle. Play warns that "you've not uploaded debug
# symbols" for any bundle with native code, and without them a crash in the Rust
# library arrives as raw addresses. The unstripped library cargo just linked is
# what Play needs: it is byte-for-byte the one inside the bundle (the Android
# Gradle Plugin does not strip it), and Play matches it to a crash by build ID,
# which `build.rs` adds. Android Gradle Plugin's own extraction was tried and
# declares the library "already stripped", so this packages it directly: a zip
# with the ABI directory at its root and the unstripped .so inside, the layout
# Play's help page describes. It is uploaded separately from the bundle (Play
# Console: Test and release > App bundle explorer > the version > Downloads >
# Assets; or the developer API as a `nativeCode` deobfuscation file) and must
# come from the same build as the bundle it describes, which is why it is
# written beside it.
if [ "$aab" -eq 1 ]; then
  unstripped="$tauri_dir/target/$rust_triple/release/libaxon_lib.so"
  symbols="$(dirname "$artifact")/native-debug-symbols.zip"
  if [ ! -f "$unstripped" ]; then
    echo "error: no unstripped library at $unstripped to package as debug symbols" >&2
    exit 1
  fi
  readelf_bin=$(ls "$NDK_HOME"/toolchains/llvm/prebuilt/*/bin/llvm-readelf 2>/dev/null | head -1 || true)
  build_id=""
  if [ -n "$readelf_bin" ]; then
    build_id=$("$readelf_bin" -n "$unstripped" | sed -n 's/^ *Build ID: *//p' | head -1)
  fi
  if [ -z "$build_id" ]; then
    echo "error: the Android library has no GNU build ID, so Play could not match symbols to it (build.rs adds one)" >&2
    exit 1
  fi
  stage=$(mktemp -d)
  # Removed on any exit, so a failed `cp` or `zip` does not leave a copy of the
  # unstripped library behind.
  trap 'rm -rf "$stage"' EXIT
  mkdir -p "$stage/$abi"
  cp "$unstripped" "$stage/$abi/libaxon_lib.so"
  rm -f "$symbols"
  (cd "$stage" && zip -q -r "$symbols" "$abi")
  rm -rf "$stage"
  trap - EXIT
  echo "==> native debug symbols $symbols ($(du -h "$symbols" | cut -f1), build ID $build_id)"
fi

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

# Upload. After signing, so what Play receives is exactly the file that was just
# verified, and with the symbols zip from the same build, because Play matches
# them to the bundle by build ID. `check` is the safe first run: Play inspects the
# bundle on upload and the edit is then discarded. The release is named after the
# version so it is recognisable in Play Console.
if [ -n "$upload_mode" ]; then
  props="$tauri_dir/gen/android/app/tauri.properties"
  built_code=$(sed -n 's/^tauri.android.versionCode=//p' "$props" 2>/dev/null || true)
  built_name=$(sed -n 's/^tauri.android.versionName=//p' "$props" 2>/dev/null || true)
  # Without these the release would be named " ()" and there would be nothing
  # to check Play's answer against, so stop before anything is sent.
  if [ -z "$built_code" ] || [ -z "$built_name" ]; then
    echo "error: could not read the versionCode and versionName from $props; nothing was uploaded" >&2
    exit 1
  fi
  echo "==> uploading to Google Play (internal track, mode: $upload_mode)"
  # `--expect-version-code`: the helper stops before it commits if Play numbers
  # the bundle differently from what was built, rather than this script
  # noticing once the wrong one is already on the track.
  uploaded_code=$(python3 "$repo_root/scripts/lib/play-upload.py" \
    --package "$package" --bundle "$artifact" --symbols "$symbols" \
    --track internal --mode "$upload_mode" --name "$built_name ($built_code)" \
    --expect-version-code "$built_code") || exit 1
  echo "==> Play accepted versionCode $uploaded_code ($upload_mode)"
fi

if [ "$install_app" -eq 1 ]; then
  echo "==> installing"
  adb install -r "$artifact"
fi

#!/usr/bin/env bash
#
# Build, and optionally install or upload, the iOS app (ADR 0102, M-W13).
#
# `tauri ios build` alone does not produce a shippable app from a clean
# checkout. This wraps it with the things it gets wrong, each of which cost a
# debugging session to find and none of which announces itself:
#
#   * The generated project is sticky. `gen/apple` is written once and then
#     left alone, so anything under `bundle.iOS` in tauri.conf.json applies to
#     the run that created it and to no other. This regenerates it every time
#     — see the block above `tauri ios init` below — which is also why the
#     build number arrives as a `--config` override rather than through
#     `tauri ios build --build-number`.
#
#   * The app icon. `tauri ios init` writes Tauri's own default artwork into
#     `gen/apple` when it generates the project and then never revisits it, so
#     a project first generated before `tauri icon` was ever run keeps the
#     Tauri logo forever. `gen/` is gitignored, so nothing in review sees it
#     and the wrong icon reaches the home screen. The committed set under
#     `src-tauri/icons/ios/` is the artwork of record — same 18 filenames — so
#     this copies it in rather than regenerating: `pnpm tauri icon` rewrites 52
#     tracked files under `icons/` with re-encodes that change no pixels, and a
#     packaging step has no business dirtying the tree.
#
#   * The Rust toolchain. A Homebrew `rust` shadows rustup's shims whenever
#     /usr/local/bin precedes ~/.cargo/bin, and Homebrew's rust has no iOS std
#     and ignores `rust-toolchain.toml`. The failure is
#     `can't find crate for 'std'` immediately after the CLI reports the target
#     "up to date", because rustup is telling the truth about a toolchain that
#     is not the one cargo will run.
#
#   * The signing keychain. Signing runs inside xcodebuild, which reads the
#     login keychain, and that one is locked in an SSH session and after a
#     reboot. The failure is `errSecInternalComponent` from codesign, or a
#     password dialog nobody is there to answer. Set AXON_SIGNING_KEYCHAIN to a
#     keychain that holds only the signing identities and has an empty
#     password, and this unlocks it first. Off unless set: it is one
#     developer's setup, not a requirement. Shared with package-macos-mas.sh;
#     see "Signing without a password prompt" in clients/web/src-tauri/README.md
#     for how to build that keychain.
#
#   * FORCE_COLOR. An exported `FORCE_COLOR=1` ends up as a stray argument to
#     the Rust build phase, which reads it as an architecture; the failure is
#     `Arch specified by Xcode was invalid. {arch} isn't a known arch` and
#     names neither. This unsets it — see the block below `export PATH`.
#
# Not a gate and not run by CI; #445 tracks a lane that would, and this script
# is what it should be built from. `--upload` needs App Store Connect
# credentials this repo does not carry; see the block above that flag below.
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/package-ios.sh [options]

  --install            install the built .ipa to a connected device
  --device <udid>      which device (default: the only connected one)
  --export-method <m>  debugging (default) | release-testing | app-store-connect
  --build-number <n>   CFBundleVersion; App Store Connect rejects a reused one.
                       `auto` asks App Store Connect for the highest it has and
                       uses one more (app-store-connect only; needs the same
                       credentials as --upload)
  --upload             upload the .ipa to App Store Connect / TestFlight
  -h, --help           this

ASC_KEY_ID and ASC_ISSUER_ID, which --upload and `--build-number auto` need, are
taken from the environment or, failing that, from .env at the repository root.

Examples:
  scripts/package-ios.sh --install
  scripts/package-ios.sh --export-method app-store-connect --build-number 2 --upload
  scripts/package-ios.sh --export-method app-store-connect --build-number auto --upload
USAGE
}

install_app=0
upload=0
device=""
export_method="debugging"
build_number=""

while [ $# -gt 0 ]; do
  case "$1" in
    --install) install_app=1 ;;
    --upload) upload=1 ;;
    --device) device="${2:?--device needs a UDID}"; shift ;;
    --export-method) export_method="${2:?--export-method needs a value}"; shift ;;
    --build-number) build_number="${2:?--build-number needs a value}"; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

# Checked here rather than at the upload, which is after the build and after
# `--install`: a forgotten environment variable should not cost a full iOS
# build before it is mentioned.
if [ "$upload" -eq 1 ]; then
  if [ "$export_method" != "app-store-connect" ]; then
    echo "error: --upload needs --export-method app-store-connect; got $export_method" >&2
    exit 2
  fi
fi
# `--build-number auto` talks to App Store Connect, so it needs what `--upload`
# does and only makes sense for a build that is going there.
if [ "$build_number" = "auto" ] && [ "$export_method" != "app-store-connect" ]; then
  echo "error: --build-number auto needs --export-method app-store-connect; got $export_method" >&2
  exit 2
fi
repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

# The App Store Connect and signing-keychain logic is shared with
# package-macos-mas.sh, so the two cannot drift apart.
. "$repo_root/scripts/lib/asc.sh"
. "$repo_root/scripts/lib/signing-keychain.sh"

if [ "$upload" -eq 1 ] || [ "$build_number" = "auto" ]; then
  asc_require_credentials "$repo_root"
fi

# Checked here for the same reason, and because this value is spliced into a
# JSON `--config` override below. `--build-number '1"}'` closes the object
# early and `--build-number '1 2'` splits into two arguments, and both reach
# `tauri` as a config it reports obscurely — after `rm -rf gen/apple` has
# already thrown the Xcode project away. `CFBundleVersion` is one to three
# period-separated non-negative integers, so anything else is a typo, not a
# version.
if [ -n "$build_number" ] && [ "$build_number" != "auto" ] && ! [[ $build_number =~ ^[0-9]+(\.[0-9]+){0,2}$ ]]; then
  echo "error: --build-number must be one to three dot-separated numbers (CFBundleVersion); got '$build_number'" >&2
  exit 2
fi

# 12: `devicectl device install app` lists --device in its usage as an option it
# requires; there is no "the only connected one" default. Resolve it now so
# `--install` with no `--device` does not fail with a missing-argument error
# after the build.
if [ "$install_app" -eq 1 ] && [ -z "$device" ]; then
  devices_json=$(mktemp)
  xcrun devicectl list devices --json-output "$devices_json" >/dev/null 2>&1 || true
  device=$(python3 "$(dirname "${BASH_SOURCE[0]}")/lib/pick-ios-device.py" "$devices_json" || true)
  rm -f "$devices_json"
  if [ -z "$device" ]; then
    echo "error: --install needs a device, and none was resolved." >&2
    echo "       Either nothing is connected, or several are (listed above)." >&2
    echo "       Name one with --device <udid>." >&2
    exit 2
  fi
  echo "==> resolved device $device"
fi

web_dir="$repo_root/clients/web"
tauri_dir="$web_dir/src-tauri"
icon_src="$tauri_dir/icons/ios"
appiconset="$tauri_dir/gen/apple/Assets.xcassets/AppIcon.appiconset"

# Resolve `auto` now, before `rm -rf gen/apple` and a multi-minute build, so a
# bad credential or an unreachable App Store Connect costs seconds. After this
# `build_number` is an ordinary number and everything below treats it as one,
# including the regex that guards the JSON override. See lib/asc.sh.
if [ "$build_number" = "auto" ]; then
  build_number=$(asc_next_build_number "$repo_root" "$tauri_dir/tauri.conf.json") || exit 1
fi

# Put rustup's shims first rather than diagnosing the Homebrew shadow after the
# fact. Harmless when they already are.
export PATH="$HOME/.cargo/bin:$PATH"

# Clear FORCE_COLOR, which a shell rc commonly exports (`export FORCE_COLOR=1`)
# so that node tools colour their output through a pipe.
#
# The generated Xcode project's "Build Rust Code" phase runs
# `pnpm tauri ios xcode-script … --configuration $CONFIGURATION ${FORCE_COLOR}
# ${ARCHS}`, and Xcode fills `${FORCE_COLOR}` in from the environment. With it
# set to 1 the command ends `release 1 arm64`, the CLI reads the 1 as an
# architecture, and the build dies in that phase with
#
#   Arch specified by Xcode was invalid. {arch} isn't a known arch
#
# — which names no architecture (the `{arch}` is printed literally, a
# formatting bug in the CLI) and says nothing about colour, so nothing in it
# points here. It only happens in a session that sourced the rc file: the same
# build passed over SSH and failed from a terminal, with identical Xcode
# build settings in both. Unset, the placeholder expands to nothing, which is
# what the template expects. The cost is uncoloured Tauri output.
unset FORCE_COLOR

# Then check, because prepending only helps if rustup is what is installed.
sysroot=$(rustc --print sysroot)
if [ ! -d "$sysroot/lib/rustlib/aarch64-apple-ios" ]; then
  cat >&2 <<EOF
error: the active Rust toolchain has no aarch64-apple-ios standard library.

  rustc sysroot: $sysroot

If that is not under ~/.rustup, a standalone Rust (Homebrew's, usually) is
shadowing rustup's shims and cannot build for iOS at all. Otherwise:

  rustup target add aarch64-apple-ios
EOF
  exit 1
fi

# Unlock the signing keychain before the build rather than discovering it is
# locked at the codesign step, minutes in. A no-op unless AXON_SIGNING_KEYCHAIN
# is set; see lib/signing-keychain.sh.
unlock_signing_keychain || exit 1

cd "$web_dir"

# Regenerate the Xcode project every time, from scratch.
#
# `tauri ios init` writes `project.yml` when it first creates `gen/apple` and
# never revisits it, so anything under `bundle.iOS` in tauri.conf.json that
# lands there is applied once and then frozen. Changing
# `minimumSystemVersion` to 15.0 and rebuilding produced a bundle still
# declaring 14.0 — and re-running `init` over the existing project changed
# nothing, because it too leaves what is already there alone. The same
# stickiness is why the app icon stayed Tauri's and why the signing team
# vanished when the directory was once removed by hand.
#
# `gen/` is generated and gitignored, so there is nothing in it to preserve.
# Deleting it costs a few seconds and makes the config the single source of
# truth for what gets built.
# The build number rides in as a config override rather than through
# `tauri ios build --build-number`. That flag writes the number into the
# generated project, so it lands on the *following* build — passing 1 produced
# a bundle still saying 0.1.0, and the next build, with no flag at all, said
# 0.1.0.1. Regenerating the project each run then clears it entirely. As an
# override it is merged into the config that both steps read, which is the same
# path `minimumSystemVersion` takes, and it has to reach both: the project
# carries it, and the build reads it back.
# Seeded rather than left empty: macOS ships bash 3.2, where `"${a[@]}"` on an
# empty array is an unbound variable under `set -u`, and `#!/usr/bin/env bash`
# finds that one whenever Homebrew's is not installed. Every use below expands
# it after the subcommand, so a leading `--ci`-style no-op is not available —
# carrying the flag and its value as one unit is.
config_args=()
if [ -n "$build_number" ]; then
  config_args=(--config "{\"bundle\":{\"iOS\":{\"bundleVersion\":\"$build_number\"}}}")
fi

echo "==> regenerating the iOS project"
rm -rf "$tauri_dir/gen/apple"
pnpm tauri ios init --ci ${config_args[@]+"${config_args[@]}"}

echo "==> installing the iOS entitlements"
# `tauri ios init` writes an empty entitlements file and has no setting to fill
# it, so the committed one is copied over it on every regeneration. Without it
# Sign in with Apple fails at run time, not at build time: see the file's own
# comment.
entitlements="$tauri_dir/gen/apple/axon_iOS/axon_iOS.entitlements"
if [ ! -f "$entitlements" ]; then
  echo "error: no generated entitlements file at $entitlements" >&2
  exit 1
fi
cp "$tauri_dir/Entitlements.ios.plist" "$entitlements"

echo "==> syncing the app icon from icons/ios"
if [ ! -d "$appiconset" ]; then
  echo "error: no appiconset at $appiconset" >&2
  exit 1
fi
# Flattened onto white rather than copied. `tauri icon` writes the iOS set with
# an alpha channel, and App Store Connect rejects the upload for it — error
# 90717, "Invalid large app icon ... can't be transparent or contain an alpha
# channel" — after the build, the signing and the upload have all succeeded.
# White is the background the set is already drawn against, so nothing changes
# visually; see scripts/lib/flatten-icons.swift.
xcrun swift "$repo_root/scripts/lib/flatten-icons.swift" "$appiconset" "$icon_src"/*.png
echo "    $(ls "$icon_src"/*.png | wc -l | tr -d ' ') icons, flattened"

build_args=(tauri ios build --export-method "$export_method"
  ${config_args[@]+"${config_args[@]}"})

echo "==> building (export method: $export_method)"
pnpm "${build_args[@]}"

# `|| true` is load-bearing, not defensive noise. Under `set -euo pipefail` a
# command substitution whose pipeline fails aborts the script *at this line*,
# and the redirect swallows the only clue — so a missing .ipa would exit
# silently instead of reaching the diagnostic written for exactly that case.
ipa=$(ls -t "$tauri_dir"/gen/apple/build/*/*.ipa 2>/dev/null | head -1 || true)
if [ -z "$ipa" ]; then
  echo "error: the build reported success but produced no .ipa" >&2
  exit 1
fi
echo "==> built $ipa"

# Say what actually shipped rather than what was meant to, read from the .ipa
# itself. Both the URL scheme and the version have been wrong in a bundle that
# built and signed cleanly: TestFlight build 25 shipped with no URL scheme, so
# every browser sign-in ended in "the application couldn't be opened".
#
# Not from DerivedData. That was read here once, by newest timestamp across
# every `axon-*` directory, which is another workspace's app whenever two
# workspaces build at once; and the archive step builds it with signing
# disabled, so it never carries entitlements. Only the exported .ipa is what
# ships.
#
# Fatal, not a warning: a line in thousands of lines of build output is not
# read, and `--upload` would go on to ship the broken bundle.
expected_scheme=org.matrixaxon.axon
ipa_check=$(mktemp -d)
unzip -q "$ipa" 'Payload/*' -d "$ipa_check"
shipped_app=$(ls -d "$ipa_check"/Payload/*.app | head -1)
shipped_plist="$shipped_app/Info.plist"
scheme=$(plutil -extract CFBundleURLTypes.0.CFBundleURLSchemes.0 raw -o - "$shipped_plist" 2>/dev/null || echo "(none)")
echo "    url scheme:   $scheme"
echo "    version:      $(plutil -extract CFBundleShortVersionString raw -o - "$shipped_plist" 2>/dev/null)"
echo "    build number: $(plutil -extract CFBundleVersion raw -o - "$shipped_plist" 2>/dev/null)"
problems=0
if [ "$scheme" != "$expected_scheme" ]; then
  echo "error: the .ipa declares URL scheme '$scheme', not '$expected_scheme' — browser sign-in (Google, Microsoft) could not return to the app" >&2
  echo "       the merged Info.plist is $tauri_dir/gen/apple/axon_iOS/Info.plist; its source is Info.ios.plist" >&2
  problems=1
fi
if codesign -d --entitlements - --xml "$shipped_app" 2>/dev/null | grep -q com.apple.developer.applesignin; then
  echo "    apple sign-in: entitled"
else
  echo "error: the signed app lacks com.apple.developer.applesignin — Sign in with Apple would fail with error 1000" >&2
  problems=1
fi
rm -rf "$ipa_check"
if [ "$problems" -ne 0 ]; then
  echo "error: not installing or uploading this build" >&2
  exit 1
fi

if [ "$install_app" -eq 1 ]; then
  echo "==> installing to device"
  xcrun devicectl device install app --device "$device" "$ipa"
fi

if [ "$upload" -eq 1 ]; then
  # The .p8 stays in ~/.appstoreconnect/private_keys where altool looks for it;
  # this script never reads it and it is never committed. Both variables were
  # checked before the build.
  echo "==> uploading to App Store Connect"
  xcrun altool --upload-app --type ios --file "$ipa" \
    --apiKey "$ASC_KEY_ID" --apiIssuer "$ASC_ISSUER_ID"
  echo "    uploaded; TestFlight processing takes a few minutes"
fi

#!/usr/bin/env bash
#
# Build, and optionally install or upload, the iOS app (ADR 0102, M-W13).
#
# `tauri ios build` alone does not produce a shippable app from a clean
# checkout. This wraps it with the two things it gets wrong, both of which cost
# a debugging session to find and neither of which announces itself:
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
  --build-number <n>   CFBundleVersion; App Store Connect rejects a reused one
  --upload             upload the .ipa to App Store Connect / TestFlight
  -h, --help           this

Examples:
  scripts/package-ios.sh --install
  scripts/package-ios.sh --export-method app-store-connect --build-number 2 --upload
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

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
web_dir="$repo_root/clients/web"
tauri_dir="$web_dir/src-tauri"
icon_src="$tauri_dir/icons/ios"
appiconset="$tauri_dir/gen/apple/Assets.xcassets/AppIcon.appiconset"

# Put rustup's shims first rather than diagnosing the Homebrew shadow after the
# fact. Harmless when they already are.
export PATH="$HOME/.cargo/bin:$PATH"

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
config_args=()
if [ -n "$build_number" ]; then
  config_args=(--config "{\"bundle\":{\"iOS\":{\"bundleVersion\":\"$build_number\"}}}")
fi

echo "==> regenerating the iOS project"
rm -rf "$tauri_dir/gen/apple"
pnpm tauri ios init --ci "${config_args[@]}"

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

build_args=(tauri ios build --export-method "$export_method" "${config_args[@]}")

echo "==> building (export method: $export_method)"
pnpm "${build_args[@]}"

ipa=$(ls -t "$tauri_dir"/gen/apple/build/*/*.ipa 2>/dev/null | head -1)
if [ -z "$ipa" ]; then
  echo "error: the build reported success but produced no .ipa" >&2
  exit 1
fi
echo "==> built $ipa"

# Say what actually shipped rather than what was meant to. Both of these have
# been wrong in a bundle that built and signed cleanly.
app=$(ls -dt ~/Library/Developer/Xcode/DerivedData/axon-*/Build/Products/*-iphoneos/Axon.app 2>/dev/null | head -1)
if [ -n "$app" ] && [ -f "$app/Info.plist" ]; then
  scheme=$(plutil -extract CFBundleURLTypes.0.CFBundleURLSchemes.0 raw -o - "$app/Info.plist" 2>/dev/null || echo "(none)")
  echo "    url scheme:   $scheme"
  echo "    version:      $(plutil -extract CFBundleShortVersionString raw -o - "$app/Info.plist" 2>/dev/null)"
  echo "    build number: $(plutil -extract CFBundleVersion raw -o - "$app/Info.plist" 2>/dev/null)"
  if [ "$scheme" = "(none)" ]; then
    echo "warning: no URL scheme in the bundle — OAuth sign-in will not return" >&2
  fi
fi

if [ "$install_app" -eq 1 ]; then
  echo "==> installing to device"
  if [ -n "$device" ]; then
    xcrun devicectl device install app --device "$device" "$ipa"
  else
    xcrun devicectl device install app "$ipa"
  fi
fi

if [ "$upload" -eq 1 ]; then
  # App Store Connect API key. The .p8 stays in ~/.appstoreconnect/private_keys
  # where altool looks for it; this script never reads it and it is never
  # committed. The issuer is an account identifier rather than a secret, but it
  # is per-account, so it comes from the environment too.
  : "${ASC_KEY_ID:?set ASC_KEY_ID (the A1B2C3D4E5 in ~/.appstoreconnect/private_keys/AuthKey_*.p8)}"
  : "${ASC_ISSUER_ID:?set ASC_ISSUER_ID (App Store Connect > Users and Access > Integrations)}"

  if [ "$export_method" != "app-store-connect" ]; then
    echo "error: --upload needs --export-method app-store-connect; got $export_method" >&2
    exit 1
  fi

  echo "==> uploading to App Store Connect"
  xcrun altool --upload-app --type ios --file "$ipa" \
    --apiKey "$ASC_KEY_ID" --apiIssuer "$ASC_ISSUER_ID"
  echo "    uploaded; TestFlight processing takes a few minutes"
fi

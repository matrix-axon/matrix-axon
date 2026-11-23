#!/usr/bin/env bash
#
# Builds the iOS app for the simulator, installs it, launches it, and checks that
# it is still running some seconds later and left no crash report.
#
#   scripts/ci/ios-launch-check.sh
#
# Why this exists: iOS 27 stops an app that does not adopt the scene lifecycle at
# launch (a SIGTRAP in UIKit), and the first Xcode 27 build of the real app had no
# scene manifest. Nothing launches the real app in CI, and the Mac that builds it
# here cannot run Xcode 27, so a build that cannot start would only be found on a
# device. This runs the built app on whatever iOS the runner's Xcode ships.
#
# It builds the same app the store build does (a regenerated project, the sharing
# configuration, the real Rust and web builds) for the simulator SDK and unsigned, so
# it needs no certificate, profile or App Store Connect key.
#
# How it builds, because the obvious routes do not work. `tauri ios build` always
# archives for a device, and an archive of the simulator's architecture is rejected
# ("none of the architectures in ARCHS are valid"). Running xcodebuild alone fails in
# the project's "Build Rust Code" phase, which reads the options of a running Tauri
# CLI ("the `tauri ios dev` or `tauri ios build` command must be running while Xcode
# builds the app"). `tauri ios build --open` is the mode for exactly that: it serves
# the options, asks the `open` command to open Xcode, and sleeps. A no-op `open` first
# on its PATH leaves it serving while this runs xcodebuild itself, for the simulator.
#
# Environment:
#   AXON_LAUNCH_DIAGNOSTICS   a folder to copy the app's Info.plist facts, any crash
#                             reports, a screenshot and a short log into, for a CI
#                             artifact. Unset, nothing is copied.
#   LAUNCH_CHECK_SETTLE_SECONDS   how long to let the app run before judging it
#                             (default 20).
#   LAUNCH_CHECK_IOS          the iOS to launch on, such as 26.0; the SDK's by default.
#                             It is how an app built with a newer SDK is tried on the
#                             oldest iOS it claims to support
#                             (bundle.iOS.minimumSystemVersion). A runtime that is not
#                             installed is downloaded.
#
# Exit status: 0 if the app was running after it settled and no crash report was
# written; 1 if it was not, or crashed; 2 for a problem with the setup.
set -euo pipefail

# die <message> [status]: status 2, a problem with the setup, unless told otherwise.
die() {
  if [ "${GITHUB_ACTIONS:-}" = "true" ]; then echo "::error::$1" >&2; else echo "error: $1" >&2; fi
  exit "${2:-2}"
}
note() { echo "==> $*"; }

repo_root=$(CDPATH="" cd -- "$(dirname "$0")/../.." && pwd)
web_dir="$repo_root/clients/web"
tauri_dir="$web_dir/src-tauri"
settle=${LAUNCH_CHECK_SETTLE_SECONDS:-20}
diagnostics=${AXON_LAUNCH_DIAGNOSTICS:-}
bundle_id=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["identifier"])' "$tauri_dir/tauri.conf.json")

# The simulator's architecture is the host's: arm64 on Apple silicon, x86_64 on Intel.
arch=$(uname -m)
case "$arch" in
  arm64|x86_64) ;;
  *) die "no simulator architecture for a $arch host" ;;
esac

export PATH="$HOME/.cargo/bin:$PATH"
unset FORCE_COLOR
started_marker=$(mktemp "${TMPDIR:-/var/tmp}/ios-launch-check.XXXXXX")
created_sim=""
trap 'rm -f "$started_marker"; [ -z "$created_sim" ] || xcrun simctl delete "$created_sim" >/dev/null 2>&1 || true' EXIT

# Same regeneration as package-ios.sh and for the same reason: the generated project
# is written once and then left alone, so a stale one would be built instead of the
# one the configuration describes. `gen/` is generated and gitignored.
note "regenerating the iOS project"
rm -rf "$tauri_dir/gen/apple"
(cd "$web_dir" && pnpm tauri ios init --ci)

note "installing the sharing configuration"
AXON_APS_ENVIRONMENT=development AXON_IOS_EXPORT_METHOD=debugging \
  node "$web_dir/scripts/configure-ios-sharing.mjs"

note "building the web client"
(cd "$web_dir" && pnpm build)

# The Tauri CLI, left running in its own session (so the whole tree can be stopped)
# with an `open` that does nothing in front of the real one.
note "starting the Tauri CLI in --open mode"
noop_bin=$(mktemp -d "${TMPDIR:-/var/tmp}/ios-launch-check-bin.XXXXXX")
printf '#!/bin/sh\nexit 0\n' >"$noop_bin/open"
chmod +x "$noop_bin/open"
cli_log="$noop_bin/tauri-cli.log"
options_file="$tauri_dir/gen/apple/.tauri/cli-options-server.json"
# `setsid` makes it a session leader, so its pid is the id of a group that holds
# everything it starts; it records the pid itself because `$!` is only the subshell's.
cli_pidfile="$noop_bin/cli.pid"
(cd "$web_dir" && PATH="$noop_bin:$PATH" python3 -c '
import os, sys
os.setsid()
open(sys.argv[1], "w").write(str(os.getpid()))
os.execvp(sys.argv[2], sys.argv[2:])
' "$cli_pidfile" pnpm tauri ios build --open --ci >"$cli_log" 2>&1) &
cli_subshell=$!
stop_cli() {
  if [ -s "$cli_pidfile" ]; then
    kill -TERM -- "-$(cat "$cli_pidfile")" 2>/dev/null || true
  fi
  kill -TERM "$cli_subshell" 2>/dev/null || true
}
trap 'stop_cli; rm -rf "$noop_bin"; rm -f "$started_marker"; [ -z "$created_sim" ] || xcrun simctl delete "$created_sim" >/dev/null 2>&1 || true' EXIT

waited=0
until [ -f "$options_file" ]; do
  if ! kill -0 "$cli_subshell" 2>/dev/null; then
    tail -n 40 "$cli_log" >&2 || true
    die "the Tauri CLI exited before serving its options"
  fi
  [ "$waited" -lt 300 ] || { tail -n 40 "$cli_log" >&2 || true; die "the Tauri CLI did not serve its options within 300s"; }
  sleep 2
  waited=$((waited + 2))
done
note "the Tauri CLI is serving its options"

note "building for the simulator ($arch)"
derived="$tauri_dir/gen/apple/launch-check-build"
rm -rf "$derived"
xcodebuild build \
  -project "$tauri_dir/gen/apple/axon.xcodeproj" \
  -scheme axon_iOS \
  -configuration release \
  -sdk iphonesimulator \
  -destination "generic/platform=iOS Simulator" \
  -derivedDataPath "$derived" \
  ARCHS="$arch" ONLY_ACTIVE_ARCH=NO \
  CODE_SIGNING_ALLOWED=NO
stop_cli

app=$(find "$derived/Build/Products" -maxdepth 2 -type d -name '*.app' -path '*iphonesimulator*' | head -1)
[ -n "$app" ] || die "the build reported success but left no .app under $derived/Build/Products"
plist="$app/Info.plist"
note "built $app"

plist_value() { plutil -extract "$1" raw -o - "$plist" 2>/dev/null || echo "(none)"; }
facts=$(
  echo "bundle id:             $(plist_value CFBundleIdentifier)"
  echo "MinimumOSVersion:      $(plist_value MinimumOSVersion)"
  echo "DTSDKName:             $(plist_value DTSDKName)"
  echo "DTXcode:               $(plist_value DTXcode)"
  if plutil -extract UIApplicationSceneManifest raw -o - "$plist" >/dev/null 2>&1; then
    echo "scene manifest:        declared"
  else
    echo "scene manifest:        NONE"
  fi
  echo "configured minimum:    $(node "$web_dir/scripts/ios-minimum-version.mjs")"
  echo "executable minos/sdk:  $(xcrun vtool -show-build "$app/$(plist_value CFBundleExecutable)" 2>/dev/null | awk '/minos|sdk/ { printf "%s ", $0 }' | tr -s ' ')"
)
echo "$facts" | sed 's/^/    /'

# The simulator to launch on: one on the iOS the SDK ships, or on LAUNCH_CHECK_IOS.
# A standard phone model is preferred (as in test-sharing-simulator.mjs: the first
# Xcode 27 runtime offered only "iPhone Duo", whose short screen the tests were not
# written for), and one is made from the runtime's device types when it has none.
sdk=$(xcrun --sdk iphonesimulator --show-sdk-version)
target_ios=${LAUNCH_CHECK_IOS:-$sdk}
note "installed simulator runtimes"
xcrun simctl list runtimes | sed 's/^/    /'

# Prints "device <udid> <name>", "create <type> <runtime> <name>" or "none".
pick_simulator() {
  python3 - "$target_ios" <<'PY'
import json, re, subprocess, sys

want = ".".join(sys.argv[1].split(".")[:2])

def simctl(*args):
    return json.loads(subprocess.check_output(["xcrun", "simctl", *args, "-j"]))

def rank(name):
    if re.fullmatch(r"iPhone \d+ Pro", name): return 0
    if re.fullmatch(r"iPhone \d+", name): return 1
    if re.match(r"iPhone \d+", name): return 2
    return 3

def model(name):
    found = re.match(r"iPhone (\d+)", name)
    return int(found.group(1)) if found else 0

runtimes = [
    r for r in simctl("list", "runtimes")["runtimes"]
    if r.get("isAvailable", True)
    and ".".join(r["version"].split(".")[:2]) == want
    and r.get("identifier", "").startswith("com.apple.CoreSimulator.SimRuntime.iOS")
]
if not runtimes:
    print("none")
    sys.exit()
runtime = runtimes[0]
devices = simctl("list", "devices", "available")["devices"].get(runtime["identifier"], [])
phones = [d for d in devices if d["name"].startswith("iPhone")]
if phones:
    best = min(phones, key=lambda d: (rank(d["name"]), d["state"] != "Booted", -model(d["name"])))
    print("device", best["udid"], best["name"])
    sys.exit()
types = [t for t in runtime.get("supportedDeviceTypes", []) if t["name"].startswith("iPhone")]
if types:
    best = min(types, key=lambda t: (rank(t["name"]), -model(t["name"])))
    print("create", best["identifier"], runtime["identifier"], best["name"])
    sys.exit()
print("none")
PY
}

choice=$(pick_simulator)
if [ "${choice%% *}" = none ] && [ -n "${LAUNCH_CHECK_IOS:-}" ]; then
  note "no iOS $target_ios simulator runtime is installed; downloading it"
  xcodebuild -downloadPlatform iOS -buildVersion "$target_ios" || echo "    the download failed" >&2
  choice=$(pick_simulator)
fi
read -r kind first second rest <<EOF
$choice
EOF
case "$kind" in
  device)
    udid=$first
    sim_name=$second${rest:+ $rest}
    ;;
  create)
    sim_name=$rest
    udid=$(xcrun simctl create "Axon launch check" "$first" "$second") || die "could not create a $sim_name simulator on $second"
    created_sim=$udid
    ;;
  *) die "no iPhone simulator for iOS $target_ios (the SDK is $sdk)" ;;
esac
note "simulator $sim_name $udid (iOS $target_ios, SDK $sdk)"
xcrun simctl boot "$udid" 2>/dev/null || true
xcrun simctl bootstatus "$udid" -b >/dev/null

xcrun simctl terminate "$udid" "$bundle_id" 2>/dev/null || true
xcrun simctl uninstall "$udid" "$bundle_id" 2>/dev/null || true
xcrun simctl install "$udid" "$app"

# Crash reports newer than this marker are this launch's.
touch "$started_marker"
note "launching $bundle_id"
launch=$(xcrun simctl launch "$udid" "$bundle_id" 2>&1) || die "simctl could not launch it: $launch" 1
echo "    $launch"
sleep "$settle"

# The app's process id from the simulator's launchd: a number while it runs, "-" once
# it has exited.
pid=$(
  xcrun simctl spawn "$udid" launchctl list 2>/dev/null \
    | awk -v id="UIKitApplication:$bundle_id" 'index($3, id) == 1 { print $1; exit }'
)
reports_dir="$HOME/Library/Logs/DiagnosticReports"
crashes=$(find "$reports_dir" -maxdepth 1 -type f \( -name '*.ips' -o -name '*.crash' \) -newer "$started_marker" 2>/dev/null \
  | grep -i -e "/axon" -e "$bundle_id" || true)

if [ -n "$diagnostics" ]; then
  mkdir -p "$diagnostics"
  echo "$facts" >"$diagnostics/app-facts.txt"
  cp "$plist" "$diagnostics/Info.plist" 2>/dev/null || true
  xcrun simctl io "$udid" screenshot "$diagnostics/after-launch.png" >/dev/null 2>&1 || true
  xcrun simctl spawn "$udid" log show --last 3m --style compact \
    --predicate "process == \"$(plist_value CFBundleExecutable)\" OR eventMessage CONTAINS \"$bundle_id\"" \
    2>/dev/null | head -n 600 >"$diagnostics/simulator.log" || true
  if [ -n "$crashes" ]; then
    mkdir -p "$diagnostics/crash-reports"
    echo "$crashes" | while IFS= read -r report; do cp "$report" "$diagnostics/crash-reports/"; done
  fi
fi

if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  {
    echo "### iOS launch check"
    echo
    echo '```'
    echo "$facts"
    echo "simulator:             $sim_name, iOS $target_ios (SDK $sdk)"
    echo "running after ${settle}s:    ${pid:--}"
    echo '```'
  } >>"$GITHUB_STEP_SUMMARY"
fi

# Whatever SDK built it, the app declares the minimum iOS the configuration asks for.
expected_min=$(node "$web_dir/scripts/ios-minimum-version.mjs")
actual_min=$(plist_value MinimumOSVersion)
if [ "$actual_min" != "$expected_min" ]; then
  echo "error: the app declares a minimum iOS of $actual_min, not the $expected_min that bundle.iOS.minimumSystemVersion asks for" >&2
  exit 1
fi
if [ -n "$crashes" ]; then
  echo "error: the app crashed; crash report(s):" >&2
  echo "$crashes" | sed 's/^/    /' >&2
  exit 1
fi
case "$pid" in
  ''|-) die "the app was not running ${settle}s after launch, and wrote no crash report" 1 ;;
esac
note "the app was still running after ${settle}s (pid $pid) and left no crash report"

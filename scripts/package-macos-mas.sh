#!/usr/bin/env bash
#
# Build, and optionally upload, the Mac App Store .pkg (ADR 0102, M-W13).
#
# The macOS counterpart of scripts/package-ios.sh, and unrelated to the
# Developer ID .dmg that desktop-build.yml makes. A store build differs in
# every step that matters:
#
#   * It is signed with the "3rd Party Mac Developer Application" (or "Apple
#     Distribution") certificate, not Developer ID, and is not notarized.
#   * It is sandboxed. Entitlements.mas.plist replaces Entitlements.plist.
#   * It embeds a Mac App Store provisioning profile for org.matrixaxon.axon,
#     which this repo does not carry: create one under Certificates,
#     Identifiers & Profiles > Profiles > Distribution > Mac App Store Connect,
#     download it, and pass it with --profile.
#   * It ships as a .pkg signed with the "3rd Party Mac Developer Installer"
#     certificate, made by productbuild.
#
# Not a gate and not run by CI. `--upload` and `--build-number auto` need App
# Store Connect credentials this repo does not carry: ASC_KEY_ID and
# ASC_ISSUER_ID from the environment or .env, and the .p8 in
# ~/.appstoreconnect/private_keys. For an unattended build, AXON_SIGNING_KEYCHAIN
# names a passwordless keychain holding the signing identities; the README's
# "Signing without a password prompt" says how to make one.
set -euo pipefail

# A restrictive umask (027 here) leaves every file in the bundle unreadable to
# other users, and productbuild preserves modes. The installer runs as root and
# the app as the user, so App Store Connect rejects the package with error
# 90255 after the build, the signing and the upload.
umask 022

usage() {
  cat <<'USAGE'
Usage: scripts/package-macos-mas.sh --profile <file> [options]

  --profile <file>       Mac App Store .provisionprofile (or MAS_PROVISIONING_PROFILE)
  --build-number <n>     CFBundleVersion; App Store Connect rejects a reused one.
                         `auto` asks App Store Connect for the highest it has on macOS and
                         uses one more (needs the same credentials as --upload)
  --app-identity <name>  application signing identity (default: first "3rd Party
                         Mac Developer Application" or "Apple Distribution" found)
  --pkg-identity <name>  installer signing identity (default: first "3rd Party
                         Mac Developer Installer" or "Mac Installer Distribution")
  --target <triple>      default universal-apple-darwin
  --upload               upload the .pkg to App Store Connect / TestFlight
  -h, --help             this

ASC_KEY_ID and ASC_ISSUER_ID, which --upload and `--build-number auto` need, are
taken from the environment or, failing that, from .env at the repository root.
AXON_SIGNING_KEYCHAIN names a keychain to unlock first, for a headless build.

Example:
  scripts/package-macos-mas.sh --profile ~/Downloads/Axon_MAS.provisionprofile \
    --build-number auto --upload
USAGE
}

profile="${MAS_PROVISIONING_PROFILE:-}"
build_number=""
app_identity=""
pkg_identity=""
target="universal-apple-darwin"
upload=0

while [ $# -gt 0 ]; do
  case "$1" in
    --profile) profile="${2:?--profile needs a file}"; shift ;;
    --build-number) build_number="${2:?--build-number needs a value}"; shift ;;
    --app-identity) app_identity="${2:?--app-identity needs a name}"; shift ;;
    --pkg-identity) pkg_identity="${2:?--pkg-identity needs a name}"; shift ;;
    --target) target="${2:?--target needs a triple}"; shift ;;
    --upload) upload=1 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

# The App Store Connect and signing-keychain logic is shared with
# package-ios.sh, so the two cannot drift apart.
. "$repo_root/scripts/lib/asc.sh"
. "$repo_root/scripts/lib/signing-keychain.sh"

# Everything below that can be checked cheaply is checked before the build.
if [ -z "$profile" ] || [ ! -f "$profile" ]; then
  echo "error: --profile must name a Mac App Store .provisionprofile; got '${profile}'" >&2
  exit 2
fi
# Absolute, because `pnpm tauri build` runs after a cd into the web client and Tauri resolves the
# profile named in --config from there: a relative path passes the checks above and the profile
# validation below, then fails at bundle time, minutes into a universal build.
profile=$(cd "$(dirname "$profile")" && pwd)/$(basename "$profile")
if [ -n "$build_number" ] && [ "$build_number" != "auto" ] && ! [[ $build_number =~ ^[0-9]+(\.[0-9]+){0,2}$ ]]; then
  echo "error: --build-number must be one to three dot-separated numbers (CFBundleVersion); got '$build_number'" >&2
  exit 2
fi
if [ "$upload" -eq 1 ] || [ "$build_number" = "auto" ]; then
  asc_require_credentials "$repo_root"
fi

# Before the identities are looked for, and well before anything signs: a locked
# keychain fails at the codesign or productbuild step, minutes in, with an error
# that does not say "locked". A no-op unless AXON_SIGNING_KEYCHAIN is set; see
# lib/signing-keychain.sh.
unlock_signing_keychain || exit 1

# `|| true`: grep finding nothing must reach the diagnostic, not abort here.
find_identity() {
  security find-identity -v | grep -E "\"($1)" | head -1 | sed -E 's/.*"([^"]+)".*/\1/' || true
}
[ -n "$app_identity" ] || app_identity=$(find_identity '3rd Party Mac Developer Application|Apple Distribution')
[ -n "$pkg_identity" ] || pkg_identity=$(find_identity '3rd Party Mac Developer Installer|Mac Installer Distribution')
if [ -z "$app_identity" ] || [ -z "$pkg_identity" ]; then
  echo "error: need both an application and an installer signing identity in the keychain" >&2
  echo "       application: '${app_identity}'  installer: '${pkg_identity}'" >&2
  exit 1
fi
team_id=$(printf '%s' "$app_identity" | sed -E 's/.*\(([A-Z0-9]{10})\)$/\1/')
# An identity given with --app-identity as a bare hash or a custom name has no "(TEAMID)" suffix; the
# sed leaves it unchanged, and the profile check below would then blame the profile for it.
if ! [[ $team_id =~ ^[A-Z0-9]{10}$ ]]; then
  echo "error: cannot read a team ID from the application identity '$app_identity'" >&2
  echo "       it must end in the team ID in parentheses, like 'Apple Distribution: Name (ABCDE12345)'" >&2
  exit 2
fi

# The profile must be for this app, this team and this platform, and unexpired;
# App Store Connect reports a mismatch only after the upload, in an email.
profile_plist=$(mktemp)
trap 'rm -f "$profile_plist"' EXIT
security cms -D -i "$profile" > "$profile_plist"
p_appid=$(plutil -extract Entitlements.com\\.apple\\.application-identifier raw -o - "$profile_plist" 2>/dev/null || true)
p_platform=$(plutil -extract Platform.0 raw -o - "$profile_plist" 2>/dev/null || true)
if [ "$p_appid" != "$team_id.org.matrixaxon.axon" ]; then
  echo "error: profile is for '$p_appid', need '$team_id.org.matrixaxon.axon'" >&2
  exit 1
fi
if [ "$p_platform" != "OSX" ]; then
  echo "error: profile platform is '$p_platform', need OSX (a Mac profile, not iOS)" >&2
  exit 1
fi
if [ "$(plutil -extract ProvisionsAllDevices raw -o - "$profile_plist" 2>/dev/null || echo false)" = "true" ]; then
  echo "error: that is a Developer ID profile; need Mac App Store Connect" >&2
  exit 1
fi

# The profile must also list the certificate that will sign. The portal shows
# several certificates with identical names and near-identical expiry dates, so
# picking the wrong one is easy, and the mismatch surfaces only when the upload
# is rejected. Any keychain certificate with the identity's name may be the
# signer, so a match against any of them passes.
keychain_hashes=$(security find-certificate -a -c "$app_identity" -Z | awk '/^SHA-1 hash:/{print $3}')
profile_match=0
i=0
while cert_b64=$(plutil -extract "DeveloperCertificates.$i" raw -o - "$profile_plist" 2>/dev/null); do
  h=$(printf '%s' "$cert_b64" | base64 -d | openssl x509 -inform der -noout -fingerprint -sha1 | sed 's/.*=//; s/://g')
  if printf '%s\n' "$keychain_hashes" | grep -qix "$h"; then profile_match=1; break; fi
  i=$((i + 1))
done
if [ "$profile_match" -ne 1 ]; then
  echo "error: the profile does not include any '$app_identity' certificate in this keychain" >&2
  echo "       regenerate it selecting the certificate that expires on the same date as the keychain one" >&2
  exit 1
fi

web_dir="$repo_root/clients/web"
tauri_dir="$web_dir/src-tauri"

export PATH="$HOME/.cargo/bin:$PATH"

# A universal build needs both Apple targets; check rather than let cargo say
# "can't find crate for 'std'" halfway through.
sysroot=$(rustc --print sysroot)
# A `case` inside `$( )` does not parse under bash 3.2, which is macOS's /bin/bash and
# what a runner's `#!/usr/bin/env bash` finds: "syntax error near unexpected token `;;'".
# It parsed everywhere it had been run, since that was Homebrew's bash 5, so it was
# found by the first CI run. The case is outside the substitution for that reason.
case "$target" in
  universal-apple-darwin) rust_targets="aarch64-apple-darwin x86_64-apple-darwin" ;;
  *) rust_targets=$target ;;
esac
for t in $rust_targets; do
  if [ ! -d "$sysroot/lib/rustlib/$t" ]; then
    echo "error: Rust target $t is not installed: rustup target add $t" >&2
    exit 1
  fi
done

out_dir=$(mktemp -d "${TMPDIR:-/tmp}/axon-mas.XXXXXX")
entitlements="$out_dir/Entitlements.mas.plist"
sed "s/TEAMID/$team_id/g" "$tauri_dir/Entitlements.mas.plist" > "$entitlements"

# Resolve `auto` now, before a multi-minute universal build, so a bad credential
# or an unreachable App Store Connect costs seconds. After this `build_number` is
# an ordinary number, and it is spliced into the JSON `--config` below, which is
# why the regex above has already refused anything else. See lib/asc.sh.
if [ "$build_number" = "auto" ]; then
  build_number=$(asc_next_build_number "$repo_root" "$tauri_dir/tauri.conf.json" macos) || exit 1
fi

# Overrides ride in as --config, as in package-ios.sh, so nothing tracked is
# edited. `files` is relative to Contents/, which is where the store expects
# embedded.provisionprofile. Built with python3 so paths are JSON-escaped.
config=$(python3 - "$app_identity" "$entitlements" "$profile" "$build_number" <<'PY'
import json, sys
ident, ent, prof, bn = sys.argv[1:]
mac = {"signingIdentity": ident, "entitlements": ent, "hardenedRuntime": False,
       "files": {"embedded.provisionprofile": prof}}
if bn:
    mac["bundleVersion"] = bn
print(json.dumps({"bundle": {"targets": ["app"], "macOS": mac}}))
PY
)

cd "$web_dir"
echo "==> building ($target, signed as $app_identity)"
APPLE_SIGNING_IDENTITY="$app_identity" pnpm tauri build --target "$target" --bundles app --config "$config"

app=$(ls -dt "$tauri_dir"/target/"$target"/release/bundle/macos/*.app 2>/dev/null | head -1 || true)
if [ -z "$app" ]; then
  echo "error: the build reported success but produced no .app" >&2
  exit 1
fi
echo "==> built $app"

echo "    version:      $(plutil -extract CFBundleShortVersionString raw -o - "$app/Contents/Info.plist" 2>/dev/null)"
echo "    build number: $(plutil -extract CFBundleVersion raw -o - "$app/Contents/Info.plist" 2>/dev/null)"
[ -f "$app/Contents/embedded.provisionprofile" ] || { echo "error: no embedded.provisionprofile in the bundle" >&2; exit 1; }
codesign --verify --deep --strict "$app"
codesign -d --entitlements - "$app" 2>/dev/null | grep -q app-sandbox \
  || { echo "error: the signed app is not sandboxed" >&2; exit 1; }

# Belt and braces for a bundle whose files were copied in with their own modes,
# e.g. a profile downloaded with 0600. Modes are not part of the code signature
# (the executable bit is preserved by X), so this does not invalidate it.
chmod -R u+rwX,go+rX "$app"
codesign --verify --deep --strict "$app"

pkg="$out_dir/Axon.pkg"
echo "==> productbuild"
productbuild --component "$app" /Applications --sign "$pkg_identity" "$pkg"
pkgutil --check-signature "$pkg" | head -3
echo "==> built $pkg"

if [ "$upload" -eq 1 ]; then
  echo "==> uploading to App Store Connect"
  xcrun altool --upload-app --type macos --file "$pkg" \
    --apiKey "$ASC_KEY_ID" --apiIssuer "$ASC_ISSUER_ID"
  echo "    uploaded; TestFlight processing takes a few minutes"
fi

#!/usr/bin/env bash
#
# The credential plumbing for the store-build workflow (apple-store-build.yml),
# kept out of the YAML so it can be run, and tested, on a Mac.
#
#   store-credentials.sh check-secrets <ios|macos> [--asc]
#   store-credentials.sh xcode [--min-sdk <major>]
#   store-credentials.sh keychain [--require <identity-name[|alternative]>]...
#   store-credentials.sh profile <ENV_VAR> --ext <mobileprovision|provisionprofile>
#                                [--expect <app-store|development>] [--bundle-id <id>]
#                                [--install] [--export-path <VAR>]
#   store-credentials.sh asc-key
#   store-credentials.sh cleanup
#   store-credentials.sh list-profiles [--bundle-id <id>] [<file-or-directory>...]
#
# `list-profiles` is for the person making the secrets, not for CI: it shows the
# provisioning profiles on this Mac, with what kind each is, so the right file
# goes into the right secret.
#
# Secrets arrive as environment variables, never as arguments, so they are not on
# a command line a log could echo. Nothing here prints a secret's value; what it
# prints about a certificate or a profile is its name, which is not secret.
#
# `keychain` and `profile --export-path` also write to $GITHUB_ENV when it is set,
# so a later step sees AXON_SIGNING_KEYCHAIN (what the packaging scripts unlock)
# and the path of a decoded profile.
#
# Bash 3.2 compatible: a macOS runner's `#!/usr/bin/env bash` may find it.
set -euo pipefail

# What each lane needs, as environment variable names.
ios_required="APPLE_STORE_CERTIFICATES APPLE_STORE_CERTIFICATES_PASSWORD IOS_APPSTORE_PROFILE APPLE_TEAM_ID"
# Not required: this Mac archived and exported an App Store build with a
# development profile installed, so the archive step may want one, but that has
# not been confirmed on a clean machine. Installed when present; the first run
# says whether it was needed.
ios_optional="IOS_DEVELOPMENT_PROFILE"
macos_required="APPLE_STORE_CERTIFICATES APPLE_STORE_CERTIFICATES_PASSWORD MAC_APPSTORE_PROFILE"
asc_required="ASC_KEY_ID ASC_ISSUER_ID ASC_PRIVATE_KEY"

die() {
  if [ "${GITHUB_ACTIONS:-}" = "true" ]; then
    echo "::error::$*" >&2
  else
    echo "error: $*" >&2
  fi
  exit 1
}

# Writes NAME=VALUE for a later workflow step, if there is one to write to.
export_env() {
  if [ -n "${GITHUB_ENV:-}" ]; then
    printf '%s=%s\n' "$1" "$2" >>"$GITHUB_ENV"
  fi
}

mask() {
  # No-op outside GitHub Actions, where the command means nothing.
  if [ "${GITHUB_ACTIONS:-}" = "true" ]; then
    echo "::add-mask::$1"
  fi
}

# Decodes base64 from stdin. Through openssl, not `base64 -d`/`-D`: those flags
# differ between macOS and GNU, and a secret pasted from `base64` may be wrapped
# or not. Whitespace is stripped first, then it is one line.
decode_b64() {
  tr -d ' \r\n' | openssl base64 -d -A
}

# Where Xcode keeps provisioning profiles, as a colon-separated list. Xcode 16 and
# later writes its own to the UserData folder and has left the old MobileDevice
# folder holding almost nothing, but the old one is the long-standing place a CI
# job installs to. Which one `xcodebuild` reads on a clean machine is not
# something this has verified, so profiles are installed into both and listed
# from both. PROVISIONING_PROFILES_DIRS overrides it, so a test can use folders
# that are not a developer's real ones.
profile_dirs() {
  printf '%s\n' "${PROVISIONING_PROFILES_DIRS:-$HOME/Library/Developer/Xcode/UserData/Provisioning Profiles:$HOME/Library/MobileDevice/Provisioning Profiles}"
}

work_dir() {
  local base=${RUNNER_TEMP:-${TMPDIR:-/tmp}}
  mkdir -p "$base"
  printf '%s\n' "$base"
}

# --- check-secrets -----------------------------------------------------------

cmd_check_secrets() {
  local lane=${1:-} need_asc=0
  [ -n "$lane" ] || die "check-secrets needs a lane: ios or macos"
  shift
  while [ $# -gt 0 ]; do
    case $1 in
      --asc) need_asc=1 ;;
      *) die "check-secrets: unknown option $1" ;;
    esac
    shift
  done
  local required optional
  case $lane in
    ios) required=$ios_required; optional=$ios_optional ;;
    macos) required=$macos_required; optional="" ;;
    *) die "check-secrets: lane must be ios or macos, not '$lane'" ;;
  esac
  if [ "$need_asc" -eq 1 ]; then
    required="$required $asc_required"
  fi

  local name missing="" warn=""
  for name in $required; do
    if [ -z "${!name:-}" ]; then missing="$missing $name"; fi
  done
  for name in $optional; do
    if [ -z "${!name:-}" ]; then warn="$warn $name"; fi
  done
  if [ -n "$warn" ]; then
    echo "note: optional secrets not set:$warn (the build may not need them)"
  fi
  if [ -n "$missing" ]; then
    die "missing repository secrets for the $lane build:$missing. See the README's \"Store builds from GitHub Actions\" for what each is and how to make it."
  fi
  echo "all required secrets for the $lane build are set"
}

# --- xcode -------------------------------------------------------------------

# Selects the newest Xcode whose iOS SDK is at least --min-sdk (default 26) and
# exports DEVELOPER_DIR for later steps. App Store Connect refuses builds made
# with an older SDK, after the build, the signing and the upload have all
# succeeded, so it is cheaper to refuse here.
cmd_xcode() {
  local min=26
  while [ $# -gt 0 ]; do
    case $1 in
      --min-sdk) min=${2:?--min-sdk needs a major version}; shift ;;
      *) die "xcode: unknown option $1" ;;
    esac
    shift
  done
  local best="" best_ver="" app dir ver major
  for app in /Applications/Xcode*.app; do
    [ -d "$app" ] || continue
    dir="$app/Contents/Developer"
    ver=$(DEVELOPER_DIR=$dir xcrun --sdk iphoneos --show-sdk-version 2>/dev/null || true)
    [ -n "$ver" ] || continue
    major=${ver%%.*}
    [ "$major" -ge "$min" ] 2>/dev/null || continue
    if [ -z "$best_ver" ] || [ "$(printf '%s\n%s\n' "$best_ver" "$ver" | sort -V | tail -1)" = "$ver" ]; then
      best=$dir
      best_ver=$ver
    fi
  done
  if [ -z "$best" ]; then
    local have
    have=$(xcrun --sdk iphoneos --show-sdk-version 2>/dev/null || echo "none")
    die "no installed Xcode has an iOS SDK of at least $min (the default one has $have). App Store Connect rejects uploads built with an older SDK."
  fi
  echo "using $best (iOS SDK $best_ver)"
  export_env DEVELOPER_DIR "$best"
}

# --- keychain ----------------------------------------------------------------

cmd_keychain() {
  local required_names=()
  while [ $# -gt 0 ]; do
    case $1 in
      --require) required_names+=("${2:?--require needs an identity name}"); shift ;;
      *) die "keychain: unknown option $1" ;;
    esac
    shift
  done
  : "${APPLE_STORE_CERTIFICATES:?APPLE_STORE_CERTIFICATES is not set}"
  : "${APPLE_STORE_CERTIFICATES_PASSWORD:?APPLE_STORE_CERTIFICATES_PASSWORD is not set}"

  local base kc pw p12 listed
  base=$(work_dir)
  kc="$base/axon-signing.keychain-db"
  pw=$(openssl rand -hex 24)
  mask "$pw"
  mask "$APPLE_STORE_CERTIFICATES_PASSWORD"

  p12=$(mktemp "$base/store-certificates.XXXXXX")
  # shellcheck disable=SC2064
  trap "rm -f '$p12'" EXIT
  printf '%s' "$APPLE_STORE_CERTIFICATES" | decode_b64 >"$p12"
  [ -s "$p12" ] || die "APPLE_STORE_CERTIFICATES decoded to nothing; it should be the base64 of a .p12 file"

  rm -f "$kc"
  security create-keychain -p "$pw" "$kc"
  # Stays unlocked for six hours, long enough for a store build and no longer.
  security set-keychain-settings -lut 21600 "$kc"
  security unlock-keychain -p "$pw" "$kc"

  # Every tool that will use a key must be trusted by it, not only codesign: an
  # installer key that trusts only codesign signs apps and then fails on the
  # package with errKCInteractionNotAllowed. The partition list is a separate
  # control that lets Apple's tools use the key without a dialog.
  #
  # `-f pkcs12` because import guesses the format from the file's extension, and
  # a temp file has none: without it this fails with "Unknown format in import".
  if ! security import "$p12" -f pkcs12 -k "$kc" -P "$APPLE_STORE_CERTIFICATES_PASSWORD" \
      -T /usr/bin/codesign -T /usr/bin/security -T /usr/bin/productsign -T /usr/bin/productbuild >/dev/null; then
    die "could not import APPLE_STORE_CERTIFICATES (wrong APPLE_STORE_CERTIFICATES_PASSWORD, or not a .p12?)"
  fi
  security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$pw" "$kc" >/dev/null

  # Put it first in the search list and keep everything already there:
  # `list-keychains -s` replaces the whole list.
  local args=("$kc")
  while IFS= read -r listed; do
    [ -n "$listed" ] && args+=("$listed")
  done <<EOF
$(security list-keychains -d user | sed -e 's/^ *"//' -e 's/"$//')
EOF
  security list-keychains -d user -s "${args[@]}"

  # `-v` lists only identities that are valid: not expired, chain trusted. A
  # self-signed test identity is neither, hence the escape hatch the tests use.
  local flags="-v" name
  if [ "${SIGNING_KEYCHAIN_ACCEPT_UNTRUSTED:-}" = "1" ]; then flags=""; fi
  echo "identities in the signing keychain:"
  # shellcheck disable=SC2086
  security find-identity $flags "$kc" | grep '"' | sed -E 's/^ +[0-9]+\) [0-9A-F]+ /  /' || true
  local listing alt found
  # shellcheck disable=SC2086
  listing=$(security find-identity $flags "$kc")
  for name in ${required_names[@]+"${required_names[@]}"}; do
    # "A|B" accepts either: the Mac App Store takes more than one certificate type.
    found=0
    while IFS= read -r alt; do
      if printf '%s\n' "$listing" | grep -qF "\"$alt"; then found=1; fi
    done <<EOF
$(printf '%s\n' "$name" | tr '|' '\n')
EOF
    if [ "$found" -eq 0 ]; then
      die "the certificates in APPLE_STORE_CERTIFICATES have no valid identity named '$name...'. Export the identity (certificate and private key) from Keychain Access, not just the certificate, and check it has not expired."
    fi
  done

  export_env AXON_SIGNING_KEYCHAIN "$kc"
  export_env AXON_SIGNING_KEYCHAIN_PASSWORD "$pw"
  echo "signing keychain ready: $kc"
}

# --- profile -----------------------------------------------------------------

cmd_profile() {
  local var=${1:-} ext="" expect="" bundle_id="" install=0 export_as=""
  [ -n "$var" ] || die "profile needs the name of the environment variable holding the base64"
  shift
  while [ $# -gt 0 ]; do
    case $1 in
      --ext) ext=${2:?--ext needs a value}; shift ;;
      --expect) expect=${2:?--expect needs a value}; shift ;;
      --bundle-id) bundle_id=${2:?--bundle-id needs a value}; shift ;;
      --install) install=1 ;;
      --export-path) export_as=${2:?--export-path needs a variable name}; shift ;;
      *) die "profile: unknown option $1" ;;
    esac
    shift
  done
  case $ext in
    mobileprovision | provisionprofile) ;;
    *) die "profile: --ext must be mobileprovision (iOS) or provisionprofile (macOS)" ;;
  esac
  if [ -z "${!var:-}" ]; then die "$var is not set"; fi

  local base file plist
  base=$(work_dir)
  file="$base/$var.$ext"
  plist="$base/$var.plist"
  printf '%s' "${!var}" | decode_b64 >"$file"
  [ -s "$file" ] || die "$var decoded to nothing; it should be the base64 of a .$ext file"
  security cms -D -i "$file" >"$plist" 2>/dev/null || die "$var is not a provisioning profile (it did not decode as one)"

  local name uuid appid expires devices gta all_devices kind
  name=$(plutil -extract Name raw -o - "$plist" 2>/dev/null || echo "?")
  uuid=$(plutil -extract UUID raw -o - "$plist" 2>/dev/null || true)
  [ -n "$uuid" ] || die "$var has no UUID, so it is not a provisioning profile"
  expires=$(plutil -extract ExpirationDate raw -o - "$plist" 2>/dev/null || true)
  appid=$(plutil -extract Entitlements.application-identifier raw -o - "$plist" 2>/dev/null \
    || plutil -extract Entitlements.com\\.apple\\.application-identifier raw -o - "$plist" 2>/dev/null || true)
  gta=$(plutil -extract Entitlements.get-task-allow raw -o - "$plist" 2>/dev/null || echo "false")
  devices=$(plutil -extract ProvisionedDevices raw -o - "$plist" 2>/dev/null || true)
  all_devices=$(plutil -extract ProvisionsAllDevices raw -o - "$plist" 2>/dev/null || echo "false")

  if [ "$gta" = "true" ]; then
    kind="development"
  elif [ "$all_devices" = "true" ]; then
    kind="enterprise-or-developer-id"
  elif [ -n "$devices" ]; then
    kind="ad-hoc"
  else
    kind="app-store"
  fi
  echo "$var: \"$name\" ($kind), app id $appid, expires $expires"

  if [ -n "$expires" ]; then
    if ! python3 - "$expires" <<'PY'
import sys, datetime
exp = datetime.datetime.strptime(sys.argv[1], "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=datetime.timezone.utc)
sys.exit(0 if exp > datetime.datetime.now(datetime.timezone.utc) else 1)
PY
    then
      die "$var is a profile that expired on $expires; regenerate it"
    fi
  fi
  if [ -n "$bundle_id" ]; then
    case $appid in
      *".$bundle_id") ;;
      *) die "$var is for '$appid', not for $bundle_id" ;;
    esac
  fi
  if [ -n "$expect" ] && [ "$kind" != "$expect" ]; then
    die "$var is a $kind profile, but this secret should hold an $expect one. Check the two profile secrets are not swapped."
  fi

  if [ "$install" -eq 1 ]; then
    local dest dirs
    dirs=$(profile_dirs)
    while IFS= read -r dest; do
      [ -n "$dest" ] || continue
      mkdir -p "$dest"
      cp "$file" "$dest/$uuid.$ext"
      printf '%s\n' "$dest/$uuid.$ext" >>"$base/installed-profiles.txt"
    done <<EOF
$(printf '%s\n' "$dirs" | tr ':' '\n')
EOF
    echo "installed as $uuid.$ext"
  fi
  if [ -n "$export_as" ]; then
    export_env "$export_as" "$file"
  fi
}

# --- asc-key -----------------------------------------------------------------

cmd_asc_key() {
  : "${ASC_KEY_ID:?ASC_KEY_ID is not set}"
  : "${ASC_PRIVATE_KEY:?ASC_PRIVATE_KEY is not set}"
  case $ASC_KEY_ID in
    *[!A-Z0-9]* | "") die "ASC_KEY_ID should be the 10 letters and digits in the key's file name (AuthKey_<id>.p8)" ;;
  esac
  local dir="$HOME/.appstoreconnect/private_keys" key
  key="$dir/AuthKey_$ASC_KEY_ID.p8"
  mkdir -p "$dir"
  (umask 077 && printf '%s\n' "$ASC_PRIVATE_KEY" >"$key")
  if ! openssl pkey -in "$key" -noout >/dev/null 2>&1; then
    rm -f "$key"
    die "ASC_PRIVATE_KEY is not a private key. It should be the full contents of the AuthKey_$ASC_KEY_ID.p8 file, including the BEGIN and END lines."
  fi
  printf '%s\n' "$key" >>"$(work_dir)/installed-keys.txt"
  echo "wrote the App Store Connect key for $ASC_KEY_ID"
}

# --- list-profiles -----------------------------------------------------------

# Prints one line per profile found: where it is, its name, its kind, the
# platform and when it expires. Looks in the folders Xcode keeps profiles in and
# in any files or folders named on the command line (a profile downloaded from the
# developer portal is usually in ~/Downloads).
cmd_list_profiles() {
  local bundle_id="" targets=() t f plist
  while [ $# -gt 0 ]; do
    case $1 in
      --bundle-id) bundle_id=${2:?--bundle-id needs a value}; shift ;;
      -*) die "list-profiles: unknown option $1" ;;
      *) targets+=("$1") ;;
    esac
    shift
  done
  while IFS= read -r t; do
    [ -n "$t" ] && targets+=("$t")
  done <<EOF
$(profile_dirs | tr ':' '\n')
EOF

  local base found=0
  base=$(work_dir)
  plist="$base/list-profiles.plist"
  for t in "${targets[@]}"; do
    if [ -d "$t" ]; then
      for f in "$t"/*.mobileprovision "$t"/*.provisionprofile; do
        [ -f "$f" ] || continue
        describe_profile "$f" "$plist" "$bundle_id" && found=1
      done
    elif [ -f "$t" ]; then
      describe_profile "$t" "$plist" "$bundle_id" && found=1
    fi
  done
  if [ "$found" -eq 0 ]; then
    echo "no provisioning profiles found${bundle_id:+ for $bundle_id}" >&2
    return 1
  fi
}

# One line for FILE, or nothing (and a failure) if it is not a profile for the
# bundle ID asked about. Same classification as `profile`.
describe_profile() {
  local file=$1 plist=$2 bundle_id=$3
  security cms -D -i "$file" >"$plist" 2>/dev/null || return 1
  local name appid platform expires gta devices all kind
  name=$(plutil -extract Name raw -o - "$plist" 2>/dev/null || echo "?")
  appid=$(plutil -extract Entitlements.application-identifier raw -o - "$plist" 2>/dev/null \
    || plutil -extract Entitlements.com\\.apple\\.application-identifier raw -o - "$plist" 2>/dev/null || true)
  if [ -n "$bundle_id" ]; then
    case $appid in *".$bundle_id") ;; *) return 1 ;; esac
  fi
  platform=$(plutil -extract Platform.0 raw -o - "$plist" 2>/dev/null || echo "iOS")
  expires=$(plutil -extract ExpirationDate raw -o - "$plist" 2>/dev/null || echo "?")
  gta=$(plutil -extract Entitlements.get-task-allow raw -o - "$plist" 2>/dev/null || echo "false")
  devices=$(plutil -extract ProvisionedDevices raw -o - "$plist" 2>/dev/null || true)
  all=$(plutil -extract ProvisionsAllDevices raw -o - "$plist" 2>/dev/null || echo "false")
  if [ "$gta" = "true" ]; then kind="development"
  elif [ "$all" = "true" ]; then kind="enterprise-or-developer-id"
  elif [ -n "$devices" ]; then kind="ad-hoc"
  else kind="app-store"
  fi
  printf '%s\n' "$file"
  printf '    %s | %s | %s | expires %s\n' "$name" "$kind" "$platform" "$expires"
}

# --- cleanup -----------------------------------------------------------------

# A hosted runner is discarded after the job, so this is for hygiene and for the
# day someone points the workflow at a self-hosted one. It never fails.
cmd_cleanup() {
  local base list f
  base=$(work_dir)
  if [ -n "${AXON_SIGNING_KEYCHAIN:-}" ]; then
    security delete-keychain "$AXON_SIGNING_KEYCHAIN" >/dev/null 2>&1 || true
  fi
  for list in installed-profiles.txt installed-keys.txt; do
    if [ -f "$base/$list" ]; then
      while IFS= read -r f; do
        [ -n "$f" ] && rm -f "$f"
      done <"$base/$list"
      rm -f "$base/$list"
    fi
  done
  echo "cleaned up"
}

# --- dispatch ----------------------------------------------------------------

cmd=${1:-}
[ -n "$cmd" ] || die "usage: store-credentials.sh <check-secrets|xcode|keychain|profile|asc-key|cleanup|list-profiles> ..."
shift
case $cmd in
  check-secrets) cmd_check_secrets "$@" ;;
  xcode) cmd_xcode "$@" ;;
  keychain) cmd_keychain "$@" ;;
  profile) cmd_profile "$@" ;;
  asc-key) cmd_asc_key ;;
  cleanup) cmd_cleanup ;;
  list-profiles) cmd_list_profiles "$@" ;;
  *) die "unknown command '$cmd'" ;;
esac

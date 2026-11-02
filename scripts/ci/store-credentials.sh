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
#                                [--entitlements <plist>]
#   store-credentials.sh asc-key
#   store-credentials.sh cleanup
#   store-credentials.sh list-profiles [--bundle-id <id>] [<file-or-directory>...]
#   store-credentials.sh explain <build-log>
#
# `explain` reads the log of a failed iOS build and says what the signing errors in it
# mean and what to do. `profile --entitlements` fails early when a profile lacks a
# capability the app's entitlements ask for.
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
# The development profile is required, not optional: the archive step looks for
# one. A run on a clean runner without it failed with "Xcode couldn't find any iOS
# App Development provisioning profiles matching <bundle id>", after the keychain,
# the profiles and the API key had all installed without complaint.
ios_required="APPLE_STORE_CERTIFICATES APPLE_STORE_CERTIFICATES_PASSWORD IOS_APPSTORE_PROFILE IOS_DEVELOPMENT_PROFILE APPLE_TEAM_ID"
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

warn() {
  if [ "${GITHUB_ACTIONS:-}" = "true" ]; then
    echo "::warning::$*"
  else
    echo "warning: $*" >&2
  fi
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
  local required
  case $lane in
    ios) required=$ios_required ;;
    macos) required=$macos_required ;;
    *) die "check-secrets: lane must be ios or macos, not '$lane'" ;;
  esac
  if [ "$need_asc" -eq 1 ]; then
    required="$required $asc_required"
  fi

  local name missing=""
  for name in $required; do
    if [ -z "${!name:-}" ]; then missing="$missing $name"; fi
  done
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

# --- what is in a keychain ----------------------------------------------------

# Says what the signing keychain holds, in terms that explain a missing identity:
#
#   * identities (a certificate with its private key), each marked valid or, if
#     not, NOT VALID with the reason `security` gives (expired, chain not trusted);
#   * certificates that have no private key, which cannot sign.
#
# The bare list of valid identities that this replaces could not tell "the .p12
# did not contain it" from "it is there but expired" from "it is there without its
# key", and those have three different fixes. Names only; nothing secret.
describe_keychain() {
  local kc=$1 valid_hashes line hash name status verdict shown_names=""
  valid_hashes=$(security find-identity -v "$kc" 2>/dev/null | awk '/^ +[0-9]+\) [0-9A-F]+ "/ { print $2 }')

  echo "identities in the signing keychain (certificate with its private key):"
  local any=0
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    any=1
    hash=$(printf '%s\n' "$line" | awk '{ print $2 }')
    name=$(printf '%s\n' "$line" | sed -E 's/^ +[0-9]+\) [0-9A-F]+ "([^"]*)".*$/\1/')
    status=$(printf '%s\n' "$line" | sed -nE 's/^.*" \((.*)\)$/\1/p')
    if printf '%s\n' "$valid_hashes" | grep -qx "$hash"; then
      verdict="valid"
    else
      verdict="NOT VALID${status:+ ($status)}"
    fi
    printf '  "%s"  %s\n' "$name" "$verdict"
    shown_names="$shown_names|$name"
  done <<EOF
$(security find-identity "$kc" 2>/dev/null | awk '
  /^ +[0-9]+\) [0-9A-F]+ "/ {
    h = $2
    # A valid identity is listed twice (matching, then valid-only); an invalid one
    # once, with its reason. Keep the line that carries a reason when there is one.
    if (!(h in line) || $0 ~ /\(CSSM/) line[h] = $0
  }
  END { for (h in line) print line[h] }')
EOF
  if [ "$any" -eq 0 ]; then echo "  (none)"; fi

  # Signing certificates are labelled "<type>: <name> (<team>)"; the CA
  # certificates that come with a .p12 are not, so the ": " keeps them out.
  local orphans=""
  while IFS= read -r name; do
    [ -n "$name" ] || continue
    case "$shown_names|" in *"|$name|"*) continue ;; esac
    orphans="$orphans$name\n"
  done <<EOF
$(security find-certificate -a "$kc" 2>/dev/null | sed -nE 's/^ *"labl"<blob>="([^"]*: [^"]*)"$/\1/p' | sort -u)
EOF
  if [ -n "$orphans" ]; then
    echo "certificates in the signing keychain with no private key (they cannot sign):"
    printf '%b' "$orphans" | sed 's/^/  "/; s/$/"/'
  fi
}

# --- importing a .p12 ---------------------------------------------------------

# Imports every certificate in the .p12 a second time, each on its own as a
# certificate with no key.
#
# Apple issues several certificate types for one private key (Apple Development and
# 3rd Party Mac Developer Application here), and `security export` writes that key
# into the .p12 once per identity. `security import` accepts the key the first time
# and, on seeing it again, skips the second identity entirely, certificate included,
# with no error and no mention in the log. The keychain then has one identity too
# few and nothing says so: a Mac build failed for want of an identity that was in the
# .p12 the whole time. Reproduced locally with a real export, and with throwaway
# certificates sharing one key; the one dropped is not predictable.
#
# A certificate imported by itself pairs with the private key already in the keychain,
# which restores the identity. Certificates that are already there are reported as
# "already exists", which is the normal case and is ignored. Best effort: if
# the .p12 cannot be listed, the first import has still happened, and the keychain
# listing that follows says what is missing.
reimport_certificates() {
  local p12=$1 kc=$2 base dir f out ossl=/usr/bin/openssl
  # macOS's own openssl, not Homebrew's: OpenSSL 3 cannot read the MAC that `security
  # export` writes without legacy flags.
  [ -x "$ossl" ] || ossl=openssl
  base=$(work_dir)
  dir=$(mktemp -d "$base/p12-certificates.XXXXXX")
  "$ossl" pkcs12 -in "$p12" -nokeys -passin env:APPLE_STORE_CERTIFICATES_PASSWORD 2>/dev/null \
    | awk -v d="$dir" 'BEGIN { n = 0 } /BEGIN CERTIFICATE/ { n++; f = sprintf("%s/c%d.pem", d, n) } n { print > f }' || true
  if ! ls "$dir"/c*.pem >/dev/null 2>&1; then
    echo "note: could not list the certificates in the .p12 to import them again; an identity that shares a private key with another may be missing"
    rm -rf "$dir"
    return 0
  fi
  for f in "$dir"/c*.pem; do
    out=$(security import "$f" -f openssl -k "$kc" 2>&1) || case $out in
      *"already exists"*) ;;
      *) echo "note: importing a certificate from the .p12 on its own failed: $out" ;;
    esac
  done
  rm -rf "$dir"
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
  reimport_certificates "$p12" "$kc"
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
  describe_keychain "$kc"
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
  local var=${1:-} ext="" expect="" bundle_id="" install=0 export_as="" entitlements=""
  [ -n "$var" ] || die "profile needs the name of the environment variable holding the base64"
  shift
  while [ $# -gt 0 ]; do
    case $1 in
      --ext) ext=${2:?--ext needs a value}; shift ;;
      --expect) expect=${2:?--expect needs a value}; shift ;;
      --bundle-id) bundle_id=${2:?--bundle-id needs a value}; shift ;;
      --install) install=1 ;;
      --export-path) export_as=${2:?--export-path needs a variable name}; shift ;;
      --entitlements) entitlements=${2:?--entitlements needs a plist path}; shift ;;
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

  # The generated Xcode project signs automatically, and automatic signing looks a
  # profile up by Xcode's own name for it, "iOS Team Provisioning Profile: <bundle id>"
  # (development) or "iOS Team Store Provisioning Profile: <bundle id>" (app-store). A
  # profile made by hand in the developer portal fails even when it lists the same
  # certificate and entitlements, and only at export or archive, as "No profiles for ...
  # were found" under a misleading "No Accounts". Seen twice: a hand-made development
  # profile ("Xcode couldn't find any iOS App Development provisioning profiles") and a
  # hand-made App Store one in IOS_APPSTORE_PROFILE. Fatal, since the build cannot succeed.
  # Which profile names Xcode accepts is inferred from those failures, not documented.
  #
  # After the swap check, so a profile of the wrong kind is told that first, and the name
  # to look for comes from what this secret should hold, not from what the file is.
  if [ -n "$expect" ] && [ "$kind" != "$expect" ]; then
    die "$var is a $kind profile, but this secret should hold an $expect one. Check the two profile secrets are not swapped."
  fi
  if [ "$ext" = "mobileprovision" ] \
      && [ "$(plutil -extract IsXcodeManaged raw -o - "$plist" 2>/dev/null || echo false)" != "true" ]; then
    local want="an Xcode-managed profile"
    case ${expect:-$kind} in
      app-store) want="\"iOS Team Store Provisioning Profile: ${bundle_id:-<bundle id>}\"" ;;
      development) want="\"iOS Team Provisioning Profile: ${bundle_id:-<bundle id>}\"" ;;
    esac
    die "$var holds \"$name\", a $kind profile that is not Xcode-managed. The Xcode project signs automatically and looks the profile up by Xcode's own name ($want), so a profile made by hand in the developer portal fails the build with \"No profiles for ... were found\". Use the Xcode-managed one: \`store-credentials.sh list-profiles\` marks it. Steps: \"When a capability change invalidates the iOS profiles\" in clients/web/src-tauri/README.md."
  fi

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

  # Every capability the app asks for must be in the profile. A profile made before a
  # capability was enabled on the App ID lacks it, and Xcode reports that minutes into
  # the build. A list must be covered item by item: an entry matches exactly, or when it
  # ends in "*" and is a prefix of the item ("<team>.*"). A lone "*" covers nothing, so
  # it cannot excuse an unrelated entitlement. The app's keychain groups start with
  # "$(AppIdentifierPrefix)", which is the team id and a dot, so that is expanded first
  # and an absent keychain-access-groups fails like any other absent capability.
  # Anything that is not a list only has to be present, because aps-environment is
  # "development" in the app and "production" in the Store profile.
  if [ -n "$entitlements" ]; then
    [ -f "$entitlements" ] || die "profile: --entitlements $entitlements is not a file"
    local missing
    missing=$(python3 - "$entitlements" "$plist" <<'PY'
import plistlib, sys
want = plistlib.load(open(sys.argv[1], "rb"))
profile = plistlib.load(open(sys.argv[2], "rb"))
have = profile.get("Entitlements", {})
team = (profile.get("ApplicationIdentifierPrefix") or [have.get("application-identifier", "").split(".")[0]])[0]


def covers(entry, item):
    return entry == item or (entry != "*" and entry.endswith("*") and item.startswith(entry[:-1]))


for key, value in want.items():
    if key not in have:
        print(key)
    elif isinstance(value, list):
        got = have[key] if isinstance(have[key], list) else [have[key]]
        for item in value:
            item = item.replace("$(AppIdentifierPrefix)", team + ".")
            if not any(covers(entry, item) for entry in got):
                print("%s = %s" % (key, item))
PY
    ) || die "profile: could not compare $var with $entitlements"
    if [ -n "$missing" ]; then
      die "$var (\"$name\") does not have what the app asks for in $entitlements: $(printf '%s' "$missing" | tr '\n' ';' | sed 's/;$//; s/;/; /g'). It was made before that capability was enabled on the App ID. Enable it in the developer portal if it is not yet, then regenerate the Xcode-managed profile and replace the secret: \"When a capability change invalidates the iOS profiles\" in clients/web/src-tauri/README.md."
    fi
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
  saw_osx=0
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
  fi
  # The Mac App Store profile is a file downloaded from the developer portal, not
  # one Xcode installs, so it is the profile most likely to be missing from this
  # list for a reason that is not obvious.
  if [ "$saw_osx" -eq 0 ]; then
    echo "note: no macOS profile (.provisionprofile) found. A Mac App Store profile is a file downloaded from the developer portal, not one Xcode installs: name the folder or file you saved it in, e.g. list-profiles${bundle_id:+ --bundle-id $bundle_id} ~/Downloads" >&2
  fi
  [ "$found" -eq 1 ]
}

# One line for FILE, or nothing (and a failure) if it is not a profile for the
# bundle ID asked about. Same classification as `profile`.
describe_profile() {
  local file=$1 plist=$2 bundle_id=$3
  security cms -D -i "$file" >"$plist" 2>/dev/null || return 1
  local name appid platform expires gta devices all kind managed
  name=$(plutil -extract Name raw -o - "$plist" 2>/dev/null || echo "?")
  appid=$(plutil -extract Entitlements.application-identifier raw -o - "$plist" 2>/dev/null \
    || plutil -extract Entitlements.com\\.apple\\.application-identifier raw -o - "$plist" 2>/dev/null || true)
  if [ -n "$bundle_id" ]; then
    case $appid in *".$bundle_id") ;; *) return 1 ;; esac
  fi
  platform=$(plutil -extract Platform.0 raw -o - "$plist" 2>/dev/null || echo "iOS")
  if [ "$platform" = "OSX" ]; then saw_osx=1; fi
  expires=$(plutil -extract ExpirationDate raw -o - "$plist" 2>/dev/null || echo "?")
  gta=$(plutil -extract Entitlements.get-task-allow raw -o - "$plist" 2>/dev/null || echo "false")
  devices=$(plutil -extract ProvisionedDevices raw -o - "$plist" 2>/dev/null || true)
  all=$(plutil -extract ProvisionsAllDevices raw -o - "$plist" 2>/dev/null || echo "false")
  if [ "$gta" = "true" ]; then kind="development"
  elif [ "$all" = "true" ]; then kind="enterprise-or-developer-id"
  elif [ -n "$devices" ]; then kind="ad-hoc"
  else kind="app-store"
  fi
  # Xcode-managed profiles are the ones automatic signing will pick up; one made by
  # hand in the developer portal is "manual" and, for development, is ignored by it.
  if [ "$(plutil -extract IsXcodeManaged raw -o - "$plist" 2>/dev/null || echo false)" = "true" ]; then
    managed="Xcode-managed"
  else
    managed="manual"
  fi
  printf '%s\n' "$file"
  printf '    %s | %s | %s | %s | expires %s\n' "$name" "$kind" "$platform" "$managed" "$expires"
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

# --- explain -----------------------------------------------------------------

# Reads the log of a failed xcodebuild/tauri run and says what to do about the
# signing failures it recognises. Xcode's first line is usually "No Accounts", which is
# only its follow-up to the real cause listed after it, and the real cause is a stale or
# wrong provisioning profile, not a missing account. Prints nothing and returns 1 when
# it recognises nothing, so a caller can say so instead of implying the log was read.
cmd_explain() {
  local log=${1:-}
  [ -n "$log" ] && [ -f "$log" ] || die "explain needs the path of a build log"
  local readme="clients/web/src-tauri/README.md"
  local runbook="\"When a capability change invalidates the iOS profiles\" in $readme"
  local caps notfound accounts cert mismatch nodev
  caps=$(grep -E "Provisioning profile \".*\" (doesn't include the .*(capability|entitlement)|doesn't support the .*(capability|App Group))" "$log" | sed -E 's/^.*error: //' | sort -u || true)
  cert=$(grep -E "Provisioning profile \".*\" doesn't include signing certificate" "$log" | sed -E 's/^.*error: //' | sort -u || true)
  mismatch=$(grep -E "doesn't match the entitlements file's value for the .* entitlement" "$log" | sed -E 's/^.*error: //' | sort -u || true)
  nodev=$(grep -E "couldn't find any iOS App Development provisioning profiles" "$log" | sed -E 's/^.*error: //' | sort -u || true)
  notfound=$(grep -E "No profiles for '.*' were found" "$log" | sed -E 's/^.*error: //' | sort -u || true)
  accounts=$(grep -E "error: (exportArchive )?No Accounts" "$log" | head -1 || true)
  [ -n "$caps$cert$mismatch$nodev$notfound$accounts" ] || return 1

  {
    echo
    echo "==> iOS signing failed. What the log says, and what to do:"
    if [ -n "$caps" ]; then
      echo
      echo "$caps" | sed 's/^/    /'
      echo
      echo "  The provisioning profile was made before that capability was enabled on the App ID,"
      echo "  so it does not carry it. Enabling a capability invalidates every profile made earlier."
      echo "  1. In the developer portal, check the App ID has the capability (and, for an App"
      echo "     Group, that the group is assigned to it)."
      echo "  2. Regenerate the Xcode-managed profiles on a Mac signed in to the account"
      echo "     (scripts/package-ios.sh does it), then replace BOTH secrets,"
      echo "     IOS_APPSTORE_PROFILE and IOS_DEVELOPMENT_PROFILE, from those files."
      echo "     \`scripts/ci/store-credentials.sh list-profiles\` finds them."
      echo "  Steps: $runbook"
    fi
    if [ -n "$cert" ]; then
      echo
      echo "$cert" | sed 's/^/    /'
      echo
      echo "  The profile does not list the distribution certificate in APPLE_STORE_CERTIFICATES. An"
      echo "  Xcode-managed profile lists every certificate on the account, so regenerate it and"
      echo "  replace the secret (or check the certificate secret is the current one)."
      echo "  Steps: $runbook"
    fi
    if [ -n "$mismatch" ]; then
      echo
      echo "$mismatch" | sed 's/^/    /'
      echo
      echo "  The app asks for a different value than the profile grants (push is the usual one:"
      echo "  development in the app and production in the Store profile, set by package-ios.sh"
      echo "  from the export method), or a keychain group the profile does not cover. Compare"
      echo "  clients/web/src-tauri/Entitlements.ios.plist with the profile in that secret."
    fi
    if [ -n "$nodev" ]; then
      echo
      echo "$nodev" | sed 's/^/    /'
      echo
      echo "  The archive step needs the Xcode-managed development profile, \"iOS Team Provisioning"
      echo "  Profile: <bundle id>\", in IOS_DEVELOPMENT_PROFILE. A profile made by hand in the portal"
      echo "  is not found, and neither is a missing or empty secret. Steps: $runbook"
    fi
    if [ -n "$notfound" ]; then
      echo
      echo "$notfound" | sed 's/^/    /'
      echo
      echo "  Xcode looks the profile up by its own name (\"iOS Team Store Provisioning Profile: <bundle id>\""
      echo "  to export, \"iOS Team Provisioning Profile: <bundle id>\" to archive). A profile made by"
      echo "  hand in the portal, or one that is missing, is not found."
      echo "  On CI, the \"Install the signing credentials\" step prints the name of each profile it"
      echo "  installed: it should be those two names. If it is not, replace the secret."
      echo "  Steps: $runbook"
    fi
    if [ -n "$accounts" ]; then
      echo
      echo "  \"No Accounts\" on its own is not the cause. Xcode prints it when it cannot ask Apple for"
      echo "  a new profile, after it has already rejected the one it has. Read the other errors."
      if [ -n "$caps$cert$mismatch$nodev$notfound" ]; then
        echo "  The errors above are the cause."
      elif [ "${GITHUB_ACTIONS:-}" = "true" ]; then
        echo "  Nothing else is listed. The workflow gives Xcode no account, only the profiles that"
        echo "  \"Install the signing credentials\" installed: check the names that step printed are"
        echo "  \"iOS Team Store Provisioning Profile: <bundle id>\" and \"iOS Team Provisioning"
        echo "  Profile: <bundle id>\", and that both secrets are newer than the last capability change."
      else
        echo "  Nothing else is listed: then this Mac has no Apple ID in Xcode > Settings > Accounts"
        echo "  and no APPLE_API_KEY, APPLE_API_ISSUER and APPLE_API_KEY_PATH (an App Store Connect"
        echo "  API key) to fetch profiles with."
      fi
    fi
    echo
  } >&2
  if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    {
      echo "### iOS signing failed"
      [ -n "$caps" ] && printf 'The profile lacks a capability the app asks for:\n\n```\n%s\n```\n\nRegenerate both Xcode-managed profiles and replace `IOS_APPSTORE_PROFILE` and `IOS_DEVELOPMENT_PROFILE`. See %s.\n\n' "$caps" "$runbook"
      [ -n "$notfound" ] && printf 'Xcode did not find a profile by its managed name:\n\n```\n%s\n```\n\nA hand-made or missing profile is not found. See %s.\n\n' "$notfound" "$runbook"
      true
    } >>"$GITHUB_STEP_SUMMARY"
  fi
}

# --- dispatch ----------------------------------------------------------------

cmd=${1:-}
[ -n "$cmd" ] || die "usage: store-credentials.sh <check-secrets|xcode|keychain|profile|asc-key|cleanup|list-profiles|explain> ..."
shift
case $cmd in
  check-secrets) cmd_check_secrets "$@" ;;
  xcode) cmd_xcode "$@" ;;
  keychain) cmd_keychain "$@" ;;
  profile) cmd_profile "$@" ;;
  asc-key) cmd_asc_key ;;
  cleanup) cmd_cleanup ;;
  list-profiles) cmd_list_profiles "$@" ;;
  explain) cmd_explain "$@" ;;
  *) die "unknown command '$cmd'" ;;
esac

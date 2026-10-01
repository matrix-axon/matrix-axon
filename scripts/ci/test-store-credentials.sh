#!/usr/bin/env bash
# Tests for store-credentials.sh.
#
#   scripts/ci/test-store-credentials.sh
#   TEST_BASH=/bin/bash scripts/ci/test-store-credentials.sh   # macOS's bash 3.2
#
# macOS only: it needs `security`. Everything runs against a throwaway,
# self-signed identity and throwaway keychains that are created here and deleted
# at the end. The `keychain` subcommand puts its keychain first in the user's
# search list, which is its job on a runner and not something a test should leave
# behind, so the search list is saved first and restored by a trap.
# `A && B || fail` is meant: fail when the conjunction does not hold. SC2015 warns that
# `fail` could also run if B itself fails, which for these `contains` checks is the same event.
# shellcheck disable=SC2015
set -euo pipefail

if ! command -v security >/dev/null 2>&1; then
  echo "skipped: no \`security\` here (this is a macOS tool)"
  exit 0
fi

here=$(CDPATH="" cd -- "$(dirname "$0")" && pwd)
sc="$here/store-credentials.sh"
test_bash=${TEST_BASH:-bash}
work=$(mktemp -d)
# macOS's openssl, not Homebrew's: a .p12 made by OpenSSL 3 uses a MAC that
# `security import` rejects.
ossl=/usr/bin/openssl
failures=0

saved_list=()
while IFS= read -r l; do
  [ -n "$l" ] && saved_list+=("$l")
done <<EOF
$(security list-keychains -d user | sed -e 's/^ *"//' -e 's/"$//')
EOF
restore_list() {
  security list-keychains -d user -s "${saved_list[@]}" >/dev/null 2>&1 || true
}
cleanup() {
  restore_list
  for k in "$work"/*.keychain-db "$work"/*/*.keychain-db; do
    [ -f "$k" ] && security delete-keychain "$k" >/dev/null 2>&1 || true
  done
  rm -rf "$work"
}
trap cleanup EXIT

fail() {
  echo "FAIL: $1" >&2
  shift
  for line in "$@"; do echo "      $line" >&2; done
  failures=$((failures + 1))
}
contains() { case $1 in *"$2"*) return 0 ;; *) return 1 ;; esac; }

# run <env assignments...> -- <args...>: the script in a clean environment.
# Prints OUT<>, ERR<>, rc=.
run() {
  local envs=()
  while [ "$1" != "--" ]; do envs+=("$1"); shift; done
  shift
  local out rc=0
  out=$(env -i PATH="$PATH" HOME="${TEST_HOME:-$work/home}" ${envs[@]+"${envs[@]}"} "$test_bash" "$sc" "$@" 2>"$work/err") || rc=$?
  printf 'OUT<%s>\nERR<%s>\nrc=%s\n' "$out" "$(cat "$work/err")" "$rc"
}
mkdir -p "$work/home"
# The real HOME for anything that goes through `security`: it finds the user's
# keychain search list from HOME, and `security cms -D` needs it too. Only the
# profile folder and the App Store Connect key folder are redirected, and those
# by their own settings or by a HOME that `security` never sees.
run_real() { TEST_HOME="$HOME" run "$@"; }
profiles="$work/pd1:$work/pd2"

# --- the throwaway identity --------------------------------------------------

(cd "$work" && $ossl req -x509 -newkey rsa:2048 -nodes -keyout k.pem -out c.pem \
  -subj "/CN=Test Store Identity: Primary (TEAM1)" -days 2 -addext "extendedKeyUsage=codeSigning" 2>/dev/null \
  && $ossl pkcs12 -export -inkey k.pem -in c.pem -out t.p12 -passout pass:testpw)
p12_b64=$(base64 -i "$work/t.p12")
# The same identity plus a certificate whose private key is not in the file: what a
# .p12 made from certificates alone, or from the wrong keychain, looks like.
(cd "$work" && $ossl req -x509 -newkey rsa:2048 -nodes -keyout /dev/null -out orphan.pem \
  -subj "/CN=Test Orphan: Certificate (TEAM1)" -days 2 -addext "extendedKeyUsage=codeSigning" 2>/dev/null \
  && $ossl pkcs12 -export -inkey k.pem -in c.pem -certfile orphan.pem -out t-orphan.p12 -passout pass:testpw)
p12_orphan_b64=$(base64 -i "$work/t-orphan.p12")

# A provisioning profile is a CMS-signed plist. `security cms -S` refuses an
# untrusted identity, so these are signed with openssl, which `security cms -D`
# decodes just as well.
make_profile() { # name file-to-write [key=value ...]  (python literal overrides)
  local out=$1; shift
  python3 - "$work/p.plist" "$@" <<'PY'
import plistlib, sys, datetime
path, *over = sys.argv[1:]
d = {"UUID": "11111111-2222-3333-4444-555555555555", "Name": "Test Profile",
     "ExpirationDate": datetime.datetime(2099, 1, 1),
     "Entitlements": {"application-identifier": "TEAM123456.org.example.app", "get-task-allow": False}}
for o in over:
    k, v = o.split("=", 1)
    v = eval(v)
    if k.startswith("Entitlements."):
        d["Entitlements"][k.split(".", 1)[1]] = v
    elif v is None:
        d.pop(k, None)
    else:
        d[k] = v
if "com.apple.application-identifier" in d["Entitlements"]:
    d["Entitlements"].pop("application-identifier", None)
plistlib.dump(d, open(path, "wb"))
PY
  $ossl smime -sign -signer "$work/c.pem" -inkey "$work/k.pem" -in "$work/p.plist" \
    -out "$work/$out" -outform DER -nodetach -binary
  base64 -i "$work/$out"
}

# --- check-secrets -----------------------------------------------------------

all_ios="APPLE_STORE_CERTIFICATES=x APPLE_STORE_CERTIFICATES_PASSWORD=x IOS_APPSTORE_PROFILE=x APPLE_TEAM_ID=x IOS_DEVELOPMENT_PROFILE=x"
all_macos="APPLE_STORE_CERTIFICATES=x APPLE_STORE_CERTIFICATES_PASSWORD=x MAC_APPSTORE_PROFILE=x"
all_asc="ASC_KEY_ID=x ASC_ISSUER_ID=x ASC_PRIVATE_KEY=x"
# shellcheck disable=SC2086
out=$(run $all_ios -- check-secrets ios)
contains "$out" "rc=0" && contains "$out" "all required secrets for the ios build are set" || fail "ios: everything set passes" "$out"

for missing in APPLE_STORE_CERTIFICATES APPLE_STORE_CERTIFICATES_PASSWORD IOS_APPSTORE_PROFILE APPLE_TEAM_ID; do
  set_vars=$(printf '%s' "$all_ios" | tr ' ' '\n' | grep -v "^$missing=" | tr '\n' ' ')
  # shellcheck disable=SC2086
  out=$(run $set_vars -- check-secrets ios)
  contains "$out" "rc=1" && contains "$out" "$missing" || fail "ios: $missing missing is an error naming it" "$out"
  for other in APPLE_STORE_CERTIFICATES_PASSWORD IOS_APPSTORE_PROFILE APPLE_TEAM_ID; do
    [ "$other" = "$missing" ] && continue
    contains "$out" "secrets for the ios build: $other" && fail "ios: only the missing secret is named" "$out"
  done
done

# GitHub gives a secret that does not exist as an empty string, not as an unset variable.
# shellcheck disable=SC2086
out=$(run $all_ios IOS_APPSTORE_PROFILE= -- check-secrets ios)
contains "$out" "rc=1" && contains "$out" "IOS_APPSTORE_PROFILE" || fail "an empty secret counts as missing" "$out"

# shellcheck disable=SC2086
out=$(run APPLE_STORE_CERTIFICATES=x APPLE_STORE_CERTIFICATES_PASSWORD=x IOS_APPSTORE_PROFILE=x APPLE_TEAM_ID=x -- check-secrets ios)
contains "$out" "rc=0" && contains "$out" "optional secrets not set: IOS_DEVELOPMENT_PROFILE" || fail "ios: the development profile is optional and says so" "$out"

# shellcheck disable=SC2086
out=$(run $all_macos -- check-secrets macos)
contains "$out" "rc=0" || fail "macos: everything set passes" "$out"
out=$(run APPLE_STORE_CERTIFICATES=x APPLE_STORE_CERTIFICATES_PASSWORD=x -- check-secrets macos)
contains "$out" "rc=1" && contains "$out" "MAC_APPSTORE_PROFILE" || fail "macos: the profile is required" "$out"

# shellcheck disable=SC2086
out=$(run $all_ios -- check-secrets ios --asc)
contains "$out" "rc=1" && contains "$out" "ASC_KEY_ID" && contains "$out" "ASC_ISSUER_ID" && contains "$out" "ASC_PRIVATE_KEY" || fail "--asc requires the App Store Connect secrets" "$out"
# shellcheck disable=SC2086
out=$(run $all_ios $all_asc -- check-secrets ios --asc)
contains "$out" "rc=0" || fail "--asc passes when they are set" "$out"

out=$(run -- check-secrets android)
contains "$out" "rc=1" && contains "$out" "lane must be ios or macos" || fail "an unknown lane is refused" "$out"
out=$(run -- check-secrets)
contains "$out" "rc=1" || fail "a missing lane is refused" "$out"

# A secret's value must never reach the output, even on the path that is about secrets.
# shellcheck disable=SC2086
out=$(run APPLE_STORE_CERTIFICATES=SECRETVALUE111 APPLE_STORE_CERTIFICATES_PASSWORD=SECRETVALUE222 APPLE_TEAM_ID=SECRETVALUE333 -- check-secrets ios)
contains "$out" "SECRETVALUE" && fail "check-secrets does not print secret values" "$out"

# --- xcode -------------------------------------------------------------------

rm -f "$work/env"
out=$(run GITHUB_ENV="$work/env" -- xcode --min-sdk 1)
contains "$out" "rc=0" && contains "$out" "using " && contains "$out" "iOS SDK" || fail "xcode: a low minimum selects an installed Xcode" "$out"
grep -q '^DEVELOPER_DIR=/Applications/Xcode.*\.app/Contents/Developer$' "$work/env" 2>/dev/null || fail "xcode: DEVELOPER_DIR is exported for later steps" "$(cat "$work/env" 2>/dev/null)"
out=$(run -- xcode --min-sdk 99)
contains "$out" "rc=1" && contains "$out" "no installed Xcode has an iOS SDK of at least 99" || fail "xcode: too old an SDK is refused with the reason" "$out"

# --- keychain ----------------------------------------------------------------

mkdir -p "$work/kc1"
rm -f "$work/env"
out=$(run_real RUNNER_TEMP="$work/kc1" GITHUB_ENV="$work/env" SIGNING_KEYCHAIN_ACCEPT_UNTRUSTED=1 \
  APPLE_STORE_CERTIFICATES="$p12_b64" APPLE_STORE_CERTIFICATES_PASSWORD=testpw -- keychain --require "Test Store Identity")
kc="$work/kc1/axon-signing.keychain-db"
contains "$out" "rc=0" && contains "$out" "signing keychain ready" || fail "keychain: imports and reports ready" "$out"
[ -f "$kc" ] || fail "keychain: the keychain file exists"
grep -q '^AXON_SIGNING_KEYCHAIN=' "$work/env" && grep -q '^AXON_SIGNING_KEYCHAIN_PASSWORD=.\{20,\}' "$work/env" || fail "keychain: both variables are exported, with a real password" "$(cut -d= -f1 "$work/env" 2>/dev/null)"
contains "$out" "$(grep '^AXON_SIGNING_KEYCHAIN_PASSWORD=' "$work/env" | cut -d= -f2)" && fail "keychain: the generated password is never printed" "$out"
contains "$out" "testpw" && fail "keychain: the .p12 password is never printed" "$out"

# First in the search list, with every entry that was there kept, in order.
now=()
while IFS= read -r l; do [ -n "$l" ] && now+=("$l"); done <<EOF
$(security list-keychains -d user | sed -e 's/^ *"//' -e 's/"$//')
EOF
[ "$(cd "$(dirname "${now[0]}")" && pwd -P)/$(basename "${now[0]}")" = "$(cd "$work/kc1" && pwd -P)/axon-signing.keychain-db" ] || fail "keychain: it is first in the search list" "first: ${now[0]}"
for i in "${!saved_list[@]}"; do
  [ "${now[$((i + 1))]:-}" = "${saved_list[$i]}" ] || fail "keychain: existing search-list entry $i is kept, in order" "want: ${saved_list[$i]}" "got:  ${now[$((i + 1))]:-}"
done

# What the keychain holds is reported in a way that explains a missing identity: a
# self-signed test identity is not trusted, and the log says so, with the reason.
contains "$out" "identities in the signing keychain (certificate with its private key):" && contains "$out" '"Test Store Identity: Primary (TEAM1)"  NOT VALID (CSSMERR_TP_NOT_TRUSTED)' || fail "keychain: each identity is listed with whether it is valid, and why not" "$out"
contains "$out" "with no private key" && fail "keychain: no 'no private key' section when every certificate has its key" "$out"

# The trusted-application list of the imported key: every tool that signs.
trusted=$(security dump-keychain -a "$kc" 2>/dev/null | grep -o '/usr/bin/[a-z]*' | sort -u | tr '\n' ' ')
for tool in codesign productsign productbuild security; do
  contains "$trusted" "/usr/bin/$tool" || fail "keychain: the key trusts $tool" "trusted: $trusted"
done
restore_list

# A certificate that arrived without its private key is called out apart from the
# identities, because "present but cannot sign" has a different fix from "absent".
out=$(run_real RUNNER_TEMP="$work/kc1" SIGNING_KEYCHAIN_ACCEPT_UNTRUSTED=1 APPLE_STORE_CERTIFICATES="$p12_orphan_b64" APPLE_STORE_CERTIFICATES_PASSWORD=testpw -- keychain --require "Test Store Identity")
contains "$out" "certificates in the signing keychain with no private key (they cannot sign):" && contains "$out" '"Test Orphan: Certificate (TEAM1)"' || fail "keychain: a certificate with no private key is listed as one" "$out"
orphan_section=${out#*"with no private key"}
contains "${out%%"with no private key"*}" "Test Orphan" && fail "keychain: the certificate without a key is not listed among the identities" "$out"
contains "$orphan_section" "Test Store Identity" && fail "keychain: an identity that has its key is not listed as lacking one" "$out"
restore_list

# The listing is printed before the check that can fail, so a missing identity is
# explained in the same log.
out=$(run_real RUNNER_TEMP="$work/kc1" SIGNING_KEYCHAIN_ACCEPT_UNTRUSTED=1 APPLE_STORE_CERTIFICATES="$p12_orphan_b64" APPLE_STORE_CERTIFICATES_PASSWORD=testpw -- keychain --require "Test Orphan: Certificate")
contains "$out" "rc=1" && contains "$out" "OUT<identities in the signing keychain" && contains "$out" '"Test Orphan: Certificate (TEAM1)"' && contains "$out" "no valid identity named 'Test Orphan: Certificate" || fail "keychain: when a required identity is a certificate with no key, the same log shows that" "$out"
restore_list

out=$(run_real RUNNER_TEMP="$work/kc1" SIGNING_KEYCHAIN_ACCEPT_UNTRUSTED=1 APPLE_STORE_CERTIFICATES="$p12_b64" APPLE_STORE_CERTIFICATES_PASSWORD=testpw -- keychain --require "No Such Identity|Test Store Identity")
contains "$out" "rc=0" || fail "keychain: --require accepts the alternative that is present" "$out"
restore_list
out=$(run_real RUNNER_TEMP="$work/kc1" SIGNING_KEYCHAIN_ACCEPT_UNTRUSTED=1 APPLE_STORE_CERTIFICATES="$p12_b64" APPLE_STORE_CERTIFICATES_PASSWORD=testpw -- keychain --require "No Such Identity|Nor This One")
contains "$out" "rc=1" && contains "$out" "no valid identity named 'No Such Identity|Nor This One" || fail "keychain: --require with no alternative present is an error naming them all" "$out"
restore_list
out=$(run_real RUNNER_TEMP="$work/kc1" SIGNING_KEYCHAIN_ACCEPT_UNTRUSTED=1 APPLE_STORE_CERTIFICATES="$p12_b64" APPLE_STORE_CERTIFICATES_PASSWORD=testpw -- keychain --require "Test Store Identity" --require "Also Missing")
contains "$out" "rc=1" && contains "$out" "'Also Missing" || fail "keychain: every --require must be met, not just the first" "$out"
restore_list

out=$(run_real RUNNER_TEMP="$work/kc1" SIGNING_KEYCHAIN_ACCEPT_UNTRUSTED=1 APPLE_STORE_CERTIFICATES="$p12_b64" APPLE_STORE_CERTIFICATES_PASSWORD=testpw -- keychain --require "No Such Identity")
contains "$out" "rc=1" && contains "$out" "no valid identity named 'No Such Identity" || fail "keychain: a required identity that is absent is an error naming it" "$out"
restore_list

out=$(run_real RUNNER_TEMP="$work/kc1" APPLE_STORE_CERTIFICATES="$p12_b64" APPLE_STORE_CERTIFICATES_PASSWORD=wrong -- keychain)
contains "$out" "rc=1" && contains "$out" "APPLE_STORE_CERTIFICATES_PASSWORD" || fail "keychain: a wrong password names the secret to check" "$out"
contains "$out" "wrong" && ! contains "$out" "wrong APPLE_STORE" && fail "keychain: the wrong password is not echoed" "$out"
restore_list

garbage=$(printf 'this is not a p12' | base64)
out=$(run_real RUNNER_TEMP="$work/kc1" APPLE_STORE_CERTIFICATES="$garbage" APPLE_STORE_CERTIFICATES_PASSWORD=testpw -- keychain)
contains "$out" "rc=1" && contains "$out" "could not import" || fail "keychain: something that is not a .p12 is refused" "$out"
restore_list

out=$(run_real RUNNER_TEMP="$work/kc1" APPLE_STORE_CERTIFICATES="$(printf '\n')" APPLE_STORE_CERTIFICATES_PASSWORD=testpw -- keychain)
contains "$out" "rc=1" || fail "keychain: an empty secret is refused" "$out"
restore_list

# A secret pasted from a base64 that wraps lines must work the same.
wrapped=$(base64 -i "$work/t.p12" | fold -w 64)
rm -f "$work/env"
out=$(run_real RUNNER_TEMP="$work/kc1" GITHUB_ENV="$work/env" SIGNING_KEYCHAIN_ACCEPT_UNTRUSTED=1 APPLE_STORE_CERTIFICATES="$wrapped" APPLE_STORE_CERTIFICATES_PASSWORD=testpw -- keychain --require "Test Store Identity")
contains "$out" "rc=0" || fail "keychain: wrapped base64 works" "$out"
restore_list

# --- profile -----------------------------------------------------------------

mkdir -p "$work/pr"
appstore=$(make_profile appstore.mobileprovision)
out=$(run_real RUNNER_TEMP="$work/pr" GITHUB_ENV="$work/penv" IOS_APPSTORE_PROFILE="$appstore" PROVISIONING_PROFILES_DIRS="$profiles" -- \
  profile IOS_APPSTORE_PROFILE --ext mobileprovision --expect app-store --bundle-id org.example.app --install --export-path SOME_PATH)
contains "$out" "rc=0" && contains "$out" "(app-store)" && contains "$out" "installed as 11111111-2222-3333-4444-555555555555.mobileprovision" || fail "profile: a good App Store profile is accepted and installed" "$out"
for d in pd1 pd2; do
  [ -f "$work/$d/11111111-2222-3333-4444-555555555555.mobileprovision" ] || fail "profile: it is installed in every folder Xcode might read ($d), named by UUID"
done
grep -q '^SOME_PATH=.*IOS_APPSTORE_PROFILE\.mobileprovision$' "$work/penv" || fail "profile: the decoded path is exported when asked" "$(cat "$work/penv" 2>/dev/null)"

adhoc=$(make_profile adhoc.mobileprovision 'ProvisionedDevices=["A","B"]')
out=$(run_real RUNNER_TEMP="$work/pr" IOS_APPSTORE_PROFILE="$adhoc" -- profile IOS_APPSTORE_PROFILE --ext mobileprovision --expect app-store)
contains "$out" "rc=1" && contains "$out" "is a ad-hoc profile" && contains "$out" "not swapped" || fail "profile: an Ad Hoc profile in the App Store slot is refused as a likely swap" "$out"

dev=$(make_profile dev.mobileprovision 'Entitlements.get-task-allow=True' 'ProvisionedDevices=["A"]')
out=$(run_real RUNNER_TEMP="$work/pr" IOS_DEVELOPMENT_PROFILE="$dev" -- profile IOS_DEVELOPMENT_PROFILE --ext mobileprovision --expect development)
contains "$out" "rc=0" && contains "$out" "(development)" || fail "profile: a development profile is recognised" "$out"
out=$(run_real RUNNER_TEMP="$work/pr" IOS_APPSTORE_PROFILE="$dev" -- profile IOS_APPSTORE_PROFILE --ext mobileprovision --expect app-store)
contains "$out" "rc=1" && contains "$out" "is a development profile" || fail "profile: a development profile in the App Store slot is refused" "$out"

out=$(run_real RUNNER_TEMP="$work/pr" IOS_APPSTORE_PROFILE="$appstore" -- profile IOS_APPSTORE_PROFILE --ext mobileprovision --bundle-id org.other.app)
contains "$out" "rc=1" && contains "$out" "not for org.other.app" || fail "profile: a profile for another app is refused" "$out"

expired=$(make_profile expired.mobileprovision 'ExpirationDate=__import__("datetime").datetime(2020,1,1)')
out=$(run_real RUNNER_TEMP="$work/pr" IOS_APPSTORE_PROFILE="$expired" -- profile IOS_APPSTORE_PROFILE --ext mobileprovision)
contains "$out" "rc=1" && contains "$out" "expired on 2020-01-01" || fail "profile: an expired profile is refused" "$out"

out=$(run_real RUNNER_TEMP="$work/pr" IOS_APPSTORE_PROFILE="$garbage" -- profile IOS_APPSTORE_PROFILE --ext mobileprovision)
contains "$out" "rc=1" && contains "$out" "is not a provisioning profile" || fail "profile: something that is not a profile is refused" "$out"

mac=$(make_profile mac.provisionprofile 'Entitlements.com.apple.application-identifier="TEAM123456.org.example.app"' 'Platform=["OSX"]')
out=$(run_real RUNNER_TEMP="$work/pr" MAC_APPSTORE_PROFILE="$mac" -- profile MAC_APPSTORE_PROFILE --ext provisionprofile --expect app-store --bundle-id org.example.app)
contains "$out" "rc=0" && contains "$out" "(app-store)" || fail "profile: a macOS profile (com.apple.application-identifier) is read too" "$out"

out=$(run_real RUNNER_TEMP="$work/pr" -- profile NOT_SET_AT_ALL --ext mobileprovision)
contains "$out" "rc=1" && contains "$out" "NOT_SET_AT_ALL is not set" || fail "profile: an unset variable is refused by name" "$out"
out=$(run_real RUNNER_TEMP="$work/pr" IOS_APPSTORE_PROFILE="$appstore" -- profile IOS_APPSTORE_PROFILE --ext zip)
contains "$out" "rc=1" && contains "$out" "--ext must be" || fail "profile: an unknown extension is refused" "$out"

# --- list-profiles -----------------------------------------------------------

mkdir -p "$work/lp1" "$work/lp2" "$work/lp-empty" "$work/extra"
printf '%s' "$appstore" | $ossl base64 -d -A >"$work/lp1/store.mobileprovision"
printf '%s' "$dev" | $ossl base64 -d -A >"$work/lp1/dev.mobileprovision"
printf '%s' "$mac" | $ossl base64 -d -A >"$work/lp2/mac.provisionprofile"
other=$(make_profile other.mobileprovision 'Entitlements.application-identifier="TEAM123456.org.other.app"')
printf '%s' "$other" | $ossl base64 -d -A >"$work/lp2/other.mobileprovision"
printf '%s' "$adhoc" | $ossl base64 -d -A >"$work/extra/adhoc.mobileprovision"

out=$(run_real PROVISIONING_PROFILES_DIRS="$work/lp1:$work/lp2" -- list-profiles --bundle-id org.example.app)
contains "$out" "rc=0" && contains "$out" "$work/lp1/store.mobileprovision" && contains "$out" "$work/lp2/mac.provisionprofile" || fail "list-profiles: finds profiles in every folder" "$out"
contains "$out" "| app-store | iOS |" && contains "$out" "| development | iOS |" && contains "$out" "| app-store | OSX |" || fail "list-profiles: says the kind and the platform of each" "$out"
contains "$out" "other.mobileprovision" && fail "list-profiles: --bundle-id leaves out a profile for another app" "$out"
contains "$out" "no macOS profile" && fail "list-profiles: no hint about a missing macOS profile when one was listed" "$out"

# A Mac App Store profile is a downloaded file, not one Xcode installs, so when the
# folders searched hold only iOS profiles the output says where to look.
out=$(run_real PROVISIONING_PROFILES_DIRS="$work/lp1" -- list-profiles --bundle-id org.example.app)
contains "$out" "rc=0" && contains "$out" "note: no macOS profile (.provisionprofile) found" && contains "$out" "name the folder or file you saved it in" || fail "list-profiles: with only iOS profiles it says no macOS profile was found, and where to look" "$out"
contains "$out" "$work/lp1/store.mobileprovision" || fail "list-profiles: ...and still lists the iOS ones" "$out"

out=$(run_real PROVISIONING_PROFILES_DIRS="$work/lp1:$work/lp2" -- list-profiles)
contains "$out" "other.mobileprovision" || fail "list-profiles: without --bundle-id, every profile is listed" "$out"

out=$(run_real PROVISIONING_PROFILES_DIRS="$work/lp-empty" -- list-profiles "$work/extra" --bundle-id org.example.app)
contains "$out" "rc=0" && contains "$out" "$work/extra/adhoc.mobileprovision" && contains "$out" "| ad-hoc | iOS |" || fail "list-profiles: a folder named on the command line is searched too, and an Ad Hoc profile is called one" "$out"
out=$(run_real PROVISIONING_PROFILES_DIRS="$work/lp-empty" -- list-profiles "$work/lp1/store.mobileprovision")
contains "$out" "rc=0" && contains "$out" "$work/lp1/store.mobileprovision" || fail "list-profiles: a single file named on the command line is described" "$out"

out=$(run_real PROVISIONING_PROFILES_DIRS="$work/lp-empty" -- list-profiles --bundle-id org.example.app)
contains "$out" "rc=1" && contains "$out" "no provisioning profiles found for org.example.app" || fail "list-profiles: nothing found is an error that says so" "$out"
contains "$out" "note: no macOS profile" || fail "list-profiles: nothing found also carries the hint about a downloaded Mac profile" "$out"

# --- asc-key -----------------------------------------------------------------

$ossl ecparam -name prime256v1 -genkey -noout -out "$work/ec.pem" 2>/dev/null
$ossl pkcs8 -topk8 -nocrypt -in "$work/ec.pem" -out "$work/ec.p8" 2>/dev/null
key_text=$(cat "$work/ec.p8")
out=$(run RUNNER_TEMP="$work/pr" ASC_KEY_ID=ABCDE12345 ASC_PRIVATE_KEY="$key_text" -- asc-key)
kf="$work/home/.appstoreconnect/private_keys/AuthKey_ABCDE12345.p8"
contains "$out" "rc=0" && [ -f "$kf" ] || fail "asc-key: the key is written where altool looks" "$out"
[ "$(stat -f %Lp "$kf")" = "600" ] || fail "asc-key: the key file is readable by its owner only" "mode: $(stat -f %Lp "$kf")"
[ "$(cat "$kf")" = "$key_text" ] || fail "asc-key: the key is written unchanged"
contains "$out" "BEGIN" && fail "asc-key: the key is never printed" "$out"

out=$(run RUNNER_TEMP="$work/pr" ASC_KEY_ID=BADKEY0000 ASC_PRIVATE_KEY="not a key" -- asc-key)
contains "$out" "rc=1" && contains "$out" "is not a private key" && [ ! -f "$work/home/.appstoreconnect/private_keys/AuthKey_BADKEY0000.p8" ] || fail "asc-key: something that is not a key is refused and not left behind" "$out"
for badid in "abcde12345" "../../x" "AB CD" ""; do
  out=$(run RUNNER_TEMP="$work/pr" ASC_KEY_ID="$badid" ASC_PRIVATE_KEY="$key_text" -- asc-key)
  contains "$out" "rc=1" || fail "asc-key: key id '$badid' is refused (it becomes part of a path)" "$out"
done

# --- cleanup -----------------------------------------------------------------

rm -rf "$work/kc2"; mkdir -p "$work/kc2"
run_real RUNNER_TEMP="$work/kc2" SIGNING_KEYCHAIN_ACCEPT_UNTRUSTED=1 GITHUB_ENV="$work/env2" APPLE_STORE_CERTIFICATES="$p12_b64" APPLE_STORE_CERTIFICATES_PASSWORD=testpw -- keychain >/dev/null
restore_list
run_real RUNNER_TEMP="$work/kc2" IOS_APPSTORE_PROFILE="$appstore" PROVISIONING_PROFILES_DIRS="$profiles" -- profile IOS_APPSTORE_PROFILE --ext mobileprovision --install >/dev/null
run RUNNER_TEMP="$work/kc2" ASC_KEY_ID=CLEAN12345 ASC_PRIVATE_KEY="$key_text" -- asc-key >/dev/null
[ -f "$work/kc2/axon-signing.keychain-db" ] || fail "cleanup: precondition, the keychain exists"
out=$(run_real RUNNER_TEMP="$work/kc2" AXON_SIGNING_KEYCHAIN="$work/kc2/axon-signing.keychain-db" -- cleanup)
contains "$out" "rc=0" || fail "cleanup: succeeds" "$out"
[ ! -f "$work/kc2/axon-signing.keychain-db" ] || fail "cleanup: deletes the keychain"
for d in pd1 pd2; do
  [ ! -f "$work/$d/11111111-2222-3333-4444-555555555555.mobileprovision" ] || fail "cleanup: removes the profile it installed in $d"
done
[ ! -f "$work/home/.appstoreconnect/private_keys/AuthKey_CLEAN12345.p8" ] || fail "cleanup: removes the key it wrote"
out=$(run_real RUNNER_TEMP="$work/kc2" -- cleanup)
contains "$out" "rc=0" || fail "cleanup: running it again, with nothing to clean, still succeeds" "$out"

if [ "$failures" -ne 0 ]; then
  echo "$failures failed" >&2
  exit 1
fi
echo "all passed"

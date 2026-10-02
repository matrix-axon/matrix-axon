#!/usr/bin/env bash
# Tests for profile-certs.sh.
#
#   scripts/lib/test-profile-certs.sh
#   TEST_BASH=/bin/bash scripts/lib/test-profile-certs.sh   # macOS's bash 3.2
#
# macOS only: it needs `security`. Everything runs against throwaway self-signed
# certificates in a throwaway keychain that is created here, named explicitly to
# the function and deleted at the end, so the user's keychains and search list are
# never touched.
# `A && B || fail` is meant: fail when the conjunction does not hold. SC2015 warns that
# `fail` could also run if B itself fails, which for these `contains` checks is the same event.
# shellcheck disable=SC2015
set -euo pipefail

if ! command -v security >/dev/null 2>&1; then
  echo "skipped: no \`security\` here (this is a macOS tool)"
  exit 0
fi

here=$(CDPATH="" cd -- "$(dirname "$0")" && pwd)
test_bash=${TEST_BASH:-bash}
work=$(mktemp -d)
kc="$work/pc.keychain-db"
# macOS's openssl: a .p12 made by OpenSSL 3 uses a MAC that `security import` rejects.
ossl=/usr/bin/openssl
failures=0
# Made up when the test runs, not written into the source; see test-store-credentials.sh.
pw=$(openssl rand -hex 12)
cleanup() {
  security delete-keychain "$kc" >/dev/null 2>&1 || true
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

# --- certificates ------------------------------------------------------------
# A and B are identities (certificate with its private key). O is a certificate
# the keychain holds without its key. X is never put in the keychain at all.

cert() { # name cn  -> name.pem, name.key
  $ossl req -x509 -newkey rsa:2048 -nodes -keyout "$work/$1.key" -out "$work/$1.pem" \
    -subj "/CN=$2" -days 2 -addext "extendedKeyUsage=codeSigning" 2>/dev/null
}
cert A "Test Application: Alpha (TEAM1)"
cert B "Test Distribution: Beta (TEAM1)"
cert O "Test Application: Orphan (TEAM1)"
cert X "Test Application: Absent (TEAM1)"

$ossl pkcs12 -export -inkey "$work/A.key" -in "$work/A.pem" -certfile "$work/O.pem" -out "$work/a.p12" -passout pass:$pw
$ossl pkcs12 -export -inkey "$work/B.key" -in "$work/B.pem" -out "$work/b.p12" -passout pass:$pw
security create-keychain -p "" "$kc"
security unlock-keychain -p "" "$kc"
for f in a b; do
  security import "$work/$f.p12" -f pkcs12 -k "$kc" -P "$pw" -T /usr/bin/security >/dev/null
done

# A profile is a plist whose DeveloperCertificates are DER certificates.
profile() { # file cert.pem...
  local out=$1
  shift
  python3 - "$out" "$@" <<'PY'
import plistlib, ssl, sys
out, *pems = sys.argv[1:]
certs = [ssl.PEM_cert_to_DER_cert(open(p).read()) for p in pems]
plistlib.dump({"Name": "Test", "DeveloperCertificates": certs}, open(out, "wb"))
PY
}
profile "$work/lists-A.plist" "$work/A.pem"
profile "$work/lists-O.plist" "$work/O.pem"
profile "$work/lists-X.plist" "$work/X.pem"
profile "$work/lists-A-and-O.plist" "$work/A.pem" "$work/O.pem"
profile "$work/lists-O-and-X.plist" "$work/O.pem" "$work/X.pem"
profile "$work/lists-nothing.plist"

# check <env assignments...> -- <plist> <identity> -> "ERR<...> rc=N"
check() {
  local envs=()
  while [ "$1" != "--" ]; do envs+=("$1"); shift; done
  shift
  local rc=0
  env -i PATH="$PATH" HOME="$HOME" ${envs[@]+"${envs[@]}"} "$test_bash" --noprofile --norc -c '. "$1"; check_profile_certificates "$2" "$3" "$4"' _ \
    "$here/profile-certs.sh" "$1" "$2" "$kc" 2>"$work/err" || rc=$?
  printf 'ERR<%s>\nrc=%s\n' "$(cat "$work/err")" "$rc"
}
trust="SIGNING_KEYCHAIN_ACCEPT_UNTRUSTED=1"

# --- the profile lists the certificate that signs ----------------------------

out=$(check $trust -- "$work/lists-A.plist" "Test Application: Alpha")
contains "$out" "rc=0" && contains "$out" "ERR<>" || fail "a profile listing the signing certificate passes, silently" "$out"

out=$(check $trust -- "$work/lists-A-and-O.plist" "Test Application: Alpha")
contains "$out" "rc=0" || fail "it is enough that one of the listed certificates is the signer" "$out"

# --- it does not, and the report says which of the three situations it is ----

out=$(check $trust -- "$work/lists-A.plist" "Test Distribution: Beta")
contains "$out" "rc=1" && contains "$out" "does not include any 'Test Distribution: Beta' certificate" || fail "the wrong identity is an error naming it" "$out"
contains "$out" "Test Application: Alpha (TEAM1)  -  in the keychain with its private key" || fail "a listed certificate that is in the keychain with its key is reported as usable" "$out"
contains "$out" "--app-identity 'Test Application: Alpha (TEAM1)'" || fail "...and the report names the identity to sign with instead" "$out"
contains "$out" "the identity chosen to sign with was: Test Distribution: Beta" || fail "the report says which identity was chosen" "$out"
contains "$out" "regenerate the profile" || fail "regenerating the profile is offered as the other way out" "$out"

out=$(check $trust -- "$work/lists-O.plist" "Test Distribution: Beta")
contains "$out" "rc=1" && contains "$out" "Test Application: Orphan (TEAM1)  -  in the keychain but with NO private key, so it cannot sign" || fail "a listed certificate present without its private key is reported as keyless" "$out"
contains "$out" "--app-identity" && fail "no --app-identity suggestion when the listed certificate cannot sign" "$out"
contains "$out" "together with its private key" || fail "...and the fix is to bring the key" "$out"

out=$(check $trust -- "$work/lists-X.plist" "Test Distribution: Beta")
contains "$out" "rc=1" && contains "$out" "Test Application: Absent (TEAM1)  -  not in the keychain" || fail "a listed certificate that is nowhere in the keychain is reported as absent" "$out"
contains "$out" "--app-identity" && fail "no --app-identity suggestion when the listed certificate is absent" "$out"

out=$(check $trust -- "$work/lists-O-and-X.plist" "Test Distribution: Beta")
contains "$out" "in the keychain but with NO private key" && contains "$out" "not in the keychain" || fail "every listed certificate gets its own line" "$out"

out=$(check $trust -- "$work/lists-A-and-O.plist" "Test Distribution: Beta")
contains "$out" "Alpha (TEAM1)  -  in the keychain with its private key" && contains "$out" "Orphan (TEAM1)  -  in the keychain but with NO private key" && contains "$out" "--app-identity 'Test Application: Alpha (TEAM1)'" || fail "a mix is reported line by line and only the usable one is suggested" "$out"
contains "$out" "--app-identity 'Test Application: Orphan" && fail "the keyless certificate is not suggested as a signer" "$out"

out=$(check $trust -- "$work/lists-nothing.plist" "Test Distribution: Beta")
contains "$out" "rc=1" && contains "$out" "lists no certificates at all" || fail "a profile with no certificates is called that" "$out"

# --- trust -------------------------------------------------------------------

# Without the escape hatch these identities are self-signed, so not valid, and the
# report must not recommend signing with one that would fail.
out=$(check -- "$work/lists-A.plist" "Test Distribution: Beta")
contains "$out" "rc=1" && contains "$out" "in the keychain with its private key, but NOT VALID" || fail "a listed certificate whose identity is not valid is reported as such" "$out"
contains "$out" "--app-identity" && fail "an identity that is not valid is not recommended" "$out"

if [ "$failures" -ne 0 ]; then
  echo "$failures failed" >&2
  exit 1
fi
echo "all passed"

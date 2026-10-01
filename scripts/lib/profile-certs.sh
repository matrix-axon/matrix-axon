# shellcheck shell=bash
# Source this; it defines check_profile_certificates.
#
#   check_profile_certificates PROFILE_PLIST APP_IDENTITY [KEYCHAIN]
#
# Returns 0 when the provisioning profile lists a certificate with APP_IDENTITY's
# name that is in the keychain. Otherwise prints what is wrong to stderr and
# returns 1.
#
# A profile lists the certificates it may be signed with, and a Mac App Store
# upload is rejected, after the build and the signing, when the app was signed with
# one it does not list. The portal shows several certificates with identical names
# and near-identical expiry dates, so picking the wrong one is easy.
#
# The check used to end at "the profile does not include any '<identity>'
# certificate in this keychain", which cannot say which of three different
# situations it is:
#
#   * a certificate the profile lists is in the keychain with its private key, and
#     the identity that was chosen is simply a different one: name that one;
#   * it is in the keychain but with no private key, so it cannot sign: the
#     certificate was imported without its key;
#   * it is not in the keychain at all.
#
# Those are different fixes, so the report says which each listed certificate is.
# Names and hashes only; nothing secret.
#
# KEYCHAIN restricts the lookups to one keychain; without it the whole search list
# is used, as the packaging script does. SIGNING_KEYCHAIN_ACCEPT_UNTRUSTED=1 treats
# an identity that is not trusted as valid, which a self-signed test identity never
# is.
#
# Bash 3.2 compatible.

# The SHA-1 of each identity in the keychain (a certificate with its private key),
# one per line: those that are valid, or all of them with `all`.
_pc_identity_hashes() {
  local which=$1 flags="-v"
  shift
  if [ "$which" = all ] || [ "${SIGNING_KEYCHAIN_ACCEPT_UNTRUSTED:-}" = "1" ]; then flags=""; fi
  # shellcheck disable=SC2086
  security find-identity $flags "$@" 2>/dev/null | awk '/^ +[0-9]+\) [0-9A-F]+ "/ { print $2 }' | sort -u
}

check_profile_certificates() {
  local plist=$1 app_identity=$2 kc=${3:-}
  local kcargs=()
  if [ -n "$kc" ]; then kcargs=("$kc"); fi

  local named_hashes
  named_hashes=$(security find-certificate -a -c "$app_identity" -Z ${kcargs[@]+"${kcargs[@]}"} 2>/dev/null | awk '/^SHA-1 hash:/ { print $3 }')

  # Every profile certificate, as "HASH<TAB>NAME".
  local listed="" i=0 cert_b64 der hash name
  while cert_b64=$(plutil -extract "DeveloperCertificates.$i" raw -o - "$plist" 2>/dev/null); do
    der=$(mktemp)
    printf '%s' "$cert_b64" | openssl base64 -d -A >"$der"
    hash=$(openssl x509 -inform der -in "$der" -noout -fingerprint -sha1 | sed 's/.*=//; s/://g')
    name=$(openssl x509 -inform der -in "$der" -noout -subject | sed -E 's/.*CN ?= ?([^,/]*).*/\1/')
    rm -f "$der"
    if printf '%s\n' "$named_hashes" | grep -qix "$hash"; then
      return 0
    fi
    listed="$listed$hash	$name
"
    i=$((i + 1))
  done

  local valid_ids all_ids all_certs
  valid_ids=$(_pc_identity_hashes valid ${kcargs[@]+"${kcargs[@]}"})
  all_ids=$(_pc_identity_hashes all ${kcargs[@]+"${kcargs[@]}"})
  all_certs=$(security find-certificate -a -Z ${kcargs[@]+"${kcargs[@]}"} 2>/dev/null | awk '/^SHA-1 hash:/ { print $3 }')

  echo "error: the profile does not include any '$app_identity' certificate in this keychain" >&2
  if [ -z "$listed" ]; then
    echo "       the profile lists no certificates at all, so it cannot be used for signing; regenerate it" >&2
    return 1
  fi
  echo "       the profile lists:" >&2
  local usable="" status
  while IFS='	' read -r hash name; do
    [ -n "$hash" ] || continue
    if printf '%s\n' "$valid_ids" | grep -qx "$hash"; then
      status="in the keychain with its private key"
      usable="$usable$name
"
    elif printf '%s\n' "$all_ids" | grep -qx "$hash"; then
      status="in the keychain with its private key, but NOT VALID (expired, or its chain is not trusted)"
    elif printf '%s\n' "$all_certs" | grep -qx "$hash"; then
      status="in the keychain but with NO private key, so it cannot sign"
    else
      status="not in the keychain"
    fi
    echo "         $name  -  $status" >&2
  done <<EOF
$listed
EOF
  echo "       the identity chosen to sign with was: $app_identity" >&2
  echo "       to fix it, one of:" >&2
  if [ -n "$usable" ]; then
    while IFS= read -r name; do
      [ -n "$name" ] || continue
      echo "         - sign with the certificate the profile lists:  --app-identity '$name'" >&2
    done <<EOF
$usable
EOF
  else
    echo "         - put a certificate the profile lists into the keychain together with its private key" >&2
    echo "           (in CI, into the .p12 secret), then use that identity" >&2
  fi
  echo "         - or regenerate the profile in the developer portal, selecting the '$app_identity' certificate" >&2
  return 1
}

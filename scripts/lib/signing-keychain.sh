# shellcheck shell=bash
# Source this; it defines unlock_signing_keychain and the helpers under it.
#
#   unlock_signing_keychain || exit 1
#
# Shared by every packaging script that signs: package-ios.sh and
# package-macos-mas.sh. Signing runs inside xcodebuild, codesign, productbuild or
# productsign, which read the login keychain unless told otherwise, and that one
# is locked in an SSH session and after a reboot. The failures are
# `errSecInternalComponent` from codesign, `errKCInteractionNotAllowed` from
# productsign, or a password dialog nobody is there to answer — none of which
# says "locked keychain".
#
# Set AXON_SIGNING_KEYCHAIN to a keychain that holds only the signing identities
# and has an empty password (a bare name is looked up under ~/Library/Keychains)
# and this unlocks it first. Off unless set: it is one developer's setup, not a
# requirement. The README's "Signing without a password prompt" says how to
# build one.
#
# AXON_IOS_KEYCHAIN and AXON_IOS_KEYCHAIN_PASSWORD are the names this had while
# it only served the iOS script. They still work, with a note, so a profile that
# exports them keeps working; the new names win when both are set.
#
# The keychain must also be the only place the identities live. An identity
# that exists in both this keychain and a locked login keychain is resolved to
# the locked copy even when this one is listed first — the same certificate
# signed cleanly with the login copy removed and failed with it present.
#
# `-p` puts the password on the command line, so it is visible to `ps` for the
# duration of the call. That is acceptable only because the intended password is
# empty; the *_PASSWORD variable exists for a keychain that has one.
#
# Bash 3.2 compatible (macOS's /bin/bash).

# The absolute path of FILE with symlinks in its directory resolved. `pwd -P`
# resolves the directory, which is where a symlink in these paths lives (`/var`
# for `/private/var`, a symlinked $HOME); a keychain file is not itself one.
# Prints nothing and fails when the directory does not exist.
canon_path() {
  (cd "$(dirname "$1")" 2>/dev/null && printf '%s/%s\n' "$(pwd -P)" "$(basename "$1")")
}

# The user's keychain search list, one unquoted path per line.
# `security list-keychains` prints them absolute and quoted, as stored.
signing_keychain_search_list() {
  security list-keychains -d user | sed -e 's/^ *"//' -e 's/"$//'
}

# keychain_in_search_list KEYCHAIN — 0 if listed, compared as resolved paths and
# not as text: a relative path (`./build.keychain-db`) or one through a symlink
# is listed and would never match a substring test.
keychain_in_search_list() {
  local wanted listed
  wanted=$(canon_path "$1") || return 1
  [ -n "$wanted" ] || return 1
  while IFS= read -r listed; do
    if [ -n "$listed" ] && [ "$(canon_path "$listed")" = "$wanted" ]; then
      return 0
    fi
  done <<EOF
$(signing_keychain_search_list)
EOF
  return 1
}

# The command that adds KEYCHAIN to the search list *keeping* what is there.
# `list-keychains -s` replaces the whole list, so naming only this keychain and
# login would silently drop every other one the developer has.
signing_keychain_fix_command() {
  local fix listed
  fix="security list-keychains -d user -s \"$1\""
  while IFS= read -r listed; do
    [ -n "$listed" ] && fix="$fix \"$listed\""
  done <<EOF
$(signing_keychain_search_list)
EOF
  printf '%s\n' "$fix"
}

# Returns 0 when there is nothing to do or it unlocked; 1 with the reason on
# stderr otherwise.
unlock_signing_keychain() {
  local requested="" varname=AXON_SIGNING_KEYCHAIN passvar=AXON_SIGNING_KEYCHAIN_PASSWORD
  local keychain password

  if [ -n "${AXON_SIGNING_KEYCHAIN:-}" ]; then
    requested=$AXON_SIGNING_KEYCHAIN
  elif [ -n "${AXON_IOS_KEYCHAIN:-}" ]; then
    requested=$AXON_IOS_KEYCHAIN
    varname=AXON_IOS_KEYCHAIN
    passvar=AXON_IOS_KEYCHAIN_PASSWORD
    echo "note: AXON_IOS_KEYCHAIN is the old name; use AXON_SIGNING_KEYCHAIN (and AXON_SIGNING_KEYCHAIN_PASSWORD)." >&2
  fi
  [ -n "$requested" ] || return 0

  case "$requested" in
    */*) keychain=$requested ;;
    *)   keychain="$HOME/Library/Keychains/${requested%.keychain-db}.keychain-db" ;;
  esac
  if [ ! -f "$keychain" ]; then
    echo "error: $varname is set but $keychain does not exist." >&2
    return 1
  fi

  # A password set to the empty string is a real answer, not an absence: the new
  # variable is consulted by whether it is *set*, so an old empty one cannot
  # override it.
  if [ "$varname" = AXON_SIGNING_KEYCHAIN ] && [ -z "${AXON_SIGNING_KEYCHAIN_PASSWORD+x}" ]; then
    password=${AXON_IOS_KEYCHAIN_PASSWORD:-}
  else
    password=${!passvar:-}
  fi
  if ! security unlock-keychain -p "$password" "$keychain"; then
    echo "error: could not unlock $keychain." >&2
    echo "       An empty password is expected; set $passvar if it has one." >&2
    return 1
  fi

  # Unlocking a keychain that is not in the search list does nothing useful:
  # the signing tools never look there.
  keychain=$(canon_path "$keychain")
  if ! keychain_in_search_list "$keychain"; then
    echo "error: $keychain is unlocked but not in the keychain search list." >&2
    echo "       $(signing_keychain_fix_command "$keychain")" >&2
    return 1
  fi
  echo "==> signing keychain unlocked: $keychain"
  return 0
}

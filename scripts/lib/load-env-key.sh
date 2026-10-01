# Source this; it defines load_env_key.
#
#   load_env_key NAME FILE
#
# Exports NAME from FILE, a dotenv-style file, unless NAME is already set and
# non-empty in the environment. Returns 0 either way; returns 10 when it took the
# value from the file, so a caller can say so, and nothing else uses that code.
# Under `set -e` that 10 ends the caller, so call it as
# `rc=0; load_env_key NAME FILE || rc=$?`.
#
# This is deliberately not `source FILE`, and deliberately takes one name at a
# time:
#
#   * A repository's `.env` is the server's configuration — database URL,
#     secrets, the lot. Sourcing it would export all of that into xcodebuild,
#     pnpm and tauri, and Xcode substitutes variables it finds in the
#     environment into build-phase commands (a stray FORCE_COLOR=1 once turned
#     into an architecture that way). A build script should take the keys it
#     needs and nothing else.
#
#   * dotenv is not shell. `source` executes the file, so a value with `$(...)`
#     in it runs, and one with a space, an unbalanced quote or a `$` is either
#     mangled or an error. This parses lines as data and never evaluates them.
#
# Matches dotenv where it matters here: the last assignment wins; blank lines and
# `#` comments are skipped; an optional leading `export`; one pair of matching
# surrounding quotes is removed; on an unquoted value, a trailing ` # comment` is
# dropped; CRLF line endings are tolerated. It does not expand `$VAR` or `~` —
# a value is what is written.
#
# Bash 3.2 compatible: macOS's /bin/bash is what `#!/usr/bin/env bash` finds
# when Homebrew's is not installed.

load_env_key() {
  local name=$1 file=$2 line value="" found=0

  # The environment wins, so a one-off `ASC_KEY_ID=… scripts/package-ios.sh`
  # overrides the file instead of being overridden by it.
  if [ -n "${!name:-}" ]; then
    return 0
  fi
  [ -f "$file" ] || return 0

  while IFS= read -r line || [ -n "$line" ]; do
    line=${line%$'\r'}
    line=${line#"${line%%[![:space:]]*}"}
    case $line in
      "export "*)
        line=${line#export }
        line=${line#"${line%%[![:space:]]*}"}
        ;;
    esac
    case $line in
      "$name="*)
        value=${line#*=}
        found=1
        ;;
    esac
  done <"$file"

  [ "$found" -eq 1 ] || return 0

  value=${value#"${value%%[![:space:]]*}"}
  value=${value%"${value##*[![:space:]]}"}
  case $value in
    \"*\" | \'*\')
      # Only a pair that matches: "x' is not quoted, it is odd, and falls through.
      value=${value:1:${#value}-2}
      ;;
    *)
      value=${value%% \#*}
      value=${value%"${value##*[![:space:]]}"}
      ;;
  esac

  [ -n "$value" ] || return 0
  export "$name=$value"
  return 10
}

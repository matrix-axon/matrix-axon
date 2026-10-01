# Source this; it defines asc_require_credentials and asc_next_build_number.
#
# The App Store Connect half of the packaging scripts, shared by package-ios.sh
# and package-macos-mas.sh so the two cannot drift apart: where the credentials
# come from, and what `--build-number auto` means.
#
# Bash 3.2 compatible (macOS's /bin/bash).

_asc_lib_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=load-env-key.sh
. "$_asc_lib_dir/load-env-key.sh"

# asc_require_credentials REPO_ROOT
#
# Makes ASC_KEY_ID and ASC_ISSUER_ID available, from the environment or else from
# REPO_ROOT/.env, and stops the script with a message naming both places when
# either is missing. Meant to run before the build: a forgotten variable should
# not cost a full build before it is mentioned.
#
# Those two names only: `.env` is the server's configuration and nothing else in
# it belongs in a build. The environment still wins, and the values are never
# printed, only which names came from the file. See lib/load-env-key.sh for why
# this is not `source .env`.
asc_require_credentials() {
  local repo_root=$1 key rc
  for key in ASC_KEY_ID ASC_ISSUER_ID; do
    rc=0
    load_env_key "$key" "$repo_root/.env" || rc=$?
    if [ "$rc" -eq 10 ]; then
      echo "==> $key taken from $repo_root/.env"
    fi
  done
  : "${ASC_KEY_ID:?set ASC_KEY_ID (the A1B2C3D4E5 in ~/.appstoreconnect/private_keys/AuthKey_*.p8), in the environment or in .env at the repository root}"
  : "${ASC_ISSUER_ID:?set ASC_ISSUER_ID (App Store Connect > Users and Access > Integrations), in the environment or in .env at the repository root}"
}

# asc_next_build_number REPO_ROOT TAURI_CONF
#
# Prints the next CFBundleVersion for the app whose bundle ID is TAURI_CONF's
# `identifier`: the highest App Store Connect lists plus one. Progress goes to
# stderr so the caller can capture the number alone:
#
#   build_number=$(asc_next_build_number "$repo_root" "$tauri_dir/tauri.conf.json") || exit 1
#
# Call it before the build, so a bad credential or an unreachable App Store
# Connect costs seconds, and after asc_require_credentials.
#
# The bundle ID is the app, not the platform, so the same call serves iOS and the
# Mac App Store: it takes the highest number across every build of the app, and
# the next one is above all of them. Numbers can skip, since an iOS build and a
# Mac build share the sequence; App Store Connect only requires that they rise.
#
# A build uploaded minutes ago and still processing may not be listed yet, so two
# uploads close together can be handed the same number — and the second is then
# rejected, which is loud, not silent.
asc_next_build_number() {
  local repo_root=$1 tauri_conf=$2 bundle_id number
  bundle_id=$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["identifier"])' "$tauri_conf") || {
    echo "error: could not read the bundle identifier from $tauri_conf." >&2
    return 1
  }
  echo "==> asking App Store Connect for the next build number ($bundle_id)" >&2
  number=$(python3 "$repo_root/scripts/lib/asc-next-build-number.py" "$bundle_id") || {
    echo "error: could not work out the next build number; pass --build-number <n> instead." >&2
    return 1
  }
  if ! [[ $number =~ ^[0-9]+(\.[0-9]+){0,2}$ ]]; then
    echo "error: App Store Connect lookup returned '$number', which is not a build number." >&2
    return 1
  fi
  echo "    build number: $number" >&2
  printf '%s\n' "$number"
}

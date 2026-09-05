#!/usr/bin/env bash
#
# Run the ADR 0097 black-box and real interoperability lanes against pinned
# Synapse and MAS containers. Runtime credentials and protocol material stay
# in memory or in the throwaway run directory, which is never uploaded.
set -euo pipefail

mode="${1:-}"
case "$mode" in
  api|acquire|grant|unsupported) ;;
  *)
    echo "usage: scripts/matrix-oauth-test.sh <api|acquire|grant|unsupported>" >&2
    exit 2
    ;;
esac

workspace_root=$(cd "$(dirname "$0")/.." && pwd)
cd "$workspace_root"
compose_file="$workspace_root/smoke/matrix-oauth/compose.yml"
fixture_dir="$workspace_root/smoke/matrix-oauth"
tmp_root="${TMPDIR:-/tmp}"
run_dir=$(mktemp -d "$tmp_root/axon-matrix-oauth.XXXXXX")
project=$(basename "$run_dir" | tr '[:upper:].' '[:lower:]-')
mas_config_volume="$project-mas-config"

cleanup() {
  docker compose -f "$compose_file" -p "$project" down -v --remove-orphans >/dev/null 2>&1 || true
  docker volume rm "$mas_config_volume" >/dev/null 2>&1 || true
  rm -rf -- "$run_dir"
}
trap cleanup EXIT INT TERM

free_port() {
  python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()'
}

export MATRIX_OAUTH_POSTGRES_PORT="${MATRIX_OAUTH_POSTGRES_PORT:-$(free_port)}"
export MATRIX_OAUTH_SYNAPSE_PORT="${MATRIX_OAUTH_SYNAPSE_PORT:-$(free_port)}"
export MATRIX_OAUTH_MAS_PORT="${MATRIX_OAUTH_MAS_PORT:-$(free_port)}"
export MATRIX_OAUTH_AXON_PORT="${MATRIX_OAUTH_AXON_PORT:-$(free_port)}"
export MATRIX_OAUTH_RUN_DIR="$run_dir"
export MATRIX_OAUTH_MAS_CONFIG_VOLUME="$mas_config_volume"

postgres_image=postgres:16
synapse_image=matrixdotorg/synapse:v1.160.0
mas_image=ghcr.io/element-hq/matrix-authentication-service:1.24.0
export MATRIX_OAUTH_POSTGRES_IMAGE="$postgres_image"
export MATRIX_OAUTH_SYNAPSE_IMAGE="$synapse_image"
export MATRIX_OAUTH_MAS_IMAGE="$mas_image"
docker volume create "$mas_config_volume" >/dev/null
docker run --rm --user 0 \
  -v "$mas_config_volume:/run/matrix-oauth" \
  "$mas_image" config generate --output /run/matrix-oauth/generated.yaml

device_code_grant_enabled=true
if [ "$mode" = unsupported ]; then
  device_code_grant_enabled=false
fi

sed \
  -e "s|@MAS_PUBLIC_BASE@|http://127.0.0.1:$MATRIX_OAUTH_MAS_PORT/|g" \
  -e "s|@DEVICE_CODE_GRANT_ENABLED@|$device_code_grant_enabled|g" \
  "$fixture_dir/mas-overrides.yaml.in" >"$run_dir/mas-overrides.yaml"
sed \
  -e "s|@SYNAPSE_PUBLIC_BASE@|http://127.0.0.1:$MATRIX_OAUTH_SYNAPSE_PORT/|g" \
  "$fixture_dir/synapse-homeserver.yaml" >"$run_dir/synapse-homeserver.yaml"
cp "$fixture_dir/synapse-log.config" "$run_dir/synapse-log.config"
mkdir -p "$run_dir/synapse"

docker compose -f "$compose_file" -p "$project" pull --quiet
docker compose -f "$compose_file" -p "$project" up -d

wait_http() {
  local label=$1
  local url=$2
  local deadline=$((SECONDS + 180))
  until curl --fail --silent --show-error --max-time 3 "$url" >/dev/null 2>&1; do
    if [ "$SECONDS" -ge "$deadline" ]; then
      echo "matrix-oauth: $label did not become ready" >&2
      exit 1
    fi
    sleep 2
  done
}

wait_http Synapse "http://127.0.0.1:$MATRIX_OAUTH_SYNAPSE_PORT/health"
wait_http MAS "http://127.0.0.1:$MATRIX_OAUTH_MAS_PORT/.well-known/openid-configuration"

mas() {
  docker compose -f "$compose_file" -p "$project" exec -T mas mas-cli "$@"
}

matrix_password='matrix-oauth-smoke-password-7uP4'
if ! mas manage register-user --yes --password "$matrix_password" --no-admin --ignore-password-complexity alice >/dev/null 2>&1; then
  echo "matrix-oauth: MAS test-user registration failed" >&2
  exit 1
fi
trusted_device_id=AXONQRTRUSTED
if ! compatibility_output=$(mas manage issue-compatibility-token alice "$trusted_device_id" 2>&1); then
  echo "matrix-oauth: MAS compatibility-session issuance failed" >&2
  exit 1
fi
compatibility_token=$(printf '%s\n' "$compatibility_output" | grep -Eo '(mct|syt)_[A-Za-z0-9_-]+' | tail -n 1 || true)
unset compatibility_output
if [ -z "$compatibility_token" ]; then
  echo "matrix-oauth: MAS compatibility-session output had no token" >&2
  exit 1
fi
whoami_status=$(printf 'header = "Authorization: Bearer %s"\n' "$compatibility_token" | \
  curl --config - --silent --output /dev/null --write-out '%{http_code}' --max-time 10 \
    "http://127.0.0.1:$MATRIX_OAUTH_SYNAPSE_PORT/_matrix/client/v3/account/whoami")
if [ "$whoami_status" != 200 ]; then
  echo "matrix-oauth: compatibility session was rejected by Synapse (HTTP $whoami_status)" >&2
  exit 1
fi

target_bin="${CARGO_TARGET_DIR:-$workspace_root/target}/debug"
"$workspace_root/scripts/check-smoke-isolation.sh" axon-smoke-matrix-oauth
cargo build -p axon-server -p axon-smoke-matrix-oauth

export AXON_SERVER_BIN="$target_bin/axon-server"
export MATRIX_OAUTH_HOMESERVER="http://127.0.0.1:$MATRIX_OAUTH_SYNAPSE_PORT"
export MATRIX_OAUTH_MAS_BASE="http://127.0.0.1:$MATRIX_OAUTH_MAS_PORT"
export MATRIX_OAUTH_DATABASE_URL="postgres://axon:axon@127.0.0.1:$MATRIX_OAUTH_POSTGRES_PORT/axon"
export MATRIX_OAUTH_COMPATIBILITY_TOKEN="$compatibility_token"
export MATRIX_OAUTH_MATRIX_PASSWORD="$matrix_password"
export MATRIX_OAUTH_USER_ID='@alice:localhost'
export MATRIX_OAUTH_TRUSTED_DEVICE_ID="$trusted_device_id"
export MATRIX_OAUTH_HARNESS_RUN_DIR="$run_dir/harness"

echo "matrix-oauth: running $mode lane against Synapse ${synapse_image##*:}, MAS ${mas_image##*:}, and matrix-sdk 0.18"
"$target_bin/axon-smoke-matrix-oauth" "$mode"

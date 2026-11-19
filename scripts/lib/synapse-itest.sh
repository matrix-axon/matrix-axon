# shellcheck shell=bash disable=SC2153  # AXON_BIN and friends are set by the caller
#
# Shared helpers for the end-to-end tests that run axon against a real Synapse
# (the `integration` compose profile): scripts/integration-test.sh and
# scripts/gap-fill-test.sh. Source it after setting:
#
#   REPO_ROOT       the repository root
#   POSTGRES_PORT   host port for the compose Postgres
#   SYNAPSE_PORT    host port for Synapse
#   TIMEOUT         seconds to wait for each condition
#
# The axon helpers read these globals, which the caller sets before using them:
#
#   AXON_BIN        the axon-server binary
#   RUN_DIR         axon's isolated working directory (holds axon.log)
#   DATABASE_URL    axon's database
#   ITEST_DB        the database `psql_q` queries
#   AXON_EXTRA_ENV  (optional array) extra `NAME=value` settings for axon

HS_URL="http://localhost:${SYNAPSE_PORT}"
AXON_BASE_URL="http://127.0.0.1:18080"
AXON_PID=""

# Exported so `docker compose` publishes the ports we expect.
export POSTGRES_PORT SYNAPSE_PORT

log()  { printf '\n\033[1;36m=== %s ===\033[0m\n' "$*" >&2; }
info() { printf '    %s\n' "$*" >&2; }
die()  { printf '\n\033[1;31mFAIL: %s\033[0m\n' "$*" >&2; exit 1; }

dc() { docker compose --profile integration "$@"; }

# admin_q runs against the default `axon` DB (for CREATE/DROP DATABASE);
# psql_q runs against the throwaway test DB (for assertions).
admin_q() { docker exec -i "$PG" psql -U axon -d axon -tAc "$1" | tr -d '[:space:]'; }
# While axon is still running migrations on the fresh DB the tables don't exist
# yet; psql then exits non-zero with "relation does not exist". We capture that
# as an empty string (never letting it trip `set -e`) so the poller just retries.
psql_q() {
    local out
    out="$(docker exec -i "$PG" psql -U axon -d "$ITEST_DB" -tAc "$1" 2>/dev/null)" || out=""
    printf '%s' "$out" | tr -d '[:space:]'
}

# itest_up — start Postgres + Synapse and wait until both are ready. Sets PG.
itest_up() {
    log "Starting Postgres + Synapse"
    dc up -d postgres synapse

    PG="$(docker compose ps -q postgres)"
    [ -n "$PG" ] || die "could not resolve the postgres container"

    log "Waiting for Postgres to accept connections"
    # Up to ~120s (matching the Synapse budget below): on a cold CI runner Postgres
    # runs first-boot initdb on a fresh volume while the Synapse image is still
    # extracting, and that contention can push readiness past a tight 30s window.
    local pg_ready=0
    for _ in $(seq 1 60); do
        if docker exec "$PG" pg_isready -U axon -d axon >/dev/null 2>&1; then pg_ready=1; break; fi
        sleep 2
    done
    if [ "$pg_ready" -ne 1 ]; then
        info "----- last 50 lines of postgres container logs -----"
        docker logs --tail 50 "$PG" >&2 || true
        die "Postgres never became ready"
    fi
    info "Postgres ready on host port ${POSTGRES_PORT}"

    log "Waiting for Synapse + Simplified Sliding Sync (MSC4186)"
    for _ in $(seq 1 60); do
        if curl -fsS "${HS_URL}/_matrix/client/versions" 2>/dev/null \
            | grep -q '"org.matrix.simplified_msc3575":[[:space:]]*true'; then
            break
        fi
        sleep 2
    done
    curl -fsS "${HS_URL}/_matrix/client/versions" 2>/dev/null \
        | grep -q '"org.matrix.simplified_msc3575":[[:space:]]*true' \
        || die "Synapse is not advertising MSC4186 (org.matrix.simplified_msc3575); axon speaks only this"
    info "Synapse healthy and advertising MSC4186"
}

# recreate_db <name> — drop and create a throwaway database. WITH (FORCE)
# terminates any leftover connections (Postgres 13+); axon runs its own
# migrations on first connect, so an empty DB is all it needs.
recreate_db() {
    admin_q "DROP DATABASE IF EXISTS $1 WITH (FORCE);" >/dev/null
    admin_q "CREATE DATABASE $1;" >/dev/null
    info "Created $1"
}

drop_db() {
    admin_q "DROP DATABASE IF EXISTS $1 WITH (FORCE);" >/dev/null 2>&1 || true
}

# register_user <localpart> <password> — register a non-admin user on Synapse.
register_user() {
    local syn
    syn="$(docker compose ps -q synapse)"
    # This command runs inside the Synapse container, so use its internal
    # listener; SYNAPSE_PORT is only the host-published port.
    docker exec "$syn" register_new_matrix_user \
        -c /data/homeserver.yaml -u "$1" -p "$2" --no-admin http://localhost:8008 \
        >/dev/null 2>&1
}

# run_axon — start axon in the background against the local servers, isolated
# from the project .env via cwd=$RUN_DIR. No account is configured (accounts
# are runtime-provisioned only, ADR 0024): axon boots with zero accounts and
# waits for the API. The same RUN_DIR keeps the SDK store across a restart.
run_axon() {
    local -a envs=(
        DATABASE_URL="$DATABASE_URL"
        # Pin the bind address: without it axon can resolve a non-loopback
        # default on a host that has one (a Tailscale/VPN interface, say) and
        # refuse to bind over plain HTTP, exiting before it syncs anything. The
        # harness would then fail 90s later on a timeout that says nothing
        # about the real cause.
        AXON_SERVER__HOST=127.0.0.1
        AXON_SERVER__PORT=18080
        AXON_SYNC__DATA_DIR="${RUN_DIR}/axon-data/sync"
        AXON_SEARCH__INDEX_PATH="${RUN_DIR}/axon-data/search"
        AXON_MEDIA__CACHE_DIR="${RUN_DIR}/axon-data/media"
        AXON_SYNC__STORE_KEY="itest-store-key"
        RUST_LOG="${RUST_LOG:-info,axon_sync=debug}"
    )
    if [ "${AXON_EXTRA_ENV+set}" = set ]; then
        envs+=("${AXON_EXTRA_ENV[@]}")
    fi
    (
        cd "$RUN_DIR" || exit
        exec env "${envs[@]}" "$AXON_BIN"
    ) >>"${RUN_DIR}/axon.log" 2>&1 &
    AXON_PID=$!
}

stop_axon() {
    [ -n "$AXON_PID" ] || return 0
    kill "$AXON_PID" 2>/dev/null || true
    wait "$AXON_PID" 2>/dev/null || true
    AXON_PID=""
}

# Poll axon's liveness probe until it responds, so API calls don't race the
# HTTP listener binding.
wait_for_axon_http() {
    local deadline=$(( $(date +%s) + TIMEOUT ))
    while :; do
        curl -fsS "${AXON_BASE_URL}/healthz" >/dev/null 2>&1 && return 0
        if [ "$(date +%s)" -ge "$deadline" ]; then
            info "----- last 40 lines of axon.log -----"
            tail -40 "${RUN_DIR}/axon.log" >&2 || true
            die "axon's HTTP listener never came up"
        fi
        sleep 1
    done
}

# Mint a bearer token through the same out-of-band CLI path operators use.
# Only the raw final line is captured; the secret is never logged.
issue_bearer_token() {
    (
        cd "$RUN_DIR" || exit
        DATABASE_URL="$DATABASE_URL" "$AXON_BIN" token issue --label integration-test
    ) | tail -n 1
}

# login_axon <token> <user_id> <password> — POST /v1/accounts/login; prints the
# resulting account_id.
login_axon() {
    local token="$1" user_id="$2" password="$3"
    curl -fsS -XPOST "${AXON_BASE_URL}/v1/accounts/login" \
        -H "Authorization: Bearer ${token}" -H 'Content-Type: application/json' \
        -d "$(python3 -c "import json,sys; print(json.dumps({'username': sys.argv[1], 'homeserver_url': sys.argv[2], 'password': sys.argv[3]}))" "$user_id" "$HS_URL" "$password")" \
        | python3 -c "import sys,json; print(json.load(sys.stdin)['data']['account_id'])"
}

# wait_until <description> <op> <want> <count-fn> — poll until the count
# satisfies the test. <op> is a `test`/`[ ]` integer operator: -eq, -ge, etc.
wait_until() {
    local desc="$1" op="$2" want="$3" fn="$4" got deadline
    deadline=$(( $(date +%s) + TIMEOUT ))
    while :; do
        got="$("$fn")"
        if [ -n "$got" ] && test "$got" "$op" "$want" 2>/dev/null; then
            info "${desc}: ${got} (${op} ${want}, ok)"
            return 0
        fi
        if [ "$(date +%s)" -ge "$deadline" ]; then
            info "----- last 40 lines of axon.log -----"
            tail -40 "${RUN_DIR}/axon.log" >&2 || true
            die "timed out waiting for ${desc}: wanted ${op} ${want}, got ${got:-<none>}"
        fi
        sleep 2
    done
}

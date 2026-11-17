#!/usr/bin/env bash
#
# End-to-end test for ADR 0140 (#1125): a burst of more than
# `sync.timeline_limit` events between two syncs must be stored in full, with
# history backfill both on and off.
#
# Against a real Synapse (the `integration` compose profile), for each of
# `sync.backfill_enabled = true` and `false`:
#
#   1. A user creates a plain room and sends a few messages.
#   2. A fresh axon logs the user in and syncs the room. With backfill on, it
#      waits until backfill has reached the room's start, so ordinary backfill
#      has nothing left to page there.
#   3. axon stops. The user sends BURST messages, more than the timeline limit.
#   4. axon restarts on the same SDK store, so sliding sync resumes from its
#      saved position and the room arrives `limited`, with only the newest
#      events. Assert every burst event is stored, that a hole was recorded,
#      and that no gap row is left open.
#
# Without the gap filler the burst's older events are never fetched, and step 4
# times out.
#
# Configuration via env vars (all have defaults):
#
#   POSTGRES_PORT   host port for the compose Postgres        (default 5432)
#   SYNAPSE_PORT    host port for Synapse                     (default 8008)
#   BURST           messages sent while axon is down          (default 60)
#   TIMEOUT         seconds to wait for each condition        (default 90)
#   KEEP_UP=1       leave containers and databases up at the end
#   AXON_BIN        test this prebuilt axon-server instead of building one
#                   (a build without the fix must fail)
#
set -euo pipefail

POSTGRES_PORT=${POSTGRES_PORT:-5432}
SYNAPSE_PORT=${SYNAPSE_PORT:-8008}
BURST=${BURST:-60}
TIMEOUT=${TIMEOUT:-90}
KEEP_UP=${KEEP_UP:-0}
INITIAL=5

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# shellcheck source=lib/synapse-itest.sh
. "${REPO_ROOT}/scripts/lib/synapse-itest.sh"

# The SDK's own default room-subscription window is 20, so a burst at or
# below that might not arrive limited, and would test nothing.
[ "$BURST" -gt 20 ] || die "BURST must exceed 20 so the restarted sync is limited"

itest_up

if [ -z "${AXON_BIN:-}" ]; then
    log "Building axon-server"
    cargo build -p axon-server
    AXON_BIN="${REPO_ROOT}/target/debug/axon-server"
fi
[ -x "$AXON_BIN" ] || die "axon binary not found at $AXON_BIN"
info "testing $("$AXON_BIN" --version)"

RUN_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/axon-gapfill.XXXXXX")"
DATABASES=()

cleanup() {
    local code=$?
    stop_axon
    if [ "$KEEP_UP" != "1" ]; then
        rm -rf "$RUN_ROOT" 2>/dev/null || true
        for db in "${DATABASES[@]}"; do drop_db "$db"; done
        log "Tearing down containers (KEEP_UP=1 to keep them)"
        dc down >/dev/null 2>&1 || true
    else
        info "kept run state under ${RUN_ROOT}"
    fi
    exit "$code"
}
trap cleanup EXIT

# --- the Matrix side, driven directly over the client-server API -------------

# hs <method> <path> [json] — one authenticated client-server call; prints the
# response body.
hs() {
    curl -fsS -X "$1" "${HS_URL}$2" \
        -H "Authorization: Bearer ${HS_TOKEN}" -H 'Content-Type: application/json' \
        ${3:+-d "$3"}
}

json_field() { python3 -c "import sys,json; print(json.load(sys.stdin)['$1'])"; }

# send_messages <count> <label> — send plain text messages to the room,
# appending each event id to SENT_IDS.
send_messages() {
    local count="$1" label="$2" i event_id
    for i in $(seq 1 "$count"); do
        event_id="$(hs PUT "/_matrix/client/v3/rooms/${ROOM_ID}/send/m.room.message/${label}-${i}-$$" \
            "{\"msgtype\":\"m.text\",\"body\":\"${label} ${i}\"}" | json_field event_id)"
        SENT_IDS+=("$event_id")
    done
}

# --- one run per backfill setting ----------------------------------------------

run_case() {
    local backfill="$1"
    local suffix
    suffix="$(date +%s)-$$-${backfill}"
    local localpart="gapfill-${suffix}"
    USER_ID="@${localpart}:localhost"
    PASSWORD="pass-${suffix}"
    ITEST_DB="axon_gapfill_${backfill}"
    DATABASE_URL="postgres://axon:axon@127.0.0.1:${POSTGRES_PORT}/${ITEST_DB}"
    RUN_DIR="${RUN_ROOT}/backfill-${backfill}"
    AXON_EXTRA_ENV=("AXON_SYNC__BACKFILL_ENABLED=${backfill}")
    mkdir -p "$RUN_DIR"
    DATABASES+=("$ITEST_DB")

    log "backfill_enabled=${backfill}: user, room and a few messages"
    recreate_db "$ITEST_DB"
    register_user "$localpart" "$PASSWORD" || die "user registration failed"
    HS_TOKEN="$(curl -fsS -XPOST "${HS_URL}/_matrix/client/v3/login" -H 'Content-Type: application/json' \
        -d "$(python3 -c "import json,sys; print(json.dumps({'type':'m.login.password','identifier':{'type':'m.id.user','user':sys.argv[1]},'password':sys.argv[2],'device_id':'GAPFILLSEED'}))" "$localpart" "$PASSWORD")" \
        | json_field access_token)"
    ROOM_ID="$(hs POST /_matrix/client/v3/createRoom '{"preset":"private_chat","name":"gap fill"}' | json_field room_id)"
    SENT_IDS=()
    send_messages "$INITIAL" initial
    info "room ${ROOM_ID}"

    log "backfill_enabled=${backfill}: axon syncs the room"
    run_axon
    wait_for_axon_http
    local token account_id
    token="$(issue_bearer_token)"
    [ -n "$token" ] || die "could not issue a bearer token"
    account_id="$(login_axon "$token" "$USER_ID" "$PASSWORD")"
    [ -n "$account_id" ] || die "login did not return an account_id"
    info "account ${account_id}"
    sent_stored() {
        local ids
        ids="$(printf "'%s'," "${SENT_IDS[@]}")"
        psql_q "SELECT count(*) FROM events WHERE room_id = '${ROOM_ID}' AND event_id IN (${ids%,})"
    }
    wait_until "initial messages stored" -eq "${#SENT_IDS[@]}" sent_stored
    if [ "$backfill" = true ]; then
        backfill_complete() {
            psql_q "SELECT count(*) FROM room_backfill WHERE room_id = '${ROOM_ID}' AND complete"
        }
        wait_until "room backfilled to its start" -eq 1 backfill_complete
    fi
    stop_axon

    log "backfill_enabled=${backfill}: ${BURST} messages while axon is down"
    send_messages "$BURST" burst
    info "sent ${#SENT_IDS[@]} messages in all"

    log "backfill_enabled=${backfill}: axon restarts; the room arrives limited"
    run_axon
    wait_for_axon_http
    wait_until "every message stored" -eq "${#SENT_IDS[@]}" sent_stored
    grep -q "limited sync left a hole in the room's history" "${RUN_DIR}/axon.log" \
        || die "axon never recorded a hole, so the burst did not exercise the gap filler"
    open_gaps() { psql_q "SELECT count(*) FROM room_timeline_gaps"; }
    wait_until "gap rows closed" -eq 0 open_gaps
    local recovered
    recovered="$(curl -fsS -H "Authorization: Bearer ${token}" "${AXON_BASE_URL}/v1/status" \
        | python3 -c "import sys,json; g=json.load(sys.stdin)['data']['history_gaps']; print(g['events_recovered'] if g['available'] and g['open'] == 0 else -1)")"
    [ "$recovered" -gt 0 ] || die "GET /v1/status did not report recovered events (got ${recovered})"
    info "GET /v1/status: history_gaps.events_recovered=${recovered}"
    stop_axon
}

run_case true
run_case false

log "PASS — every message of a ${BURST}-message burst was stored, with backfill on and off"

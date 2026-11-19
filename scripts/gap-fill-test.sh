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
# A third run (backfill off) restarts axon while the hole's insert is stuck on a
# lock held on the gap table, sends a message meanwhile, then restarts it again
# once the lock is gone. The hole must survive: its row has to be committed
# before the SDK saves its sync position past the limited response.
#
# Without the gap filler the burst's older events are never fetched, and step 4
# times out.
#
# Two more runs cover the one-time verification pass (ADR 0140 decision 4),
# which repairs holes that have no gap row:
#
#   * history-lost: axon stores every message live, then loses most of them
#     (deleted from its database while it is down, as holes from before ADR
#     0140 have no record). The management API queues the room's verification,
#     and the walk must store every message again.
#   * upgrade (only with AXON_OLD_BIN): a build from before ADR 0140 leaves a
#     real limited-sync hole with no record of it. The build under test then
#     starts on the same database and SDK store; its migration queues the room,
#     and the pass must fill the hole with nothing else asked of it.
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
#   AXON_OLD_BIN    an axon-server built from before ADR 0140, for the upgrade
#                   run; without it that run is skipped
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
        for db in ${DATABASES[@]+"${DATABASES[@]}"}; do drop_db "$db"; done
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

# --- the gap table lock for the restart case -------------------------------------

# Seconds the lock is held: longer than axon waits before it stops below.
LOCK_SECS=15

# hold_gap_lock — lock the gap table against inserts in the background, as a
# long transaction elsewhere would.
hold_gap_lock() {
    docker exec -d "$PG" psql -U axon -d "$ITEST_DB" -c \
        "BEGIN; LOCK TABLE room_timeline_gaps IN SHARE ROW EXCLUSIVE MODE; SELECT pg_sleep(${LOCK_SECS}); COMMIT;"
}
gap_lock_held() {
    psql_q "SELECT count(*) FROM pg_locks l JOIN pg_class c ON c.oid = l.relation
            WHERE c.relname = 'room_timeline_gaps' AND l.mode = 'ShareRowExclusiveLock' AND l.granted"
}
gap_inserts_waiting() {
    psql_q "SELECT count(*) FROM pg_stat_activity
            WHERE datname = current_database() AND wait_event_type = 'Lock'
              AND query LIKE 'INSERT INTO room_timeline_gaps%'"
}

# --- one run per case -------------------------------------------------------------

# run_case <backfill_enabled> [restart-while-locked]
run_case() {
    local backfill="$1" locked="${2:-}"
    local name="backfill-${backfill}${locked:+-${locked}}"
    local suffix
    suffix="$(date +%s)-$$-${backfill}"
    local localpart="gapfill-${suffix}"
    USER_ID="@${localpart}:localhost"
    PASSWORD="pass-${suffix}"
    ITEST_DB="axon_gapfill_${backfill}${locked:+_locked}"
    DATABASE_URL="postgres://axon:axon@127.0.0.1:${POSTGRES_PORT}/${ITEST_DB}"
    RUN_DIR="${RUN_ROOT}/${name}"
    AXON_EXTRA_ENV=("AXON_SYNC__BACKFILL_ENABLED=${backfill}")
    mkdir -p "$RUN_DIR"
    DATABASES+=("$ITEST_DB")

    log "${name}: user, room and a few messages"
    recreate_db "$ITEST_DB"
    register_user "$localpart" "$PASSWORD" || die "user registration failed"
    HS_TOKEN="$(curl -fsS -XPOST "${HS_URL}/_matrix/client/v3/login" -H 'Content-Type: application/json' \
        -d "$(python3 -c "import json,sys; print(json.dumps({'type':'m.login.password','identifier':{'type':'m.id.user','user':sys.argv[1]},'password':sys.argv[2],'device_id':'GAPFILLSEED'}))" "$localpart" "$PASSWORD")" \
        | json_field access_token)"
    ROOM_ID="$(hs POST /_matrix/client/v3/createRoom '{"preset":"private_chat","name":"gap fill"}' | json_field room_id)"
    SENT_IDS=()
    send_messages "$INITIAL" initial
    info "room ${ROOM_ID}"

    log "${name}: axon syncs the room"
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

    log "${name}: ${BURST} messages while axon is down"
    send_messages "$BURST" burst
    info "sent ${#SENT_IDS[@]} messages in all"

    if [ -n "$locked" ]; then
        log "${name}: axon restarts while the gap table is locked, then stops"
        hold_gap_lock
        wait_until "gap table locked" -eq 1 gap_lock_held
        run_axon
        wait_for_axon_http
        wait_until "the hole's insert waiting on the lock" -ge 1 gap_inserts_waiting
        # A later message while the insert waits. A watcher that let the SDK
        # move on would store it, and the SDK would save its position past the
        # limited response, so the restart below would never see the hole.
        send_messages 1 during-lock
        sleep 3
        stop_axon
        wait_until "gap table unlocked" -eq 0 gap_lock_held
        log "${name}: axon restarts again"
        run_axon
        wait_for_axon_http
        send_messages 1 after-restart
    else
        log "${name}: axon restarts; the room arrives limited"
        run_axon
        wait_for_axon_http
    fi
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

# --- the verification pass (ADR 0140 decision 4) -----------------------------

# verify_setup <name> — a fresh user, database and room with a few messages.
verify_setup() {
    local name="$1" suffix
    suffix="$(date +%s)-$$-${name}"
    local localpart="gapfill-${suffix}"
    USER_ID="@${localpart}:localhost"
    PASSWORD="pass-${suffix}"
    ITEST_DB="axon_gapfill_${name//-/_}"
    DATABASE_URL="postgres://axon:axon@127.0.0.1:${POSTGRES_PORT}/${ITEST_DB}"
    RUN_DIR="${RUN_ROOT}/${name}"
    AXON_EXTRA_ENV=("AXON_SYNC__BACKFILL_ENABLED=false")
    mkdir -p "$RUN_DIR"
    DATABASES+=("$ITEST_DB")

    log "${name}: user, room and a few messages"
    recreate_db "$ITEST_DB"
    register_user "$localpart" "$PASSWORD" || die "user registration failed"
    HS_TOKEN="$(curl -fsS -XPOST "${HS_URL}/_matrix/client/v3/login" -H 'Content-Type: application/json' \
        -d "$(python3 -c "import json,sys; print(json.dumps({'type':'m.login.password','identifier':{'type':'m.id.user','user':sys.argv[1]},'password':sys.argv[2],'device_id':'GAPFILLSEED'}))" "$localpart" "$PASSWORD")" \
        | json_field access_token)"
    ROOM_ID="$(hs POST /_matrix/client/v3/createRoom '{"preset":"private_chat","name":"history verify"}' | json_field room_id)"
    SENT_IDS=()
    send_messages "$INITIAL" initial
    info "room ${ROOM_ID}"
}

# verify_login — start axon and log the user in; sets TOKEN and ACCOUNT_ID.
verify_login() {
    run_axon
    wait_for_axon_http
    TOKEN="$(issue_bearer_token)"
    [ -n "$TOKEN" ] || die "could not issue a bearer token"
    ACCOUNT_ID="$(login_axon "$TOKEN" "$USER_ID" "$PASSWORD")"
    [ -n "$ACCOUNT_ID" ] || die "login did not return an account_id"
    info "account ${ACCOUNT_ID}"
}

stored_count() {
    local ids
    ids="$(printf "'%s'," "${SENT_IDS[@]}")"
    psql_q "SELECT count(*) FROM events WHERE room_id = '${ROOM_ID}' AND event_id IN (${ids%,})"
}

# verify_finished <name> — wait for the room's walk to close at its first
# stored event with every message stored, and check what it reported.
verify_finished() {
    local name="$1" recovered
    wait_until "every message stored" -eq "${#SENT_IDS[@]}" stored_count
    walk_done() {
        psql_q "SELECT count(*) FROM room_history_verify
                WHERE room_id = '${ROOM_ID}' AND complete AND stop_reason = 'first_stored_event'"
    }
    wait_until "the room's walk closed at its first stored event" -eq 1 walk_done
    grep -q "history verify: stored history that was missing from the room" "${RUN_DIR}/axon.log" \
        || die "${name}: axon never logged the history the walk recovered"
    recovered="$(curl -fsS -H "Authorization: Bearer ${TOKEN}" "${AXON_BASE_URL}/v1/status" \
        | python3 -c "import sys,json; v=json.load(sys.stdin)['data']['history_verify']; print(v['events_recovered'] if v['available'] and v['rooms_pending'] == 0 else -1)")"
    [ "$recovered" -gt 0 ] || die "${name}: GET /v1/status did not report recovered events (got ${recovered})"
    info "GET /v1/status: history_verify.events_recovered=${recovered}"
}

# History axon once stored is lost while it is down; the management API
# queues the room's walk, which stores it again.
run_history_lost_case() {
    local name="history-lost" lost
    verify_setup "$name"
    log "${name}: axon stores every message live"
    verify_login
    send_messages 30 live
    wait_until "every message stored" -eq "${#SENT_IDS[@]}" stored_count
    stop_axon

    # The 25 oldest live messages: newer than the room's first stored events,
    # so only a walk from the live end reaches them.
    lost="$(printf "'%s'," "${SENT_IDS[@]:INITIAL:25}")"
    psql_q "WITH d AS (DELETE FROM events WHERE event_id IN (${lost%,}) RETURNING 1)
            SELECT count(*) FROM d" >/dev/null
    [ "$(stored_count)" -eq $(( ${#SENT_IDS[@]} - 25 )) ] || die "${name}: the history was not lost"

    log "${name}: axon restarts, and the management API queues the room's walk"
    run_axon
    wait_for_axon_http
    local queued
    queued="$(curl -fsS -XPOST "${AXON_BASE_URL}/v1/management/accounts/${ACCOUNT_ID}/history-verify" \
        -H "Authorization: Bearer ${TOKEN}" -H 'Content-Type: application/json' \
        -d "$(python3 -c "import json,sys; print(json.dumps({'room_id': sys.argv[1]}))" "$ROOM_ID")" \
        | python3 -c "import sys,json; print(json.load(sys.stdin)['data']['rooms_queued'])")"
    [ "$queued" -eq 1 ] || die "${name}: the management API queued ${queued} rooms"
    verify_finished "$name"
    stop_axon
}

# A build from before ADR 0140 leaves a limited-sync hole with no row. The
# build under test starts on the same database and SDK store; its migration
# queues the room, and the pass fills the hole unasked.
run_upgrade_case() {
    local name="upgrade" new_bin="$AXON_BIN"
    verify_setup "$name"
    AXON_BIN="$AXON_OLD_BIN"
    log "${name}: the old build syncs the room"
    verify_login
    wait_until "initial messages stored" -eq "${#SENT_IDS[@]}" stored_count
    stop_axon

    log "${name}: ${BURST} messages while axon is down; the old build restarts"
    send_messages "$BURST" burst
    run_axon
    wait_for_axon_http
    newest_stored() {
        psql_q "SELECT count(*) FROM events WHERE event_id = '${SENT_IDS[${#SENT_IDS[@]}-1]}'"
    }
    wait_until "the limited batch stored" -eq 1 newest_stored
    sleep 3
    stop_axon
    [ "$(stored_count)" -lt "${#SENT_IDS[@]}" ] \
        || die "${name}: the old build stored every message, so there is no hole to repair"
    info "the old build stored $(stored_count) of ${#SENT_IDS[@]} messages"

    AXON_BIN="$new_bin"
    log "${name}: the build under test starts on the same database"
    run_axon
    wait_for_axon_http
    verify_finished "$name"
    stop_axon
}

run_case true
run_case false
run_case false restart-while-locked
run_history_lost_case
if [ -n "${AXON_OLD_BIN:-}" ]; then
    [ -x "$AXON_OLD_BIN" ] || die "old axon binary not found at $AXON_OLD_BIN"
    info "upgrading from $("$AXON_OLD_BIN" --version)"
    run_upgrade_case
else
    info "AXON_OLD_BIN unset; skipping the upgrade run"
fi

log "PASS — every message of a ${BURST}-message burst was stored: backfill on, off, and across a restart while the hole's insert was blocked; lost history was restored by the verification pass"

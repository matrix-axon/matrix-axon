#!/usr/bin/env bash
#
# End-to-end integration test for re-decryption and encrypted media proxying
# against a real Synapse. It exercises the whole prize path with no manual steps:
#
#   1. Bring up Postgres + Synapse (the `integration` compose profile).
#   2. Register a fresh, unique user and run the seeder ("device A"): it makes
#      an encrypted room, sends messages plus an image attachment, and mints a
#      Secure Backup recovery key.
#   3. Start axon (with no account — accounts are runtime-provisioned, ADR 0024)
#      and log "device B" in via `POST /v1/accounts/login` with NO recovery key —
#      assert the messages land as UTDs (m.room.encrypted rows with content IS NULL).
#   4. Call `POST /v1/accounts/{id}/recover` WITH the recovery key — assert the
#      rows flip to decrypted m.room.message with content populated (the
#      re-decryption queue working). No restart needed.
#   5. Fetch the attachment through axon's media route and compare the decrypted
#      response byte-for-byte with the original image.
#
# Runs locally and in CI. Configuration via env vars (all have defaults):
#
#   POSTGRES_PORT   host port for the compose Postgres        (default 5432)
#   SYNAPSE_PORT    host port for Synapse                     (default 8008)
#   MESSAGE_COUNT   how many messages the seeder sends        (default 3)
#   TIMEOUT         seconds to wait for each DB condition      (default 90)
#   KEEP_UP=1       leave containers running at the end (faster local iteration)
#
# On macOS a Homebrew Postgres often holds 5432, so locally you'll typically run:
#
#   POSTGRES_PORT=5433 scripts/integration-test.sh
#
set -euo pipefail

POSTGRES_PORT=${POSTGRES_PORT:-5432}
SYNAPSE_PORT=${SYNAPSE_PORT:-8008}
MESSAGE_COUNT=${MESSAGE_COUNT:-3}
TIMEOUT=${TIMEOUT:-90}
KEEP_UP=${KEEP_UP:-0}

# A dedicated, throwaway database (dropped + recreated each run) keeps the test
# isolated from the shared dev `axon` DB — otherwise axon tries to sync every
# account row it finds, and stale accounts from other work spam decrypt errors.
ITEST_DB="${ITEST_DB:-axon_itest}"
DATABASE_URL="postgres://axon:axon@127.0.0.1:${POSTGRES_PORT}/${ITEST_DB}"

# Resolve repo root from this script's location so it runs from anywhere.
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# shellcheck source=lib/synapse-itest.sh
. "${REPO_ROOT}/scripts/lib/synapse-itest.sh"

# --- bring up services ------------------------------------------------------

itest_up

log "Creating throwaway test database ${ITEST_DB}"
recreate_db "$ITEST_DB"

# --- build --------------------------------------------------------------------

log "Building axon-server + seeder"
cargo build -p axon-server -p axon-itest

AXON_BIN="${REPO_ROOT}/target/debug/axon-server"
SEED_BIN="${REPO_ROOT}/target/debug/seed"
[ -x "$AXON_BIN" ] || die "axon binary not found at $AXON_BIN"
[ -x "$SEED_BIN" ] || die "seed binary not found at $SEED_BIN"

# --- unique identities + run state -------------------------------------------

SUFFIX="$(date +%s)-$$"
LOCALPART="alice-${SUFFIX}"
USER_ID="@${LOCALPART}:localhost"
PASSWORD="pass-${SUFFIX}"
# Isolated run dir: axon loads .env via dotenvy from cwd upward, and the project
# .env points at a *remote* account. Running from here keeps us off it, and the
# SDK/search/media paths are pinned under here so device B starts unverified and
# the run leaves no platform-dir state behind. The same dir is reused on restart
# so the device persists.
RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/axon-itest.XXXXXX")"
MEDIA_FIXTURE="${RUN_DIR}/media-verification.png"
DOWNLOADED_MEDIA="${RUN_DIR}/media-downloaded.png"

cleanup() {
    local code=$?
    [ -n "$AXON_PID" ] && kill "$AXON_PID" 2>/dev/null || true
    rm -rf "$RUN_DIR" 2>/dev/null || true
    if [ "$KEEP_UP" != "1" ]; then
        # Drop the throwaway DB, then the containers. (KEEP_UP=1 leaves both for
        # inspection — the seeding user on Synapse is unique per run regardless.)
        drop_db "$ITEST_DB"
        log "Tearing down containers (KEEP_UP=1 to keep them)"
        dc down >/dev/null 2>&1 || true
    fi
    exit "$code"
}
trap cleanup EXIT

# A deterministic valid 1x1 PNG. Device A encrypts and uploads these bytes; the
# final assertion proves axon's media route returns the exact plaintext again.
python3 - "$MEDIA_FIXTURE" <<'PY'
import base64
import pathlib
import sys

png = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII="
)
pathlib.Path(sys.argv[1]).write_bytes(png)
PY

log "Registering seeding user ${USER_ID}"
register_user "$LOCALPART" "$PASSWORD" || die "user registration failed"
info "Registered ${USER_ID}"

# --- seed (device A) ----------------------------------------------------------

log "Seeding encrypted room + key backup (device A)"
SEED_JSON="$(
    SEED_HOMESERVER="$HS_URL" \
    SEED_USER_ID="$USER_ID" \
    SEED_PASSWORD="$PASSWORD" \
    SEED_STORE_DIR="${RUN_DIR}/seed-store" \
    SEED_MESSAGE_COUNT="$MESSAGE_COUNT" \
    SEED_MEDIA_FILE="$MEDIA_FIXTURE" \
    RUST_LOG="${RUST_LOG:-warn}" \
    "$SEED_BIN"
)"
[ -n "$SEED_JSON" ] || die "seeder produced no output"

# Parse the seeder's JSON (python3 is present locally and on CI runners).
read_json() {
    printf '%s' "$SEED_JSON" \
        | python3 -c "import sys,json; value=json.load(sys.stdin)['$1']; print('' if value is None else value)"
}
ROOM_ID="$(read_json room_id)"
RECOVERY_KEY="$(read_json recovery_key)"
MEDIA_EVENT_ID="$(read_json media_event_id)"
[ -n "$ROOM_ID" ] || die "no room_id from seeder"
[ -n "$RECOVERY_KEY" ] || die "no recovery_key from seeder"
[ -n "$MEDIA_EVENT_ID" ] || die "no media_event_id from seeder"
info "room=${ROOM_ID}  recovery_key=${RECOVERY_KEY:0:9}…  messages=${MESSAGE_COUNT}  media=${MEDIA_EVENT_ID}"

# --- helpers for running axon + polling the DB -------------------------------

# Count UTDs (encrypted, not yet decrypted) for this run's user.
utd_count() {
    psql_q "SELECT count(*) FROM events e JOIN accounts a USING (account_id)
            WHERE a.user_id = '${USER_ID}'
              AND e.event_type = 'm.room.encrypted' AND e.content IS NULL;"
}
# Count back-filled (decrypted) message rows for this run's user.
decrypted_count() {
    psql_q "SELECT count(*) FROM events e JOIN accounts a USING (account_id)
            WHERE a.user_id = '${USER_ID}'
              AND e.event_type = 'm.room.message' AND e.content IS NOT NULL;"
}
# Whether the seeded attachment has decrypted content with an encrypted MXC
# descriptor. This is the metadata the media route needs to decrypt the bytes.
media_event_ready() {
    psql_q "SELECT count(*) FROM events e JOIN accounts a USING (account_id)
            WHERE a.user_id = '${USER_ID}'
              AND e.event_id = '${MEDIA_EVENT_ID}'
              AND e.event_type = 'm.room.message'
              AND e.content->'file'->>'url' LIKE 'mxc://%';"
}

# recover_axon <token> <account_id> <recovery_key> — POST the recovery key so
# the already-logged-in device imports the key backup + cross-signing keys.
recover_axon() {
    local token="$1" account_id="$2" recovery="$3"
    curl -fsS -XPOST "${AXON_BASE_URL}/v1/accounts/${account_id}/recover" \
        -H "Authorization: Bearer ${token}" -H 'Content-Type: application/json' \
        -d "$(python3 -c "import json,sys; print(json.dumps({'recovery_key': sys.argv[1]}))" "$recovery")" \
        >/dev/null
}

# --- start axon + mint a bearer token -----------------------------------------
#
# One long-lived axon process for the whole test: accounts are runtime-only
# (ADR 0024), so "restart with the recovery key" becomes "call /recover on the
# already-running device" — no restart, no second boot.

log "Starting axon (no account configured) and minting a bearer token"
run_axon
wait_for_axon_http
TOKEN="$(issue_bearer_token)"
[ -n "$TOKEN" ] || die "could not issue client bearer token"

# --- phase 1: fresh device B, login with no recovery key -> UTDs accumulate --
#
# axon archives the events Simplified Sliding Sync surfaces. The per-room timeline
# window is raised from the SDK default of 1 (sync.timeline_limit, default 20), so
# device B sees several UTDs rather than just the latest — but still bounded by
# that window, not the full backlog. We assert on the count axon *actually*
# archived rather than MESSAGE_COUNT, then prove that set flips (and that
# nothing is left undecrypted once backfill has caught up).

log "Phase 1: log in fresh device B (no recovery key) — expect UTDs"
ACCOUNT_ID="$(login_axon "$TOKEN" "$USER_ID" "$PASSWORD")"
[ -n "$ACCOUNT_ID" ] || die "login did not return an account_id"
info "logged in as account ${ACCOUNT_ID}"
wait_until "UTDs accumulated" -ge 1 utd_count
SEEN="$(utd_count)"
info "device B archived ${SEEN} encrypted event(s) as UTD(s)"
[ "$(decrypted_count)" = "0" ] || die "expected 0 decrypted rows before recovery, got $(decrypted_count)"

# --- phase 2: supply the recovery key -> rows flip to decrypted --------------

log "Phase 2: POST the recovery key to the running device — expect re-decryption"
recover_axon "$TOKEN" "$ACCOUNT_ID" "$RECOVERY_KEY"
# `-ge`, not `-eq`: backfill pages history behind the sync window and decrypts
# that too, so on a room deeper than `sync.timeline_limit` the count legitimately
# exceeds what phase 1 saw. "No UTDs remain" below is the assertion that bites.
wait_until "rows back-filled to decrypted" -ge "$SEEN" decrypted_count
# And no UTDs should remain.
wait_until "UTDs drained to zero" -eq 0 utd_count
wait_until "encrypted media metadata available" -eq 1 media_event_ready

# --- phase 3: proxy download -> original plaintext bytes --------------------

log "Phase 3: fetch encrypted attachment through axon's media proxy"
MEDIA_MXC="$(
    psql_q "SELECT e.content->'file'->>'url' FROM events e
            JOIN accounts a USING (account_id)
            WHERE a.user_id = '${USER_ID}' AND e.event_id = '${MEDIA_EVENT_ID}';"
)"
[ -n "$MEDIA_MXC" ] || die "could not resolve encrypted media MXC URL"

MEDIA_PATH="${MEDIA_MXC#mxc://}"
MEDIA_SERVER="${MEDIA_PATH%%/*}"
MEDIA_ID="${MEDIA_PATH#*/}"
[ "$MEDIA_PATH" != "$MEDIA_MXC" ] || die "invalid media MXC URL: ${MEDIA_MXC}"
[ -n "$MEDIA_SERVER" ] && [ "$MEDIA_ID" != "$MEDIA_PATH" ] && [ -n "$MEDIA_ID" ] \
    || die "invalid media MXC components: ${MEDIA_MXC}"

curl --globoff -fsS -o "$DOWNLOADED_MEDIA" \
    -H "Authorization: Bearer ${TOKEN}" \
    "${AXON_BASE_URL}/v1/media/${ACCOUNT_ID}/${MEDIA_SERVER}/${MEDIA_ID}" \
    || die "media proxy request failed"
if ! cmp -s "$MEDIA_FIXTURE" "$DOWNLOADED_MEDIA"; then
    info "expected bytes: $(wc -c < "$MEDIA_FIXTURE" | tr -d '[:space:]')"
    info "received bytes: $(wc -c < "$DOWNLOADED_MEDIA" | tr -d '[:space:]')"
    die "media proxy response did not match the original plaintext image"
fi
info "media proxy returned $(wc -c < "$DOWNLOADED_MEDIA" | tr -d '[:space:]') exact plaintext bytes"
stop_axon

log "PASS — re-decrypted ${SEEN}/${SEEN} UTD(s) and verified encrypted media byte-for-byte"

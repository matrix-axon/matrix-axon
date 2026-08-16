# Integration testing against a local Synapse

axon's unit tests and the `#[ignore]` Postgres tests don't exercise the real
thing: syncing against a live Matrix homeserver, (M3c) re-decrypting UTDs once
megolm keys arrive, and downloading an encrypted attachment as plaintext through
the media proxy. This doc covers the **automated** end-to-end test plus the manual
walkthrough it's built from (handy for debugging).

The homeserver is **localhost-only and insecure by design** (no TLS, sqlite,
dev secrets, wide-open rate limits — see `docker/synapse/homeserver.yaml`). Never
expose it or reuse its secrets.

## Quick start: the automated script

One command runs the whole prize path — seed an encrypted room with messages and
an image attachment, log axon in as a fresh device that can't decrypt it, then
POST it the recovery key, watch the UTDs flip to decrypted, and fetch the
original image bytes through axon's media proxy:

```sh
# macOS: a Homebrew Postgres usually holds 5432, so publish the compose one on 5433.
POSTGRES_PORT=5433 scripts/integration-test.sh
# Linux / CI (5432 free): just run it.
scripts/integration-test.sh
```

What it does, with no manual steps:

1. Brings up Postgres + Synapse (the `integration` compose profile) and waits for
   Synapse to advertise Simplified Sliding Sync (MSC4186) — axon speaks _only_
   this, so it's the make-or-break capability.
2. Creates a throwaway `axon_itest` database (dropped + recreated each run) so the
   test is fully isolated from your dev `axon` DB.
3. Registers a unique user and runs the **seeder** (`crates/axon-itest`, playing a
   normal verified "device A"): creates an encrypted room, sends messages and a
   deterministic PNG attachment, and enables Secure Backup to mint a fresh
   recovery key.
4. Starts axon with **no** account configured (accounts are runtime-provisioned
   only, ADR 0024) and logs "device B" in via `POST /v1/accounts/login` with
   **no** recovery key; asserts the message lands as a UTD (`m.room.encrypted`,
   `content IS NULL`).
5. Calls `POST /v1/accounts/{id}/recover` **with** the recovery key against the
   same running device (no restart) and asserts the row flips to a decrypted
   `m.room.message` and the UTD backlog drains to zero.
6. Reads the attachment's decrypted `content.file` metadata from the event,
   requests its MXC URL through `GET /v1/media/{account_id}/{server}/{media_id}`,
   and compares the response byte-for-byte with the original PNG.

On success it prints `PASS …`; on failure it dumps the tail of axon's log and
exits non-zero. It tears the containers + test DB down at the end.

Knobs (all optional, via env):

| Var             | Default | Meaning                                      |
| --------------- | ------- | -------------------------------------------- |
| `POSTGRES_PORT` | `5432`  | host port for the compose Postgres           |
| `SYNAPSE_PORT`  | `8008`  | host port for Synapse                        |
| `MESSAGE_COUNT` | `3`     | messages the seeder sends                    |
| `TIMEOUT`       | `90`    | seconds to wait for each DB condition        |
| `KEEP_UP=1`     | off     | leave containers + test DB up for inspection |

> **Why it asserts on 1 UTD even though the seeder sends 3:** axon archives the
> events Simplified Sliding Sync surfaces, which is the _latest_ timeline event
> per room — not the full backlog. The script asserts on the count axon actually
> archived, then proves that exact set re-decrypts, so it's robust regardless.

### In CI

`.github/workflows/integration.yml` runs this same script on pull requests and
pushes to `main`. The GitHub runner already has docker, docker compose, python3,
and curl, and the default ports are free there, so the job is just "checkout →
install Rust → run the script". It's slower than the lint/test lane (it pulls the
Synapse + Postgres images), which is why it's a separate workflow.

## Prerequisites (manual walkthrough)

The rest of this doc is the manual procedure the script automates — useful when
something breaks and you want to poke at it by hand.

- The dev Postgres and Synapse both come from `docker-compose.yml`.
- Synapse is gated behind the `integration` compose profile, so it does **not**
  start with a plain `docker compose up`.
- **Postgres port:** this machine runs a Homebrew Postgres on 5432, so the
  compose Postgres is published on **5433** (`POSTGRES_PORT` in `.env`). All
  `DATABASE_URL`s below use 5433 — adjust if your `.env` differs.

## 1. Start Synapse

```sh
docker compose --profile integration up -d synapse
# wait for healthy:
docker compose --profile integration ps synapse
```

First boot generates the signing key into the `axon-synapse-data` volume and
runs schema migrations; give it ~10s. Confirm Simplified Sliding Sync (MSC4186)
is advertised — axon speaks _only_ this, so it's the thing that must be on:

```sh
curl -fsS http://localhost:8008/_matrix/client/versions | python3 -m json.tool | grep simplified
# -> "org.matrix.simplified_msc3575": true,
```

## 2. Register a test user

Shared-secret registration (open registration is off):

```sh
docker exec matrix-axon-synapse-1 \
  register_new_matrix_user -c /data/homeserver.yaml \
  -u alice -p hunter2secret --no-admin http://localhost:8008
```

## 3. Plaintext smoke test (sync → persist)

This validates the whole real-server loop without E2EE. Seed a room + message
over the client-server API:

```sh
HS=http://localhost:8008
TOKEN=$(curl -fsS -XPOST $HS/_matrix/client/v3/login -H 'Content-Type: application/json' \
  -d '{"type":"m.login.password","identifier":{"type":"m.id.user","user":"alice"},"password":"hunter2secret"}' \
  | python3 -c "import sys,json;print(json.load(sys.stdin)['access_token'])")
ROOM=$(curl -fsS -XPOST $HS/_matrix/client/v3/createRoom -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' -d '{"name":"axon-smoke","preset":"private_chat"}' \
  | python3 -c "import sys,json;print(json.load(sys.stdin)['room_id'])")
curl -fsS -XPUT "$HS/_matrix/client/v3/rooms/$ROOM/send/m.room.message/txn-$(date +%s)" \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"msgtype":"m.text","body":"hello from the smoke test"}'
```

Run axon against the local servers with no account configured (accounts are
runtime-provisioned only, ADR 0024). **Isolate it from `.env`** — `axon-server`
loads `.env` via dotenvy from the working dir upward, and the project `.env` may
point at other settings you don't want here. Running the built binary from a
throwaway dir avoids that:

```sh
cargo build -p axon-server
BIN=$PWD/target/debug/axon
mkdir -p /tmp/axon-smoke && cd /tmp/axon-smoke
DATABASE_URL=postgres://axon:axon@127.0.0.1:5433/axon \
AXON_SERVER__PORT=18080 \
AXON_SYNC__DATA_DIR=/tmp/axon-smoke/axon-data/sync \
AXON_SEARCH__INDEX_PATH=/tmp/axon-smoke/axon-data/search \
AXON_MEDIA__CACHE_DIR=/tmp/axon-smoke/axon-data/media \
AXON_SYNC__STORE_KEY=local-smoke-key \
RUST_LOG=info,axon_sync=debug \
  "$BIN" &
```

In another shell, mint a bearer token and log the account in:

```sh
DATABASE_URL=postgres://axon:axon@127.0.0.1:5433/axon "$BIN" token issue --label smoke
# -> prints the token on its own line; export it
TOKEN=<paste the token>
curl -fsS -XPOST http://127.0.0.1:18080/v1/accounts/login \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"username":"@alice:localhost","homeserver_url":"http://localhost:8008","password":"hunter2secret"}'
```

Watch axon's log for `account logged in and supervised` → `persisted event … event_type="m.room.message"`,
then confirm the row landed:

```sh
docker exec matrix-axon-postgres-1 psql -U axon -d axon -tAc \
  "SELECT event_type, content IS NOT NULL AS decrypted, count(*) FROM events e \
   JOIN accounts a USING(account_id) WHERE a.user_id='@alice:localhost' GROUP BY 1,2;"
# -> m.room.message|t|1
```

## 4. E2EE re-decryption test (the M3c prize), by hand

This is what the automated script does; here's the manual version using the
**seeder** binary as the "device A" that creates encrypted history + a key backup
(axon can't seed that itself — it's the unverified device under test). Set
`SEED_MEDIA_FILE` to also send an encrypted attachment; the automated script
uses this to verify the media proxy after recovery.

Build it and seed against a freshly registered user:

```sh
cargo build -p axon-itest
SEED_HOMESERVER=http://localhost:8008 \
SEED_USER_ID=@alice:localhost \
SEED_PASSWORD=hunter2secret \
SEED_STORE_DIR=$(mktemp -d) \
SEED_MESSAGE_COUNT=3 \
  ./target/debug/seed
# -> {"user_id":"…","device_id":"…","room_id":"!…:localhost",
#     "recovery_key":"EsT… …","messages":["seed message 0", …]}
```

Grab the `recovery_key` and `room_id` from that JSON, then:

1. **Fresh axon device, log in with no recovery key** — run axon as in step 3
   but wipe its pinned SDK/search/media paths first, then log in, so it starts
   unverified and leaves no platform-dir state behind. Historical messages
   persist as UTDs:
   ```sh
   rm -rf /tmp/axon-smoke/axon-data   # force a fresh, isolated device
   # …run axon (same as step 3, no recovery key involved)…
   curl -fsS -XPOST http://127.0.0.1:18080/v1/accounts/login \
     -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
     -d '{"username":"@alice:localhost","homeserver_url":"http://localhost:8008","password":"hunter2secret"}'
   # -> {"data":{"account_id":"…", …}} — save the account_id
   docker exec matrix-axon-postgres-1 psql -U axon -d axon -tAc \
     "SELECT event_type, content IS NOT NULL AS decrypted, megolm_session_id IS NOT NULL AS has_session, count(*) \
      FROM events e JOIN accounts a USING(account_id) WHERE a.user_id='@alice:localhost' GROUP BY 1,2,3;"
   # UTDs show: m.room.encrypted | f | t | N
   ```
2. **POST the recovery key** from the seeder to the same running device — no
   restart needed:
   ```sh
   curl -fsS -XPOST "http://127.0.0.1:18080/v1/accounts/<account_id>/recover" \
     -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
     -d '{"recovery_key":"<recovery_key from the seeder JSON>"}'
   ```
   Watch for `account recovered keys via recovery key`, then `re-decrypted UTD`
   lines, and re-run the query — the `m.room.encrypted / decrypted=f` count falls
   as real types with `decrypted=t` rise.

> The recovery key here is a _fresh, local_ one minted by the seeder on this
> Synapse — not any key you may have used elsewhere.

## Teardown

```sh
docker compose --profile integration down            # stop synapse (keeps data)
docker compose --profile integration down -v synapse # also wipe the volume
```

[Synapse]: https://github.com/element-hq/synapse

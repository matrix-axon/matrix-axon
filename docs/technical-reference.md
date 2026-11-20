# Technical reference

How Axon is put together and the knobs the server exposes: architecture, database behavior, and configuration.
For task-oriented server setup (sign-in, tokens, push, TLS), see [server-admin.md](server-admin.md).

## Architecture overview

```
Homeserver(s)  →  Axon (single binary)  →  axon-web · axon-tui
(Synapse /         sync · crypto · store      + any client built
 Dendrite)         search · media · api         against /v1/ API
```

One Rust binary, one Postgres database, media cached to local disk.
See the [architecture section of the tech spec](mvp/tech-spec.md#architecture-overview) for detail, and the [ADRs](https://github.com/matrix-axon/matrix-axon/tree/main/docs/adr) for the reasoning behind individual decisions.

## Clients

| Client                                                                                   | Platform                                              | Status                                                                   |
| ---------------------------------------------------------------------------------------- | ----------------------------------------------------- | ------------------------------------------------------------------------ |
| [`axon-tui`](../clients/tui/README.md)                                                   | Terminal                                              | Active (MVP reference client)                                            |
| [`axon-web`](https://github.com/matrix-axon/matrix-axon/blob/main/clients/web/README.md) | Web browser (desktop + mobile)                        | Active (nearing MVP)                                                     |
| [`axon-desktop`](../clients/web/src-tauri/README.md)                                     | Linux, macOS, Windows (Tauri shell around `axon-web`) | Active ([download](https://github.com/matrix-axon/matrix-axon/releases)) |
| `axon-mobile`                                                                            | iOS + Android (the same shell)                        | Planned (ADR 0102)                                                       |

See [ADR 0031](adr/0031-client-strategy.md) for the client strategy and sequencing.

### Reaction images

Personal, current-room, and joined canonical ancestor image packs support custom emoji and sticker discovery.
The room `emoji-packs` endpoint exposes cached packs, with `usage=sticker` selecting sticker-capable entries.
The media proxy accepts current pack references before their first use as reactions.
HTML messages also retain the first 64 distinct inline image references in document order, independent of subsequent pack changes.
References come from parsed, sanitized `img` elements in HTML bodies of at most 64 KiB.
Authorization uses the same latest valid edit as the timeline, so stray replacement content, another sender's edits, and superseded edits do not authorize images.
These references authorize only the owning account while the room is not known left or banned, and an authorized redaction in the same room revokes the reference.
Redaction checks use SDK-pruned evidence or cached sender/server and current power-level authority, matching the server's existing state-repair policy.
This fallback does not reconstruct historical authorization: a later promotion can make an earlier pending redaction provable.
Proven redaction authority is persisted atomically with event ingestion in either arrival order, so later power-level changes cannot restore redacted media.
A forward migration preserves already-applied same-room redactions for every stored event type, even when the historical moderator state is no longer available.
Subsequent creation or power-level state writes retry pending authority proofs for that account and room and enqueue search refreshes atomically.
Event and authority-state writes share a transaction-scoped account/room lock, with the configured SQL lock/statement deadlines, so concurrent arrival cannot lose the proof.
The inline authorization read has a ten-second overall deadline, including pool acquisition, in addition to configured database deadlines.
Inline images use the same raster MIME restrictions as pack previews.
Historical references are repaired in the background in committed batches of at most 32 messages, using the isolated maintenance connection and a ten-second batch deadline.
The worker waits one second between batches, retries failures, and resumes pending rows after restart; identical re-deliveries can also heal pending references.
The pacing ceiling is about 32 messages per second before query and parsing time: one million pending messages therefore take at least 8.7 hours, and larger or busy databases take longer.
Until a historical row is repaired, its previous media authorization behavior applies.
Historical repair bounds each loaded content object to 128 KiB; oversized objects receive an empty projection.
Clients may send an optional shortcode with a custom reaction, or send an existing pack image through `send-sticker` as a standalone `m.sticker` event.
See [Custom emoji discovery](custom-emoji.md) for limits, sync behavior, inheritance, sending, and diagnostics.

The authenticated media download and thumbnail endpoints also resolve custom reaction images whose keys are `mxc://` URLs (MSC4027), without requiring an image pack definition.
Known left or banned rooms cannot authorize reaction image downloads.
Reaction media references must be structurally valid MXC URIs of at most 2,048 bytes; malformed and oversized keys do not authorize a fetch.
Reaction tallies also expose an optional `shortcode` from the original reaction content (`shortcode`, then `com.beeper.reaction.shortcode`).
Only custom MXC reactions with nonblank names of at most 100 UTF-8 bytes qualify.
C0 and C1 control characters are rejected, and surrounding whitespace is trimmed with an explicit ECMAScript-compatible character set; these rules do not depend on the database locale.
The oldest valid name by `(origin_ts, event_id)` among the existing capped, deduplicated reaction representatives wins; redaction removes that representative from consideration.
Each sender/key pair keeps its smallest-event-ID representative; a name on another duplicate is ignored until that representative is removed.
Names are omitted when unavailable, so clients should retain a generic fallback and render supplied names as untrusted text.
Safe PNG, JPEG, GIF, and WebP signatures determine reaction image response types; unrecognized objects remain downloads with `application/octet-stream` and `nosniff`.
Media is fetched on client request and uses the existing per-object limit and globally bounded LRU cache.
The reaction index is built transactionally during startup, before this instance starts sync or serves requests; plan a maintenance restart for a large event store.
Displaying these images in Axon's reference clients is separate client work.

## Database encoding

Axon's database must use the `UTF8` encoding, and the server refuses to start on any other.
The Docker Compose stack, the Debian and RPM packages, and the Homebrew caveats all create a `UTF8` database.
A plain `createdb` copies the encoding of the cluster, and `initdb` under a `C` or `POSIX` locale gives `SQL_ASCII`, which Axon cannot use.
Check with `psql -d axon -c 'SHOW server_encoding'`.

To move an existing database, stop `axon-server`, copy the data into a new `UTF8` database, and swap it in under the old name, running each command as a PostgreSQL superuser:

```sh
createdb -E UTF8 -T template0 --locale=C -O axon axon_utf8
bash -o pipefail -c 'pg_dump --encoding=UTF8 axon | psql -q -v ON_ERROR_STOP=1 -d axon_utf8' &&
  psql -c 'ALTER DATABASE axon RENAME TO axon_old; ALTER DATABASE axon_utf8 RENAME TO axon'
```

Start the server, and once it works, `dropdb axon_old`.
The rename runs only if the copy succeeded.
`--encoding=UTF8` makes PostgreSQL check every byte as it dumps; a dump in the old encoding would load bytes that are not UTF-8 into the new database unchecked.
If the copy reports an invalid byte sequence, drop `axon_utf8` and open an issue, since Axon itself only writes UTF-8.
The pipe writes no dump file, which would hold your decrypted message history.
The `C` locale accepts `UTF8` on any cluster, and Axon's queries do not depend on the locale.
The copy carries everything inside the database, but not what is set on the database itself: `ALTER DATABASE axon SET` and `ALTER ROLE ... IN DATABASE axon SET` parameters, grants on the database, and its comment.
None of the install methods set these; if you added any by hand, `psql -c '\drds'` and `psql -c '\l+ axon_old'` list them after the swap, to re-apply on `axon`.

## Database deadlines

Every server database connection has PostgreSQL statement and lock deadlines, including connections replaced after a request is canceled.
Ordinary statements default to 10 seconds, lock waits to 3 seconds, and pool acquisition to 5 seconds.
Hot timeline, media, and relation reads use a separate pool with `max_connections` slots.
Status and search indexing each reserve another connection, so a slow status query or bulk maintenance cannot block those reads or live indexing.
Status uses exact event totals maintained by database triggers instead of counting the event table on each request.
If its progress query fails, status still returns build, sync, management, and disk information, with `backfill.progress_available=false`.
Bulk maintenance shares another connection with a 120-second statement deadline; startup migrations and developer database repair use a 600-second statement and lock deadline.
Idle transactions are terminated after 30 seconds so an abandoned transaction cannot hold locks indefinitely.
These are per-statement deadlines, not a deadline for an entire multi-step operation.
A timed-out SQL statement fails and rolls back its writes; a canceled HTTP response does not prove that a write failed to commit.

Upgrades apply schema migrations at startup, before the server accepts requests.
A migration that adds a column or index to the `events` table holds it locked until the migration commits.
The inline-media upgrade builds both its pending-row and GIN indexes non-concurrently and seeds historical redaction proofs before accepting traffic.
The historical proof seed and its search-outbox fanout run as one unbatched startup statement under `migration_statement_secs`, just like the index builds.
A timeout rolls back that migration and prevents startup; increase the deadline and restart to retry.
Plan a maintenance window for large event stores; this startup work is separate from the paced inline-media repair afterward.
After startup, the search worker drains the potentially large new outbox backlog in its normal bounded batches.
The thread-summary index (#763) took 96 seconds on a 2.7 GB `events` table, of which the backfill and the index build are separate statements under the 600-second deadline.
On a much larger database, raise `migration_statement_secs` for the upgrade if a statement times out; the failed migration rolls back and runs again at the next start.
A migration that rewrites many rows, as that one does, leaves index-only scans reading the table itself until autovacuum next processes it.
To get the faster plans at once, run `VACUUM (ANALYZE) events;` after the upgrade, or `VACUUM (PARALLEL 0, ANALYZE) events;` in a container whose `/dev/shm` is small (Docker's default is 64 MB).

Configure these values under `[database.timeouts]` in [axon.toml.example](../axon.toml.example), or through overrides such as `AXON_DATABASE__TIMEOUTS__STATEMENT_SECS=20` and `AXON_DATABASE__TIMEOUTS__MAINTENANCE_STATEMENT_SECS=240`.
All deadlines must be between 1 and 86400 seconds; zero is rejected.
The statement deadlines must satisfy `statement_secs <= maintenance_statement_secs <= migration_statement_secs`.
Changes take effect after restarting the server.
On a slow machine, increase the relevant statement deadline while keeping lock and acquisition waits short enough to surface contention promptly.
The database must allow `2 * max_connections + 3` server connections, plus connections from CLI commands and other database users.
Short-lived token/OAuth commands verify one ordinary connection and open other pools only if needed; the server still verifies its entire budget before serving.

Account deletion and room purge commit event deletions in batches of at most 1000 rows, each with atomic search-cleanup obligations.
An interrupted account teardown retains its existing `deleting` breadcrumb for boot reconciliation; purge-on-leave queues a durable event watermark and wakes the supervised cleanup worker immediately, with retries every 30 seconds and after restart, without delaying sync on bulk deletion.
Wakeups schedule a bounded page of pending rooms; a backlog or slow maintenance operation can delay a particular room.
When purge-on-leave is enabled, ingestion skips events from rooms the SDK has marked left or banned, including the departure batch, so late callbacks cannot recreate purged data.
Each cleanup batch that deletes history also resets the history-backfill cursor atomically, allowing deleted history to be fetched again even after a rejoin.
Retries that delete no history preserve the current cursor.
Local leave state and its purge intent commit together, so an enqueue failure cannot leave a successfully saved leave without its cleanup obligation.
A later leave advances a pending watermark; retrying an earlier leave keeps its captured watermark so later rejoin events survive.
Retry pages rotate past persistent failures so the first 100 failing rooms cannot starve all later rooms.
Room cleanup removes only events captured by that watermark and enqueues per-event search removals, preserving later messages and rejoin metadata.
Room-summary rebuild remains transactional on the maintenance pool; account/session-wide pending-UTD scans use its longer deadline too.
Sync ingestion writes one event per statement; unread projections operate on one row per room; live search drains use indexed, limited outbox batches with ordinary deadlines.
Search-index seeding and outbox pruning use their own connection and the longer maintenance deadline.
Search-outbox pruning commits batches of at most 1000 rows so interruptions leave a safely retryable prefix.
Stale refresh-token cleanup runs in the background at most once per minute, with one sweep in flight across the server's store handles; cleanup failure does not reject token issuance or rotation.
Refresh tokens beyond the 30-day retention horizon are treated as unknown even if physical cleanup is delayed.
Session settings are initialized after connection, supporting direct PostgreSQL and session-mode poolers without a startup `options` parameter.
For PgBouncer, retain sqlx’s existing `ignore_startup_parameters = extra_float_digits` setting; ignoring `options` is unnecessary.
Transaction-mode poolers are unsupported: session deadlines and session advisory locks require a stable backend connection, as described in [PgBouncer’s feature matrix](https://www.pgbouncer.org/features.html).
See [ADR 0112](adr/0112-bounded-database-work-and-pool-isolation.md) for the pool topology, transaction audit, and remaining maintenance tradeoffs.
Query and pool timeout failures use existing API error responses and request-error logging; cleanup failures log a redacted diagnostic category and retry on later activity.

## Homeserver load gate

Axon's background requests to a homeserver pass a load gate first ([ADR 0122](adr/0122-homeserver-load-gate.md)): history backfill pages, upstream room probes, key-backup downloads, DM auto-joins, and the supplementary state long-poll that carries image packs, guest access, and server ACL.
Each homeserver origin has one gate, shared by every account on the server.
It admits two background requests at once to start, grows to four after fast successes, and halves on a timeout, 5xx, 429 or transport error.
A timed-out request keeps its slot for 15 minutes, because the homeserver may still be working on it.
At a limit of zero a circuit breaker stops all background requests, then sends one probe after a cooldown of 1 to 30 minutes, once no held slots remain.
Requests a user is waiting on, such as sends, joins, recovery and manual re-decryption, never wait on the gate.
When one of them times out or fails with a 5xx, 429 or transport error, it halves the limit too, but never below one, so a struggling homeserver slows background work before it slows users.
An upload's timeout is the exception: it doesn't count, because it depends on the file's size and the server's own uplink.
`GET /v1/status` lists each gate under `homeservers`: its limit, active and held slots, waiting requests, and breaker state.
The gate's state is in memory and starts fresh on restart.
Its constants are not configurable.

## Holes left by limited syncs

Sliding sync delivers at most `sync.timeline_limit` (default 20) new events per room in each response.
When more than that reach a room between two responses, the homeserver sends only the newest ones and marks the room `limited`.
Axon records every such hole and fetches the missing events from `/messages` ([ADR 0140](adr/0140-fill-limited-sync-gaps.md)).
A busy room while sync is slow, a restart after an outage, or a bulk import can each leave one.

- The live ingestion handler writes one `room_timeline_gaps` row per hole, from the SDK's room-update broadcast, logged at `INFO` as "limited sync left a hole in the room's history".
  The row commits before the response's events are stored and before the SDK saves its sync position, so a restart cannot skip it.
  An insert the database refuses is retried for up to 30 seconds while sync waits on it, then from memory with the same room, token and batch; until it commits, a restart loses that hole, which is logged at `ERROR`.
  A room's first response is also `limited`, but nothing was stored before it, so that row is dropped unpaged (a `DEBUG` line): history before a room's first window is backfill's job.
- The backfill task pages each row backward, ahead of ordinary backfill pages and through the same load gate, disk guard and throttle.
  A row closes on the first page that holds an event Axon stored before the hole was detected, and logs at `INFO` with its `pages_fetched` and `events_recovered`.
  A row that recovered nothing logs "limited sync had nothing missing" instead: a room coming back into the sliding-sync window can be reported `limited` with no hole.
  Failures back off and park as backfill rooms do (ADR 0121); a rejected token restarts the row from the room's live end.
  A page with an event the store could not write keeps its token and backs off the same way, for gap rows and ordinary backfill alike, so the event is fetched again rather than skipped.
  So does a gap row whose page lookup or update the store failed.
  The one exception is an event whose own values the database rejects (SQLSTATE class 22, 23 or 54), such as JSON with a `\u0000` escape that Postgres `jsonb` refuses.
  It would fail on every attempt, so it is logged at `ERROR` ("the store rejects this event's values; skipping it") and skipped, and cannot hold a room's cursor for good.
  Any other error, including one that would refuse every write (a broken connection setup, a schema mismatch), holds the page, so it can't skip events wholesale.
- The task runs even when `sync.backfill_enabled = false`; it then pages only these rows and never extends history.
  `sync.backfill_target_depth` does not apply to them.
- If the broadcast lags, the watcher cannot tell which lost responses were limited, so it probes every joined room that stored events during the lag (a `WARN` line with the boundary and the number of rooms).
- `GET /v1/status` reports `history_gaps`: `open` rows, of which `parked` and `awaiting_batch` (waiting for the limited batch to be stored), and `events_recovered` since the server started.
- Rows go with their account, and with their room when purge-on-leave removes it.

`sync.timeline_limit` must be at least 1; startup rejects 0.

Holes that existed before this release have no rows, so a one-time verification pass finds them (ADR 0140 decision 4):

- After the upgrade, the migration queues one `room_history_verify` row for every room with stored events, and the pass starts on its own.
  It logs "verifying the stored history of joined rooms" at `INFO` when it starts and "every joined room's stored history has been verified" when it finishes.
- The backfill task walks each joined room from its live end, most recently active room first, and stores every page; events already stored are no-ops.
  It alternates one verification page with one ordinary backfill page, and takes every page while ordinary backfill has none.
  A room with an open gap row waits for it, since that row is filling the same newest history.
- A walk closes on the page that holds the room's lowest-`id` event, the first one Axon stored.
  If the homeserver no longer serves that event (it purged the room), the walk closes after 500 events in a row that were neither stored nor newer than the room's first stored events ("passed below the room's stored history", `INFO`).
  Otherwise it ends at the room's start, logged at `WARN`.
  A walk whose pages time out three times in all is closed as `timed_out`, also at `WARN`.
  The homeserver is then most likely fetching the room's history over federation for every page (a room it purged, or one under a retention policy).
  What the walk recovered is kept, and the management action can queue it again.
- A walk that stored missing events logs "stored history that was missing from the room" at `WARN`, once per room, with `events_recovered`.
  Each page logs at `DEBUG` with `inserted` and `already_stored`.
- Rooms the account is not in wait, and are walked if it rejoins them before purge-on-leave removes them.
  Failures back off and park as backfill rooms do (ADR 0121).
  Ten empty pages in a row count as a failure, so a homeserver that keeps answering with empty pages parks the room.
  A walk restarted while it was in flight (by the management action, an expired gap row, or a purge) stops and is read again: every write names the walk's generation.
- `GET /v1/status` reports `history_verify`: `rooms_pending` (joined rooms not yet verified), of which `rooms_held` (waiting on a gap row) and `rooms_parked` (failing, retried about once a day), then `rooms_not_joined`, `rooms_complete` (of which `rooms_timed_out`), and `events_recovered`.
  Rooms the account left are recorded as `rooms_not_joined` only from five minutes after startup, once the room list has loaded.
- `POST /v1/management/accounts/{account_id}/history-verify` queues the pass again, for one room (`{"room_id": "…"}`) or every room of the account (`{}`).
- A limited row dropped unpaged because none of its batch was stored within 10 minutes queues its room's walk again, which finds the hole from the live end.
- `sync.history_verify_enabled = false` turns the pass off, for an operator who must avoid its one-off load on the homeserver.
  It runs whatever `sync.backfill_enabled` says, and `sync.backfill_target_depth` does not apply to it.
- Rows go with their account, and with their room when purge-on-leave removes it.
  A rejoined room the purge keeps restarts an unfinished walk.

`scripts/gap-fill-test.sh` exercises the whole path against a local Synapse ([integration testing](integration-testing.md#gap-fill-adr-0140)).

## Media streaming

Video and audio (`video/*` and `audio/*` by the event's `info.mimetype`) stream from the homeserver as they arrive ([ADR 0139](adr/0139-stream-large-video-and-audio.md)).
The first bytes leave Axon as soon as the first chunk arrives, encrypted attachments are decrypted chunk by chunk, and the object is cached once it has been verified.
Images, files and thumbnails keep the buffered path, which checks an encrypted attachment's hash before serving any byte.

A streamed object's SHA-256 can only be checked after its last byte, so a response withholds the object's last 64 KiB until then and is aborted, not finished, if the check fails.
A failed check is remembered for 10 minutes, so a retry gets `422 media_undecryptable` at once.

| Setting (`[media]`)      | Default | Meaning                                                                                                                          |
| ------------------------ | ------- | -------------------------------------------------------------------------------------------------------------------------------- |
| `fetch_timeout_secs`     | 60      | Deadline for a stream slot plus the homeserver's response headers; past it, a retryable `503`. Bounds a whole buffered download. |
| `stall_timeout_secs`     | 30      | Cancels a streamed download that delivers no bytes for this long. A stream has no total-duration limit.                          |
| `max_concurrent_streams` | 4       | Streamed downloads open to the homeserver at once, separate from `max_concurrent_downloads` for buffered media.                  |
| `max_stream_bytes`       | 2 GiB   | Largest streamed object (`413` above it). Must not exceed `max_bytes - max_object_bytes`; unset, it is lowered to that bound.    |

A streamed download reserves its declared length in the cache before reading its body, and runs to completion even if the viewer goes away.
When the other streams' reservations leave no room, the request gets a retryable `503` rather than waiting.
A `503` for a stream that could not start, for want of a slot, an upstream answer or room, carries `Retry-After: 5`.

Two configurations change this:

- **A small cache.** With `max_stream_bytes` unset and a cache smaller than about twice `max_object_bytes`, the stream limit would fall below `max_object_bytes`. Streaming would then refuse objects the buffered path serves, so video and audio stay on the buffered path and startup logs a warning. An explicit `max_stream_bytes` always streams.
- **A disabled cache** (`enabled = false`). Video and audio still stream, and requests made while a download runs share it, but nothing is kept afterwards. Each later request downloads the object again, as a buffered object does with the cache disabled.
  `max_bytes` still bounds the staging files that streams in flight hold, so the `max_stream_bytes` check and the small-cache fallback above apply as they do with the cache on.

Clients keep their own limits.
The web client plays video and audio from a stream ticket (below), so playback starts before the download finishes and the client sets no size limit of its own.
In the macOS, Windows and iOS shells the media element loads the ticket through the shell's `axon-media` scheme, which forwards it to the ticket route ([ADR 0139](adr/0139-stream-large-video-and-audio.md) section 7).
The Linux and Android shells' players cannot load it, so there video and audio still buffer a whole object, up to 100 MiB ([issue 1192](https://github.com/matrix-axon/matrix-axon/issues/1192), [issue 1194](https://github.com/matrix-axon/matrix-axon/issues/1194)).
The scheme returns at most 4 MiB per response, and reads the object in 4-MiB-aligned blocks of up to 4 MiB, held in memory only (at most 32 MiB), so a player that reads one sample at a time costs the server one request per block rather than per sample.
Once a read is served from a complete block, the next block is fetched ahead of the player.
The TUI still refuses media over 20 MiB.
A video or audio event whose `info.mimetype` is missing, generic (`application/octet-stream`), has parameters or does not start with a lowercase `audio/` or `video/` is served as `application/octet-stream`, which no ticket covers, so every client buffers it as before ([issue 1182](https://github.com/matrix-axon/matrix-axon/issues/1182)).

### Stream tickets

A `<video>` or `<audio>` element cannot send a bearer token, so a client asks for a stream ticket (`POST /v1/media/{account_id}/{server_name}/{media_id}/stream-ticket`) and loads the path it returns, `/v1/media-stream/<ticket>`, without one ([ADR 0139](adr/0139-stream-large-video-and-audio.md) section 5).
The ticket is a credential for that one object.
It stops working 30 minutes after its last use, 12 hours after it was issued, when the token that minted it is revoked or expires, when the owning event is redacted, or when the server restarts.
A use is a request served without error; a request that fails does not keep the ticket alive.
These checks run when each request starts, so a response already streaming when the token is revoked runs to its end, as a bearer-authenticated media response does.
Each credential session holds at most 32 tickets.
Issuing a ticket logs at `INFO` with the account, MXC URI, token id and session id, and a refused ticket logs at `DEBUG`; neither records the ticket.

Axon's own request log records `/v1/media-stream/{ticket}` in place of the ticket.
**A reverse proxy in front of Axon must do the same in its access log**, or every media range request writes a usable ticket to disk:

- The packaged nginx and Apache sites (`packaging/proxy/`) already do. An nginx or Apache site written before this release, or any other proxy configuration, needs the same `log_format` or `LogFormat` change; copy it from those files.
- Caddy keeps no access log unless the site has a `log` block. The packaged Caddy site shows a `log` block that filters the ticket out of `request>uri`; use the same filter in any site that logs.
- Any other proxy: rewrite or drop the request path for `/v1/media-stream/` in its access log.

A proxy's error log can still contain a ticket, in the request line of a proxied request that failed. Keep error logs private.

## Environment variables

Two kinds of `AXON_`-prefixed environment variables exist:

**Standalone vars** — not tied to any config field, used by the CLI directly:

| Variable        | Used by                                    | Meaning                                                                                                                                                                                     |
| --------------- | ------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `AXON_CONFIG`   | server, all CLI subcommands                | Path to the server config, when not passed via `--config`. Falls back to `./axon.toml`, then `~/.config/axon-server/config.toml` (with a read-only fallback to `~/.config/axon/axon.toml`). |
| `DATABASE_URL`  | server, all CLI subcommands                | Postgres connection string. Also settable as `AXON_DATABASE__URL` or `[database].url`.                                                                                                      |
| `AXON_BASE_URL` | `axon utd redecrypt` (HTTP CLI calls), TUI | Base URL of the running axon-server to call. **Defaults to `http://127.0.0.1:8080` — set explicitly for any non-local server.**                                                             |
| `AXON_TOKEN`    | `axon utd redecrypt`, TUI                  | Bearer token sent with the request, in place of `--token` (web does not consume token from env)                                                                                             |
| `RUST_LOG`      | server                                     | Overrides `log.level` / `AXON_LOG__LEVEL` with a raw `tracing` filter directive.                                                                                                            |

On Unix, config discovery ends with `/etc/axon-server/config.toml`, where the `.deb` and `.rpm` put the service's config, so `sudo -u axon axon-server token issue --label laptop` on a packaged install needs no `--config`.
A user's own file still wins over it.
When that file exists and the invoking user cannot read it, a command with no other configuration (no file, no `--config`, no `DATABASE_URL` or `AXON_*` variables) fails and says to run as the service's user; environment-only configuration and `axon-server init` are unaffected.

TUI-specific display vars (`AXON_FONT_SIZE`, `AXON_IMAGE_PROTOCOL`, `AXON_NO_IMAGE_QUERY`) are documented in [`clients/tui/README.md`](../clients/tui/README.md).

**Structured config overrides** — every field in `axon.toml` can also be set via `AXON_<SECTION>__<FIELD>` (double underscore between nesting levels), e.g. `AXON_SERVER__PORT=9090` or `AXON_MEDIA__MAX_BYTES=1048576`.
Precedence (lowest to highest): built-in defaults < `axon.toml` < bare `DATABASE_URL` < `AXON_`-prefixed vars.

[`axon.toml.example`](../axon.toml.example) is the full reference for every section and field, with the corresponding env var noted alongside each one.
[`.env.example`](https://github.com/matrix-axon/matrix-axon/blob/main/.env.example) mirrors the same fields in env-var form for anyone who prefers configuring entirely through the environment.

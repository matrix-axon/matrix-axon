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
See the [architecture section of the tech spec](mvp/tech-spec.md#architecture-overview) for detail, and the [ADRs](adr/) for the reasoning behind individual decisions.

### Reaction images

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
[`.env.example`](../.env.example) mirrors the same fields in env-var form for anyone who prefers configuring entirely through the environment.

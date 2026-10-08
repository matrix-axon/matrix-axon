# Room metadata

[ADR 0111](adr/0111-cached-progressive-room-metadata.md) records the architecture for cached room metadata and bounded progressive enrichment.
This guide documents the implemented API, current sync coverage, verification, and remaining work.
[Tracking issue 618](https://github.com/matrix-axon/matrix-axon/issues/618) records the remaining acquisition, discovery, and client work.

## Cached state details

`GET /v1/accounts/{account_id}/rooms/{room_id}/metadata` returns eight typed snapshots from that account's cached room state.
It requires the same bearer authentication as other `/v1/` reads.
It performs one database query for a fixed set of singleton tuples, with no homeserver request, membership aggregation, or discovery-cache merge.
Each state's PostgreSQL-rendered content is limited to 128 KiB before transfer and JSON decoding.
This is a local transfer budget with headroom for JSONB spacing above Matrix's compact event-size limit; it is not an upstream event-validity check.
PostgreSQL still detoasts and renders the full selected value before measuring it; this cap does not bound database-side rendering work.
[Issue 631](https://github.com/matrix-axon/matrix-axon/issues/631) tracks that remaining resource bound.
An oversized state keeps its provenance but withholds its content; other fields remain available.

| Snapshot             | State type                  | Detail fields                                                                            |
| -------------------- | --------------------------- | ---------------------------------------------------------------------------------------- |
| `aliases`            | `m.room.canonical_alias`    | `alias`, `alt_aliases`                                                                   |
| `creation`           | `m.room.create`             | `room_version`, `creator`, `additional_creators`, `federate`, `room_type`, `predecessor` |
| `join_rules`         | `m.room.join_rules`         | `join_rule`, `allow` conditions                                                          |
| `encryption`         | `m.room.encryption`         | `algorithm`, `rotation_period_ms`, `rotation_period_msgs`                                |
| `power_levels`       | `m.room.power_levels`       | Role thresholds, `users`, `events`, `notifications.room`                                 |
| `server_acl`         | `m.room.server_acl`         | `allow_ip_literals`, `allow`, `deny`                                                     |
| `history_visibility` | `m.room.history_visibility` | `history_visibility`                                                                     |
| `guest_access`       | `m.room.guest_access`       | `guest_access`                                                                           |

Creation content maps Matrix `m.federate` to API `federate` and Matrix `type` to API `room_type`.
These names deliberately differ from the stored Matrix keys.

Each snapshot includes `status`, `event_id`, `sender`, `origin_ts`, `redacted`, `redaction_event_id`, and `content`.
`origin_ts` is the upstream event timestamp, not the time of the last successful sync or an access-freshness guarantee.

| Status        | Meaning                                                                                              |
| ------------- | ---------------------------------------------------------------------------------------------------- |
| `unknown`     | No cached tuple; Axon cannot establish that the setting is unset. Provenance and content are `null`. |
| `available`   | Typed content is available, including explicit empty lists and omitted fields.                       |
| `unavailable` | A tuple exists with SQL NULL or JSON null content; neither establishes redaction or removal.         |
| `partial`     | Valid fields and entries remain available; `invalid_fields` identifies withheld malformed data.      |
| `invalid`     | Stored content has an incompatible shape; content is withheld.                                       |
| `too_large`   | Stored content exceeded the content-size bound; content is withheld.                                 |

Only `available` and `partial` snapshots have non-null content.
A `partial` snapshot preserves valid fields and entries and reports malformed paths in `invalid_fields` (for example, `rotation_period_msgs`, `allow[]`, or `users.*`).
Paths use wildcards instead of upstream map keys, are deduplicated, and never contain offending values.
Clients must not treat a malformed field or a filtered list/map as confirmed absent or complete, or apply defaults to an invalid field.
All other snapshots have an empty `invalid_fields` list unless typed decoding still fails after field validation.
Nullable content fields preserve omission instead of filling Matrix defaults.
In particular, an unknown encryption snapshot does not mean "unencrypted," and an unknown alias snapshot does not mean "no aliases."
For available creation state, the enclosing `sender` provides the create-event sender when a room version omits the `creator` content field.
A creation predecessor can omit `event_id`; its `room_id` remains available.
Power levels describe configured state, with legacy decimal strings and floats normalized to integers (floats truncate toward zero); they are not resolved permissions and must not drive authorization decisions.
Legacy normalization currently does not consult the room version; [issue 632](https://github.com/matrix-axon/matrix-axon/issues/632) tracks version-aware validation or diagnostics.
Unknown condition types retain their `type` and optional `room_id`; extension-specific payloads are not exposed by this typed read.
Redaction evidence is independent of content availability, consistent with [Matrix redaction semantics](https://spec.matrix.org/v1.19/client-server-api/#redactions).
`redacted: true` means the SDK supplied a redacted state form.
`redacted: null` means Axon has no positive redaction evidence, including legacy rows and unknown tuples.
Absence of the SDK marker does not prove that content is original rather than server-pruned or censored, so Axon does not emit `false`.
`redaction_event_id` identifies a known redaction when available; a null ID does not negate positive evidence.
An empty object or NULL content alone never establishes redaction.
Already-redacted state can retain room-version-protected fields and remain `available` or `partial`.
Axon uses the SDK's retained content instead of implementing its own pruning rules.
A raw timeline redaction row alone never changes the metadata's evidence or withholds its content.
This also avoids withholding legitimate protected fields in pre-migration rows that already contain a redacted form.

A bounded worker reconciles SDK-redacted singleton state into the shared `room_state` projection.
It covers the eight metadata types plus name, topic, avatar, and tombstone, so `/metadata`, `/info`, room-list summaries, and space display enrichment use the same retained state.
It does not enumerate membership or space-link state keys.
Live redactions enqueue a hint only if the target event ID matches a current metadata/display singleton in the same account and room.
The filter uses fixed primary-key probes, with a two-second deadline; failure conservatively queues a hint, without treating the timeline row as evidence.
Marker-free singleton state writes check only their own SDK cache entry and enqueue a hint if that same event is already redacted there.
Ordinary message redactions and normal initial hydration therefore do not enqueue full room scans.
The state-write check closes redaction-before-state delivery races when the target filter saw no tuple yet.
These checks wait for bounded local I/O, never for the subsequent repair.
Each account has one paced worker, a 32-entry hint queue, and at most eight active rooms per tick: four hints and four rooms from a keyset-paged room-summary sweep.
Duplicate hints coalesce; draining stops at four distinct rooms, leaving the channel tail for the next tick.
Queue overflow and interrupted jobs heal through subsequent sweeps.
The worker reads twelve singleton SDK cache entries per room, bounds raw JSON before decoding to 128 KiB, and uses a two-second deadline for each room and each sweep-page read.
Ticks run once per second with missed ticks skipped; after a complete sweep the next sweep waits five minutes.
That idle interval reduces repeated background SQLite reads; retained non-joined rooms can still occupy cheap sweep-page slots, but the worker checks SDK membership before reading singleton state.
All acquisition is local SDK cache I/O; this worker makes no homeserver requests.
One room or field failing is logged and skipped without stopping sync.
The worker is canceled and joined with its account run.

Repair updates only an existing tuple whose event ID still matches the SDK form.
It cannot overwrite a replacement, advance state from historical data, or recreate a purged room or removed account.
A repaired display tuple and its room-summary projection commit in the same transaction.
Same-event replays cannot clear positive evidence or restore original content; a new replacement event has independent evidence.
Startup sweeps repair a crash between the SDK cache commit and Axon's projection update without a cold resync or re-dispatched state event.
The nullable evidence columns are added by a forward-only migration; older rows are not guessed or rewritten in a bulk backfill.

A client should re-read when reopening the panel, on reconnect, after relevant timeline state events, and after timeline redactions.
Redaction repair is asynchronous, so the first read after a redaction frame may precede reconciliation; bounded visible-panel polling also covers that interval.
Required-state-only updates are persisted without a live invalidation frame, so timeline events and reconnect alone cannot keep an open panel fresh.
Until that gap is closed, clients displaying this endpoint need bounded polling while the panel is visible (for example, one request every 30 seconds, canceled when hidden, with one request in flight and backoff after failures).
This endpoint does not add a metadata-specific live frame.
State invalidation covering required-state updates is a prerequisite for replacing that polling, tracked with the acquisition and client follow-ups below.
Unknown account/room IDs return unknown snapshots with HTTP 200, matching existing cached state-read conventions.
Cached state from a room the account has left remains historical cached state according to the existing retention policy; this endpoint does not assert current membership or upstream access.

## Joined-room member counts

`GET /v1/accounts/{account_id}/rooms/{room_id}/info` adds nullable `member_counts`.
An observed summary contains `joined`, `invited`, and `observed_at` (Unix milliseconds).
Counts come from an atomic SDK `RoomInfo` summary snapshot, never from the lazily loaded member-list projection.
The SDK's normalized invited count includes zero; an uninitialized zero joined count is withheld because a joined room must include the account itself.
A missing SDK room, non-joined membership, uninitialized summary, or count outside signed 64-bit storage range produces `null`, not a guessed count.
Existing rows start unknown; there is no membership backfill or new room-list aggregation.

`observed_at` is when Axon read its local SDK cache, not when the homeserver last confirmed membership.
A recent observation can still reflect a disconnected SDK's old summary.
Consumers must consider account sync health as well as the timestamp; this endpoint does not assert upstream freshness.
Counts persist across process restarts and remain readable while sync is disconnected.
A local leave atomically clears the observation; a rejoin needs another joined SDK observation.
A confirmed upstream `gone` verdict also withholds counts.
Retained room state after leave remains historical, while this count field becomes unknown.

Each account has one cancelable worker subscribed before startup reconciliation.
One-second ticks skip missed ticks and process at most four distinct live hints plus four keyset-paged room-summary rows.
At most 32 SDK notifications are consumed per tick, coalescing duplicates within that budget and preserving the receiver tail.
Lagged notifications wake a sweep without rewinding its cursor, so continuous traffic cannot starve later rooms.
Each page and each room observation has a two-second deadline, and every room failure is logged and skipped independently.
Reconciliation transactions also set PostgreSQL-local statement and lock deadlines of 1.5 seconds and one second, so timed-out awaits cannot leave unbounded database work.
After a complete sweep, another begins after five minutes; startup begins immediately.
An offline-to-online transition sends a coalesced wake signal to resume scanning at the next paced tick, including reconnects that do not restart the account task.
Recovery time scales with the number of cached summaries and local I/O, rather than promising a fixed five-minute completion for every account.
There is no account-sized in-memory dedup map, unbounded room enumeration, or remote request.
The worker only updates existing account/room summary rows; purge or account removal cannot be undone by a late completion.
Its cancellation token and join handle are owned by the account run.
The keyset traversal is shared with singleton-state redaction repair.
Existing tracing controls suffice; warnings identify account, room where applicable, and the SDK-summary source without logging private metadata bodies.

Counts have no new live frame.
Clients can re-read `/info` on reconnect and use bounded visible-panel polling, as for cached state details.
Server counts complete [issue 620](https://github.com/matrix-axon/matrix-axon/issues/620); web and TUI presentation remain separate follow-ups.

## Alias scope

Advertised aliases are the canonical `alias` and `alt_aliases` in `m.room.canonical_alias`.
These can be read locally when that state tuple is cached.
They are not a complete federation-wide alias enumeration.
The separate Matrix `/rooms/{roomId}/aliases` endpoint lists aliases maintained by the account's homeserver and requires its own permission-aware acquisition and cache.
Client follow-ups should use "Advertised aliases" and "Aliases on your homeserver" rather than "Full alias list."
See the [Matrix alias API](https://spec.matrix.org/v1.19/client-server-api/#get_matrixclientv3roomsroomidaliases).

## Sync coverage and remaining acquisition

Axon uses matrix-sdk-ui 0.19.1's `SyncService` room-list required-state defaults.
Those request canonical-alias, creation, join-rule, encryption, power-level, and history-visibility state, including their full content.
The defaults do not request guest-access or server-ACL state; those snapshots can remain unknown until an event is observed.
Even a requested state type can be unknown before room hydration or if no tuple was supplied.
The API deliberately does not hide these gaps with default values or perform a remote fetch on every detail read.
[Issue 621](https://github.com/matrix-axon/matrix-axon/issues/621) tracks bounded missing-state acquisition and homeserver-only metadata.

Authoritative member counts are cached separately as described above; the lazily loaded member list is not an authoritative count.
Paginated unjoined-room discovery and summary enrichment are [issue 622](https://github.com/matrix-axon/matrix-axon/issues/622).
The discovery cache, progressive updates, and acquisition budgets follow ADR 0111 and are not implemented by the cached state endpoint.
Web and TUI consumption are [issue 623](https://github.com/matrix-axon/matrix-axon/issues/623) and [issue 624](https://github.com/matrix-axon/matrix-axon/issues/624).

## Verification

The HTTP integration tests seed real PostgreSQL state and exercise the authenticated router without an upstream service.
The regression suite covers already-redacted state, filtered redaction hints, queued bursts spanning multiple ticks, stale SDK event IDs, raw redaction rows without applied evidence, both log/state arrival orders, account/room isolation, replacement events, ordered and concurrent replay protection, shared display projection repair, and evidence persistence after reopening the store.
A PostgreSQL-gated SDK-cache test applies a redaction in the real SDK state store, invokes the actual reconciliation consumer without re-dispatching a state event, and verifies retained fields and purge safety.
Use a throwaway database; these tests run migrations and write fixture accounts.

```sh
DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon_test cargo test -p axon-api --test http room_metadata -- --ignored
DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon_test cargo test -p axon-store --test state -- --ignored
DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon_test cargo test -p axon-sync --lib state_redaction -- --include-ignored
DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon_test cargo test -p axon-store --test member_counts -- --ignored
DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon_test cargo test -p axon-sync --lib member_counts -- --include-ignored
DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon_test cargo test -p axon-api --test http room_state_read_endpoints -- --ignored
cargo test -p axon-sync --lib room_sweep
cargo test -p axon-api --test openapi
```

For a running instance, call the new endpoint for a joined room with advertised alternative aliases and compare its `aliases.content` with the room's Matrix state.
Replace the alias state with a newer event that removes the canonical alias and empties `alt_aliases`; the next read must contain the replacement event ID and empty list.
Query a different account or unknown room and verify that missing tuples remain `unknown`.
Do not paste bearer tokens, raw event bodies, or private room identifiers into verification logs.

### Disposable live acceptance

Use an isolated Synapse/PostgreSQL/Axon stack with disposable Matrix accounts and persistent SDK storage.
Do not point these tests at an operator database or use production credentials.

1. Create rooms using room versions 10 and 11, and set power levels with distinct `ban` and `invite` thresholds and restricted join rules with an `allow` condition.
2. Wait for `/metadata` to report the state event IDs, then redact each power-level and join-rule event through Matrix.
   Poll until Axon reports positive evidence and the matching redaction ID.
   The `ban` threshold and restricted join conditions must survive; `invite` is removed in version 10 and retained in version 11.
   Check that `/info` uses the same retained join rule.
3. Redact the current room-name event and check that `/v1/rooms` clears its name after reconciliation.
4. Replace the power-level state, then redact its predecessor again.
   The replacement must keep its own event ID, content, and unknown evidence.
5. In the disposable database only, restore the original power-level content and clear its evidence while preserving the current event ID.
   The paced sweep must restore the SDK-pruned content without another state write.
   Repeat with Axon stopped, then restart against the same SDK store and verify startup recovery.
6. Redact power-level state before a second account joins the room.
   Its first hydrated snapshot must preserve protected fields and report positive evidence.

The fixture homeserver may reject creation-event redactions; verify that a rejected request leaves Axon's evidence unchanged rather than treating the attempt as an applied redaction.
These small-room checks validate reconciliation and recovery, not throughput for hundreds of rooms.

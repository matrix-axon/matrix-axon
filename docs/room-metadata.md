# Room metadata

[ADR 0111](adr/0111-cached-progressive-room-metadata.md) records the architecture for cached room metadata and bounded progressive enrichment.
This guide documents the implemented API, current sync coverage, verification, and remaining work.
[Tracking issue 618](https://github.com/matrix-axon/matrix-axon/issues/618) records the remaining acquisition, member-count, and client work.

## Cached state details

`GET /v1/accounts/{account_id}/rooms/{room_id}/metadata` returns eight typed snapshots from that account's cached room state.
It requires the same bearer authentication as other `/v1/` reads.
It performs one database query for a fixed set of singleton tuples, with no homeserver request, membership aggregation, or discovery-cache merge.
Each state's content is limited to 64 KiB in PostgreSQL before transfer and JSON decoding.
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

Each snapshot includes `status`, `event_id`, `sender`, `origin_ts`, and `content`.
`origin_ts` is the upstream event timestamp, not the time of the last successful sync or an access-freshness guarantee.

| Status        | Meaning                                                                                              |
| ------------- | ---------------------------------------------------------------------------------------------------- |
| `unknown`     | No cached tuple; Axon cannot establish that the setting is unset. Provenance and content are `null`. |
| `available`   | Typed content is available, including explicit empty lists and omitted fields.                       |
| `unavailable` | A tuple exists but its content is absent, for example after redaction.                               |
| `invalid`     | Stored content has an incompatible shape; content is withheld.                                       |
| `too_large`   | Stored content exceeded the content-size bound; content is withheld.                                 |

Only `available` snapshots have non-null content.
Nullable content fields preserve omission instead of filling Matrix defaults.
In particular, an unknown encryption snapshot does not mean "unencrypted," and an unknown alias snapshot does not mean "no aliases."
For available creation state, the enclosing `sender` provides the create-event sender when a room version omits the `creator` content field.
Power levels describe configured state, with legacy numeric strings normalized to integers; they are not resolved permissions and must not drive authorization decisions.
Unknown condition types retain their `type` and optional `room_id`; extension-specific payloads are not exposed by this typed read.
An empty content object is not itself evidence of redaction: the stored projection does not always retain enough information to establish that distinction.

A client should re-read after a relevant state event, when reopening the panel, and on reconnect.
The relevant state types are the eight types in the table above, conveyed through the existing event stream.
There is no new metadata-specific live frame in this step.
Unknown account/room IDs return unknown snapshots with HTTP 200, matching existing cached state-read conventions.
Cached state from a room the account has left remains historical cached state according to the existing retention policy; this endpoint does not assert current membership or upstream access.

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

Authoritative member counts are [issue 620](https://github.com/matrix-axon/matrix-axon/issues/620); the lazily loaded member list is not an authoritative count.
Paginated unjoined-room discovery and summary enrichment are [issue 622](https://github.com/matrix-axon/matrix-axon/issues/622).
The discovery cache, progressive updates, and acquisition budgets follow ADR 0111 and are not implemented by the cached state endpoint.
Web and TUI consumption are [issue 623](https://github.com/matrix-axon/matrix-axon/issues/623) and [issue 624](https://github.com/matrix-axon/matrix-axon/issues/624).

## Verification

The HTTP integration tests seed real PostgreSQL state and exercise the authenticated router without an upstream service.
Use a throwaway database; these tests run migrations and write fixture accounts.

```sh
DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon_test cargo test -p axon-api --test http room_metadata -- --ignored
cargo test -p axon-api --test openapi
```

For a running instance, call the new endpoint for a joined room with advertised alternative aliases and compare its `aliases.content` with the room's Matrix state.
Replace the alias state with a newer event that removes the canonical alias and empties `alt_aliases`; the next read must contain the replacement event ID and empty list.
Query a different account or unknown room and verify that missing tuples remain `unknown`.
Do not paste bearer tokens, raw event bodies, or private room identifiers into verification logs.

# ADR 0111 — Cached room metadata with bounded progressive enrichment

**Status:** Accepted; implementation proceeds through [issue 618](https://github.com/matrix-axon/matrix-axon/issues/618).
The first step adds typed cached state reads; acquisition, discovery, member counts, and client consumption are follow-up work.

## Context

Space membership and room membership are separate.
Axon can cache a parent's `m.space.child` links without having synced the referenced rooms.
The existing space-child read enriches links from each child's locally cached name, avatar, and creation state.
An unjoined child often has none of those tuples, so clients fall back to its Matrix room ID.
Fetching each child's state separately when opening Room Information would make latency and upstream load grow with the number of rooms, including spaces with hundreds of children.

The room information surface also omits metadata that Matrix can provide, such as advertised alternative aliases, creation details, configured power levels, and server ACLs.
These fields differ in source, access rules, size, and freshness.
Some come from joined-room state; member counts come from authoritative SDK summaries; homeserver alias lists and unjoined-room summaries need permission-aware remote acquisition.
An uncached field does not establish that the setting is absent.

[ADR 0055](0055-room-metadata-exposure.md) separates cheap list summaries from detail reads and rejects a single all-metadata response with heterogeneous costs.
[ADR 0084](0084-room-state-read-endpoints.md) replaces generic state passthrough for several clusters with typed reads, including room information and spaces.
This decision extends that typed approach and defines how missing information is acquired without tying panel latency to remote work.

## Decision

### Keep summaries, cached state details, and discovery distinct

Room lists retain a small, inexpensive summary projection.
Selected-room details use typed, bounded local reads with explicit availability and provenance.
The new `/metadata` read groups eight singleton state types into one fixed-size query; it does not trigger upstream requests or aggregate membership.
Existing `/info`, `/upgrade`, and space endpoints remain compatible.
Endpoint fields, limits, and verification belong in the [room metadata guide](../room-metadata.md).

Use progressive enrichment wherever remote acquisition is needed, with acquisition policies chosen for each source.
Do not refresh every field remotely just because a panel opens.
Joined-room state already arrives through sync, and an available local field should be readable immediately.
Expensive or permission-sensitive detail is acquired only when requested for a selected room or when its source-specific refresh policy requires it.

### Joined-room state is authoritative for current joined-room settings

Use account-scoped synced state as the source for joined-room names, aliases, creation, access settings, encryption, power levels, and ACLs.
Acquire missing required state through a bounded repair path, preferring sync configuration where the SDK supports it.
Keep repair off the local read's critical path and coalesce repeated requests.
Synced replacements are complete replacements: omitted fields and explicit empty lists must remove older values.
Discovery data must not fill a field that authoritative synced state has removed.

Cache joined-room member counts from authoritative SDK room summaries.
Do not count the lazily loaded member-list projection, which can contain only a subset of the membership.
Keep counts and their freshness separate from state-event snapshots.

Cached state retained after leaving a room is historical information under the existing retention policy.
A cached read does not assert current membership or continuing upstream access.

### Unjoined-room discovery uses a separate account-scoped cache

Fetch accessible child summaries through the homeserver's paginated room hierarchy API, with immediate children requested and all child links considered rather than suggested children alone.
Use selected-room summary discovery when supported for details that hierarchy results do not provide.
Do not fan out one request per child or per metadata field.

Discovery snapshots are keyed by account and room and remain separate from `room_state`.
They carry source, observation time, expiration, and acquisition outcome so clients can distinguish a stale summary from authoritative synced state.
Permission failures and unsupported discovery are cacheable outcomes with bounded retry policies.
Cached discovery never grants access or changes join authorization.

Return locally known links and summaries immediately, then publish progressive updates as discovery pages are cached.
Pagination must expose continuation and incomplete results; a bounded row cap must not silently imply that every child was returned.
Preserve the parent's child-link membership, ordering, and suggested flags independently of discovered child display metadata.
A partial traversal or a failed page must not erase previously known child links.

When an account joins a discovered room, synced state takes precedence for current settings.
In-flight discovery completions must not overwrite newer synced values or resurrect information after account removal or access invalidation.
When access is known to have been revoked, invalidate the corresponding discovery data according to its access and retention policy.

### Availability and freshness are explicit

Typed state reads distinguish unknown, available, partially valid, unavailable content, invalid shape, and oversized content.
Partially valid snapshots retain valid fields and entries and explicitly identify malformed paths; clients must not interpret filtered data as complete.
Available empty lists are meaningful values.
Missing state is not evidence that aliases are absent or encryption is disabled.
Do not manufacture Matrix defaults or infer redaction from an empty object when the cached projection lacks the necessary evidence.
Configured power levels are metadata, not resolved authorization decisions.

Retain event provenance for state snapshots.
An upstream event timestamp is not a successful-fetch timestamp or a freshness guarantee.
Discovery refresh state and errors are represented separately from cached values so a failed refresh can leave usable stale data visible where access permits.
Clients refresh joined state after relevant live events and on reconnect; discovery notifications cause clients to re-read the affected cached summaries.
Live invalidation must also cover required-state-only sync updates before clients can rely on that mechanism alone.
The initial cached endpoint lacks that invalidation; the guide specifies bounded visible-panel polling as the interim freshness policy.
Reopening a panel can request refresh, subject to expiration and coalescing, without blocking its initial display.

### Alias lists describe their scope

Expose the canonical alias and `alt_aliases` from `m.room.canonical_alias` as **Advertised aliases**.
Expose aliases returned by the account's homeserver alias-list API as **Aliases on your homeserver**, with a separate permission-aware cache.
Neither list is a complete federation-wide enumeration, so clients must not call either a **Full alias list**.

### Acquisition has explicit resource and lifecycle bounds

Use a shared bounded scheduler with fixed concurrency, a bounded queue, request coalescing, timeouts, cancellation, retry backoff, and positive and negative cache expiration.
Bound pages per job, response bytes before decoding, entries per page, cache size, and per-account work so a large space or slow homeserver cannot monopolize resources.
Continuation allows a later job to resume traversal without requiring an unbounded task.
Choose and document the concrete acquisition limits and expiration defaults in the implementation that introduces the scheduler.

Each job carries its account, room, source, and generation or equivalent freshness guard.
The scheduler owns in-flight request coordination; store updates must reject results superseded by newer state, access invalidation, or account teardown.
Do not hold a shared lock across remote I/O.
Account removal cancels queued and active work and prevents late results from recreating its cache entries.
Persisted cache and continuation data must support safe retry after restart; in-flight work cannot leave a permanent pending state.

Bound state content before database transfer and decoding, and validate typed shapes before exposing them.
One invalid field, inaccessible room, or failed page is logged and skipped without failing sync or unrelated enrichment.
Use structured account, room, source, event, and outcome fields and aggregate queue, cache, and timing observations for debugging.
Never log credentials or raw private metadata bodies.
Prefer existing tracing controls; add developer configuration only if implementation reveals a concrete diagnostic need.

### Land server capabilities before client behavior

Implement typed state reads, authoritative member counts, bounded acquisition, and paginated discovery as independently reviewable server steps.
Then add web and TUI consumption in separate client changes.
Keep OpenAPI and generated client schemas synchronized with each server API addition.
The [guide](../room-metadata.md) tracks current coverage and the individual implementation issues.

## Alternatives considered

- **Fetch every child or field when opening the panel.** Request fanout and latency grow with space size and duplicate work across clients and panel openings.
- **Put discovered unjoined-room state into `room_state`.** Discovery summaries have different access, completeness, and freshness semantics and cannot substitute for synced state events.
- **Use one remote all-metadata endpoint for every room.** This couples inexpensive reads to slow or inaccessible sources and repeats the cost problem identified in ADR 0055.
- **Have clients call the homeserver and maintain their own caches.** This duplicates acquisition policy across clients and bypasses Axon's role as the persistent state layer.

## Consequences

Panels can display known data immediately while names and optional details arrive progressively.
Upstream work follows bounded page and job budgets rather than the number of metadata fields multiplied by the number of children.
Clients must support unknown, stale, incomplete, and failed acquisition states instead of treating every missing value as an empty setting.
Discovery introduces cache invalidation, scheduler, and lifecycle responsibilities that require transition tests, including join, leave, access changes, reconnect, restart, and account removal.
This ADR records the architecture; the guide and tracking issues record what has been implemented and the concrete operational limits.

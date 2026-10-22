# ADR 0124 — Isolated image-pack sync

Status: proposed

## Context

The SDK UI's primary Simplified Sliding Sync service does not expose configuration for additional required state.
Emoji discovery needs both existing room packs at startup and subsequent updates at arbitrary state keys.
The generic account-data path already acquires personal packs.

A second SDK-managed Sliding Sync instance shares the primary client's SDK room store.
Even a zero-timeline response can carry room summaries, counts, recency, and pagination fields.
Feeding that response into SDK sync processing risks replacing state owned by the primary service.

## Decision

Use one additional state-only Simplified Sliding Sync transport per supervised account run.
Send typed raw HTTP requests through the authenticated SDK client, with connection ID `axon-packs`, both room-pack types, all state keys, a growing range in steps of 100 rooms, and a timeline limit of zero.
Only allowlisted `required_state` events enter the shared PostgreSQL room-state projection.
Never feed these responses into SDK sync processing; ignore all summaries, timelines, pagination fields, and extensions.
The primary service remains the sole owner of SDK room state.

The account run owns cancellation and joins the transport before restart.
There is one request in flight per worker, SDK retries are disabled, server long polls wait at most 30 seconds, and client plus outer request deadlines are 90 seconds.
Successful polls wait at least one second before the next request.
Failures back off exponentially from two to 60 seconds and preserve the position, except for `M_UNKNOWN_POS`, which starts a fresh connection.
Persistence uses the same timestamp, redaction, and departed-room rules as ordinary state ingestion.
Missing events never imply a pack was deleted.
Partial failures are logged with account and room identity and do not stop primary sync.

ADR 0122 remains proposed and its `HomeserverGate` is absent from this implementation's base.
This PR therefore cannot integrate with an existing gate.
Its inventory exempts SDK sync long polls; the additional pack transport is explicitly documented here rather than silently treated as unrelated SDK traffic.
When that gate is implemented, include this transport in the integration decision: ordinary long-poll latency must not be counted as a slow background read, and request timeouts must retain the gate's abandoned-work protection.
The local bounds above do not claim to replace a shared homeserver load gate.

## Alternatives

Relying only on incidental pack state in primary sync misses existing packs not requested by the SDK UI.
A second SDK-managed sync connection acquires the state but introduces the shared-store interference described above.
Polling whole-room state would add repeated background reads and transfer unrelated state.
Forking the SDK UI to expose required-state configuration adds a dependency maintenance burden; revisiting that option is reasonable if upstream exposes a supported configuration point.

## Consequences

Existing room packs hydrate without manual re-sync, and pack updates cannot overwrite SDK room metadata.
Each account adds a bounded long-poll connection and associated homeserver work.
The transport owns only its in-memory position, so process restart safely starts a fresh session.
Discovery remains a cached snapshot with no initial-sync completeness guarantee.
Tests assert the request shape, delivered pack state, unchanged SDK room state, and cancellation of an unresponsive peer.

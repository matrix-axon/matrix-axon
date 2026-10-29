# ADR 0031 — Sender-device trust: the at-decrypt snapshot, the verification bundle, and the violation overlay

## Context

M7a verified axon's **own** device; 7b gated **clients** with bearer tokens. M7c
is about a third, independent thing: the trust axon reports about the **senders**
of the events it stored — *other* Matrix users' devices in encrypted rooms. The
standard Matrix per-message "shield": was a message from Bob sent from a device
Bob cross-signed (really Bob), or from a new/unverified device (possible
impersonation)? (`docs/mvp/implementation.md` §7c.)

The storage *partly* existed. ADR 0015's `event_sender_device_keys` sibling
already persisted the sending device's identity keys + a coarse
`verification_state` (`verified`/`unverified`) snapshot at decrypt time — but
nothing read it back in production, nothing exposed trust on reads, and the
tech-spec's promised opt-in "verification bundle" API did not exist. This ADR
records the decisions made closing that gap (one PR, three layered parts: snapshot
→ bundle → overlay). 7c **reports** trust; it does not establish it (no
interactive SAS against other users). It does not depend on any new auth: the read
+ bundle surfaces are open like the rest of the read API and ride the 7b bearer
gate automatically.

## Decision

### Snapshot vs. current trust are two separate facts

A message's trust has two distinct readings, and conflating them is a security
trap, so they are reported separately:

- The **snapshot** is what Matrix's cryptographic evidence said *when the event
  arrived*. It is immutable and persisted. A device trusted when it sent can later
  be revoked (and vice versa); the snapshot preserves the at-receipt truth.
- The **current** evidence is read live from the SDK at request time and can
  differ. It lives only in the verification bundle (and the overlay frame below),
  never overwrites the snapshot.

### The four-valued `sender_trust` verdict maps the SDK's `VerificationState`

`meta::sender_trust` collapses matrix-sdk's `VerificationState` /
`VerificationLevel` to the spec's four values: `Verified` → `verified`;
`Unverified(VerificationViolation)` → `verification_violation`;
`Unverified(None(_))` → `unknown` (couldn't link to a device);
everything else unverified → `unverified`. It is recorded in a **new, nullable
`sender_trust` column** on `event_sender_device_keys` (CHECK-constrained),
**alongside** the existing coarse `verification_state` rather than replacing it —
the legacy column and its contract are untouched, and existing rows need no
backfill (a UTD re-decrypted later upserts the verdict). Derived at the single
`crypto_meta` choke point so both the live-dispatch and UTD re-decryption paths
populate it identically (ADR 0015).

The verdict is an **immutable snapshot**, so it is **write-once-non-null**: the
upsert `COALESCE`s `sender_trust` (`COALESCE(existing, EXCLUDED)`) rather than
overwriting it. The first recorded non-null verdict — the trust Matrix's evidence
reported when the event first decrypted — wins; a duplicate delivery or a later
re-decryption after the sender's trust changed cannot rewrite history. A still-
`NULL` value (the initial UTD never decrypted) is still populated by the
re-decryption that first reads the event. The coarse legacy `verification_state`
keeps its plain overwrite contract — only the new snapshot column is frozen.

### `sender_trust` rides the existing read shapes; the bundle is a new opt-in route

The snapshot is surfaced on the timeline/single-event `EventDto` and the
`/v1/ws` `timeline.event` payload by joining the sibling into the shared
`TIMELINE_SELECT` projection — no new route for the common case.

Because the snapshot is write-once-frozen, the **live frame must carry the
*persisted* verdict, not the freshly-derived one** — they diverge on a duplicate
delivery whose trust changed (e.g. an event first seen `unverified`, redelivered
after the device is verified: the frozen snapshot stays `unverified`, but the new
`EncryptionInfo` would derive `verified`). So the persist path writes the crypto
sibling **before** emitting, and `upsert_event_crypto` `RETURNING`s the effective
`COALESCE`d `sender_trust`, which is what the frame advertises — guaranteeing a
live subscriber and a subsequent timeline read agree byte-for-byte. *Limitation:*
a UTD's live frame is still emitted before decryption (no `EncryptionInfo` yet), so
its `sender_trust` is `null` until re-decryption back-fills the row; re-decryption
does not re-emit a live frame, so clients re-read the timeline (matches existing
UTD-content behavior).

The richer evidence is an **opt-in** `GET …/events/{event_id}/verification`
bundle: the durable snapshot **plus** live cross-signing evidence (the sender
device's `is_cross_signed_by_owner`, the sender `UserIdentity`'s `is_verified` /
`has_verification_violation` / `was_previously_verified`, and the master key).
A missing device or identity (deleted, not yet downloaded, federation lag) is
reported as "unknown" fields, never an error.

The durable snapshot half carries the full **content-authentication provenance**
the tech-spec promises (`docs/mvp/implementation.md` §7c), not just the device
identity: alongside the sending device's keys and the at-decrypt trust verdicts it
includes the **Megolm session provenance** already persisted in the
`event_megolm_session` sibling — the `session_id` and whether the key reached us
forwarded (`forwarded` + the forwarder's user/device id). The bundle's
`event_sender_trust` store read `LEFT JOIN`s both crypto siblings so a client can
tell "verified, key delivered direct from the sender's device" from "verified, but
the key was forwarded by a third party." Forwarding is provenance a reader needs to
weigh trust, so it belongs in the bundle, not just the device verdict.

### Same consumer-owned-port + composition-root-adapter shape as 7a-6

`axon-api` defines the `SenderTrustService` port it needs;
`axon-sync` owns the concrete `SenderTrustEngine` (it needs a live `Client` to read device/identity);
`axon-server` adapts one onto the other.
`axon-api` and `axon-sync` never depend on each other (ADR 0021/0027).
The bundle is **read-only and takes no per-identity lock**: a read tolerates a client a concurrent teardown is severing (worst case an upstream error), and login and the verify verbs hold that lock across homeserver calls, so taking it would make this per-message read endpoint wait out a slow homeserver it never talks to.
The bundle's own reads are local.
`get_device` / `get_user_identity` pass the `OlmMachine` no key-query timeout, so they return what the crypto store holds without waiting for a pending `/keys/query` (matrix-sdk 0.19).
A cold `get_or_connect` restores the session from disk and spawns E2EE initialization in the background.
The endpoint therefore needs no request timeout (#57).
The store's `event_sender_trust` is the first production reader of the sibling table.

The **`active` gate is an explicit store read**, not `ClientManager::get_or_connect`'s.
`get_or_connect` re-checks `accounts.state` only on a *cache miss* — its cache-hit
fast path returns a still-cached client without re-reading the row. Logout flips the
row to `deactivated` *before* the task drains and the slot is emptied (and a drain
that fails can leave the client cached indefinitely), so relying on the connect gate
alone could serve live trust for a deactivated account with a `200` despite the
documented `409`. The engine therefore reads the authoritative `accounts.state`
itself and returns `NotActive` for any non-`active` row before touching the client, so
a cached-client hit cannot bypass the gate. A benign race remains (the row could flip
*after* the read), but that is the same window every read endpoint has, not the
indefinitely-stale cached-client hole.

### The `verification_violation` overlay is per-identity, not per-event

A sender's identity can enter a violation *after* events were stored (their
cross-signing key changed). Rather than mutate the immutable per-event snapshots,
a per-account watcher subscribes to the SDK's `user_identities_stream` and emits a
live `sender_trust.violation` `LiveFrame` naming the affected **user_id** (not
per-event diffs — that keeps the frame bounded). It tracks which senders it has
reported in-violation and emits **on a transition**: `verification_violation:
true` when a sender enters a violation and `false` when it clears — the clear
frame is what lets a client un-badge from the live stream alone. The frame is a
*push notification to re-evaluate*; the verification bundle / timeline re-read
remains the source of truth. The watcher shares the run-scoped child-token + drain
lifecycle of the `verified`-flag watcher (ADR 0026), so it never outlives its
client.

A failed `user_identities_stream` subscription, or a stream that ends, pauses the overlay rather than ending it (#58).
The watcher resubscribes on the supervisor's backoff (1 s doubling to 60 s, reset only after a stream stayed up for 60 s) until its run ends.
The reported-in-violation set survives the gap, and on each resubscribe the watcher rechecks those senders against the crypto store and emits the clear for any that cleared meanwhile.
A sender whose identity is no longer in the store keeps the last report: a missing identity is no evidence of a clear, and un-badging on it would fail open.
A sender who _entered_ a violation during the gap is not reported until their identity next changes, because the stream has no replay (#890);
the bundle still reports current trust on demand.

One known limitation remains, tracked as a follow-up issue: the bundle's live device lookup can't surface a `MismatchedSender` device (#59).
It is degraded-not-blind — the snapshot still records the mismatch.

## Consequences

- A new nullable column and one forward-only migration; no rewrite of existing
  crypto-sibling rows.
- `AppState::new` gains a `trust` port argument (one composition-root wiring + the
  test stubs).
- Clients can badge messages by `sender_trust` and drill into the bundle on
  demand; `axon-tui`'s badge UI is separate client work (like the 7a-6 verify UI),
  not part of this backend PR.
- Establishing trust interactively against *other* users stays out of scope; 7c
  evaluates and exposes, it does not verify.

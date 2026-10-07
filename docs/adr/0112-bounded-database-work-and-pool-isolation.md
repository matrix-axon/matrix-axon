# ADR 0112 — Bound database work and isolate availability-critical pools

**Status:** Accepted.

## Context

Issue #614 exposed unbounded progress queries that exhausted the shared PostgreSQL pool and prevented authentication, ingestion, and indexing.
Canceling an HTTP request did not terminate its database statement.
A timeout alone would leave status expensive, and a single shared pool would still couple independent workloads during slow queries.

## Decision

Initialize PostgreSQL statement, lock, and idle-transaction deadlines on every connection, including replacements, and bound pool acquisition.
Ordinary statements default to 10 seconds, locks to 3 seconds, acquisition to 5 seconds, maintenance statements to 120 seconds, migration statements and locks to 600 seconds, and idle transactions to 30 seconds.
The six settings are configurable and reject zero; ordinary, maintenance, and migration statement deadlines must be ordered.
These bound individual statements and pool waits rather than an entire multistep operation.

Use independent ordinary/auth/ingest and hot-read pools, each with `database.max_connections` slots, plus one connection each for status, indexing, and maintenance.
The server verifies all `2 * max_connections + 3` slots before readiness and closes its pools on verification failure.
Short-lived token/OAuth commands verify a single ordinary connection; test helpers verify an ordinary connection and acquire other pool slots lazily.
All modes initialize the same session deadlines.
Session-mode PgBouncer works with post-connect initialization; transaction-mode poolers cannot guarantee the session settings or session advisory locks Axon requires.

Maintain exact event totals with statement-level INSERT/DELETE transition-table triggers instead of a full event-table scan per status request.
The migration pays for one initial scan.
The trigger locks each affected account row until the SQL transaction commits, ordering accounts to avoid opposite lock acquisition in multi-account statements.
Live sync and backfill both call `Store::upsert_event`, which writes one event and its projection in one autocommit statement; neither holds a PostgreSQL transaction across a network request, producer delay, or decryption.
Deletion commits batches of at most 1000 events, cascading event crypto siblings within each batch.
Concurrent same-account ingestion is covered by the counter-lock regression.
`COPY FROM` fires the INSERT trigger and updates the total, as confirmed against the disposable PostgreSQL test database.
Account IDs on stored events are immutable in application code, and application code never uses `TRUNCATE` or disables triggers; such administrative operations require rebuilding the totals.

Status derives room progress from summaries and totals from those counters.
If optional progress is unavailable, return the remaining status fields with `backfill.progress_available=false`.

Room purge captures a durable event watermark, deletes in committed batches with atomic per-event index obligations, and conditionally removes its intent only if that watermark is still current.
A new leave advances a pending watermark; an ordinary retry must not advance it or it could erase messages received after a rejoin.
Local leave state and its purge intent commit in the same transaction; an enqueue failure rolls back the state write, and stale membership events cannot advance the intent.
A supervised worker wakes on durable enqueue, retries every 30 seconds, and rotates pages past persistent failures.
Later rejoin events and membership preserve current room metadata.
Account deletion retains its existing `deleting` breadcrumb until all final metadata is removed, and replaying an account-purge sentinel is safe.

## Transaction audit

The explicit store transactions are in `tokens`, `oauth_native`, `oauth_bind_requests`, `oauth_identities`, `matrix_oauth_acquire`, local leave/intent persistence in `state`, and the room-summary rebuild in `rooms`.
Their awaited operations inside a transaction are PostgreSQL queries, transaction helpers, commit, or rollback.
None awaits a homeserver, SDK decryption, filesystem operation, timer, or background task while holding a transaction.
Native identity verification occurs before the store transaction; Matrix OAuth acquisition adopts the SDK store after its database commit.
Room-summary rebuild is the longest explicit transaction and uses maintenance statement deadlines; time spent executing SQL is not idle-in-transaction time.
No production event-insert path opens an explicit transaction across event batches.
The 30-second idle deadline therefore bounds abandoned sessions without imposing a 30-second deadline on a running SQL statement.

## Consequences and follow-ups

The counter trigger introduces per-account serialization, and large batches remain subject to lock deadlines.
If measured contention warrants it, replace the counter with sharded deltas and bounded folding; retaining exact transactional totals currently keeps status simple and consistent.

Room purge, account teardown, summary rebuild, pending-UTD scans, and stale refresh-token cleanup share one maintenance connection.
Batches release that connection between statements, so ordinary/auth, hot reads, status, and indexing stay independent, but maintenance jobs can delay one another.
The final account deletion still cascades bounded-by-account room metadata in one maintenance statement; operators may raise the maintenance deadline for exceptionally large accounts.
Per-class maintenance scheduling and batching the remaining metadata are follow-ups rather than additional pools in this change.

Refresh-token sweeping remains opportunistically spawned with a shared single-flight guard and cooldown.
It is safe to cancel because each delete batch commits independently, and token retention semantics do not depend on physical cleanup.
Moving it to a shutdown-tracked server maintenance supervisor remains a follow-up.

A failed leave-state/intent transaction is logged under the existing best-effort sync policy.
As with other failed ingestion writes, recovery of events already checkpointed by the SDK remains an ingestion-level concern; a successful leave projection can no longer commit without its purge intent.

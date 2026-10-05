# ADR 0039 — Full-text search: the Tantivy index (M9)

## Context

M9 adds full-text search over message bodies. `axon-search` opens a [Tantivy]
index, populated as events are ingested, and serves `GET /v1/search` (the query
endpoint is 9b; this ADR and the 9a PR are the engine). The milestone is
sequenced after relation aggregation (M8) and before history backfill (M10) on
purpose: indexing reads the *resolved* M8 projection (latest edited body,
redaction-masked), and standing the index up before backfill means the deep
history M10 pages in is indexed incrementally through the same ingestion path —
one pass, no separate bulk reindex (see `docs/mvp/implementation.md` §9).

Three facts make the design non-trivial:

- **Ingestion is not ordered by causality.** Backfill pages newest→oldest, so a
  relation can arrive *before* its target — an edit or redaction indexed, then
  the older target landing later. A naïve "index the event you just stored"
  would index a stale or already-redacted body.
- **Tantivy has a single writer.** Only one `IndexWriter` may exist per index,
  and commits are expensive (fsync + segment merge), so writes must be funneled
  through one owner and committed in batches — not per event.
- **The index must *converge*, not merely be best-effort.** It is a derived view
  of the event store, and the requirements treat it as one: every event that was
  durably persisted must become searchable (and every redaction/deletion must
  un-search its target), even across overload, periods with search disabled, a
  lost/replaced index directory, or a crash. A lossy in-memory queue cannot
  promise that; the durable mechanism below is what does.

## Decision

### Index from the resolved projection, never the raw event

The unit of work is "(re)derive the document for `(account_id, event_id)`", not
"store this event's text". Each index-affecting store write records the affected
id(s) — the event's own id **plus**, for a relation or redaction, its *target's*
id (see the outbox below). The indexer resolves each id through
`Store::get_event` — the same M8 projection the read API uses — and:

- a message with a resolved body → **upsert** the document (delete-then-add);
- a redacted event (projection masks the body to `NULL`), a standalone relation
  (an `m.replace` edit or `m.reaction` annotation, whose text belongs to the
  target), or a non-text event → **delete** the document.

This makes indexing **order-independent**. *Edit before target:* indexing the
edit resolves the target id, which isn't stored yet → no-op; when the target
lands, its own command re-derives it and the projection already folds the edit in
→ the edited body is indexed. *Redaction before target:* same — once the target
arrives the projection masks it → the document is deleted/never added. *UTD then
decrypt:* the encrypted row indexes nothing (no body); the re-decryption back-fill
re-enqueues it → the now-decrypted body is indexed. The build also re-derives, so
a from-scratch rebuild always converges; the index is derived data keyed by
`event_id`.

### A transactional outbox makes the index convergent

Correctness rests on a **durable change log, not an in-memory queue**. Every store
write that can change a resolved document appends a row to `search_outbox` —
`(seq, account_id, event_id)` — **in the same transaction as the mutation**
(`upsert_event`, `update_decrypted_event`, `delete_account_row`, via a single
`WITH … RETURNING` statement). So the event change and its indexing obligation
commit atomically: the system can never persist one without the other. Entries are
deliberately *engine-neutral* — they name *which* document may have changed, never
a Tantivy document; the search crate decides how to resolve and index it. A new
message enqueues its own id; an edit/redaction enqueues its **target's** id (the
top-level `relates_to->>'event_id'` or `redacts`; a plain reply has no top-level
target and changes no other document, so it is excluded); a re-decryption (an
in-place `UPDATE` that mints no new `events.id`) enqueues its own id — the outbox
is the *only* durable signal for that case. `event_id = ''` is the account-purge
sentinel.

One actor task owns the sole `IndexWriter` and **drains the outbox in `seq`
order**: it applies a batch, commits (amortizing the fsync), then advances a
durable cursor (`search_meta.outbox_cursor`) and prunes the applied rows. The
cursor moves *only after* a commit and strictly in order, so it is always a
**contiguous complete prefix** of applied-and-committed work — never "two frontiers
happened to reach N". A crash mid-batch re-runs that batch (indexing is idempotent,
keyed by `(account_id, event_id)`).

Ingestion no longer carries the work: the sync persist handler and the
re-decryption back-fill only poke the actor with a best-effort
`IndexHandle::notify` — a coalescing wakeup hint that may be dropped on a full
channel **without consequence**, because the durable obligation is already on disk
and the next drain (a later notify, a periodic safety tick, or a restart) applies
it. This dissolves the old "full channel ⇒ permanent divergence" failure rather
than patching it. The live broadcast bus (`/v1/ws`) was rejected as the trigger: it
is lossy by design and not a reliable signal.

### Resource boundedness

The seed (below) and the live drain share the one writer and stay bounded: both
read in keyset/`seq` batches — `Store::events_for_index(after_id, limit)` and
`drain_search_outbox(after_seq, limit)`, a single query per batch, never a query
per event — so the actor holds at most one pooled connection at a time and leaves
the shared `PgPool` headroom for sync and API reads. The seed throttles between
batches (configurable) and runs as a **background task**, so boot never blocks on
it even for a large corpus. The writer has a fixed heap budget. The outbox is
pruned to the cursor every drain, so it stays small in steady state (it grows only
while search is disabled — bounded by ingestion, drained on the next enable).

### `events` is the rebuild source; the outbox is the incremental delta

The outbox is **prunable** precisely because it is *not* the reconstruction source:
the authoritative rebuild source is `events` itself. When the physical index is
**fresh** — a new, empty, deleted/replaced, or schema-bumped directory — the actor
**seeds** from `Store::events_for_index` (a full keyset scan of the resolved
projection from `id = 0`), which reproduces the complete current index independent
of the outbox. Pruning can therefore never remove information needed to
reconstruct a lost index (the invariant the pruning rule must satisfy).

Freshness is detected by a **seed-completion marker inside the index directory**
(`axon_schema_version`), compared on `open` *before* Tantivy's `open_or_create`
(which would otherwise reject a changed schema and fail boot instead of rebuilding).
A missing or mismatched marker wipes and recreates the directory and signals a seed.
Tracking the version against the *physical* index — not a global DB marker — is what
lets a new/empty/moved directory be detected; a global "built" flag cannot.

Crucially, the marker means **"valid for this schema *and* fully seeded"**, not
merely "openable". It is stamped by the actor **only after `seed` completes and is
committed**, never at `open`. So a crash or failure mid-seed — including after
`delete_all_documents` or after only part of the corpus is committed — leaves *no*
marker, and the next `open` sees `fresh`, wipes the partial index, and reseeds
rather than trusting it. (Marking at open was the bug an early draft shipped: it
declared completion before the corpus was durable, so an interrupted seed could be
trusted as complete forever.) A `fresh` open therefore yields an *empty* index that
fills in as the actor seeds; the seed rebuilds in place (honest partial results
during the window) rather than via a staging-directory swap, consistent with the
best-effort, never-blocks-boot posture. If the seed itself errors, the actor stops
instead of draining the outbox onto a partial index — degrading to an honestly empty
index that self-heals on the next boot.

After a seed, the cursor is set to the outbox high-water mark captured *before* the
seed, so the subsequent drain applies only changes made since (everything earlier
is already in the corpus scan). Because the seed reflects *whatever is in `events`*,
enabling search after M10 has backfilled deep history indexes the full corpus, not
just a pre-backfill slice — the "small slice" framing in the implementation doc
assumes search was on from M9 onward. A steady-state restart is *not* fresh: the
actor simply resumes draining the outbox tail from the stored cursor.

The one bounded, self-healing gap: a change whose transaction is in flight at the
exact instant the high-water mark is captured and that the seed scan then races
past. The next edit/redaction/redecryption to that event re-enqueues it, and `axon
search reindex` (9b) forces a clean rebuild — so it cannot become permanent.

### Account deletion: a durable, ordered, search-disabled-safe purge

A deleted account's documents must stop being searchable, and the cross-store
ordering must survive a crash (ADR 0024). `delete_account_row` appends the
account-purge sentinel to `search_outbox` **in the same statement** that drops the
row. The outbox has **no foreign key** to `accounts`, so the obligation *outlives*
the row it cascades — exactly the breadcrumb boot catch-up needs. This holds in all
three cases the review flagged: a crash before the purge commits to Tantivy (drained
next boot), a deletion while `search.enabled = false` so no actor is running (drained
on the next enabled boot), and the normal case. When the actor *is* live, the
lifecycle verb additionally calls `IndexHandle::flush` after the row delete — drain
to empty and acknowledge — so the documents are actually gone from Tantivy before
the verb returns, closing the privacy window synchronously rather than relying on
eventual catch-up. `axon search reindex` (9b) wipes the directory to force a seed.

### Keyword fields, not Tantivy facets

`account_id` / `room_id` / `sender` are exact-match keyword (`STRING`) fields, not
hierarchical Tantivy *facets* (which the tech spec names). MVP filtering is flat
equality; `STRING` fields are robust to the `!`/`:`/`@`/`/` characters in room ids
and senders without facet-path escaping, and `STRING | STORED` doubles as the
retrieval field used to hydrate a hit. The same Matrix `event_id` can exist under
two accounts, so the delete key is a composite `doc_key = "<account_id>\u{1f}<event_id>"`,
ensuring a per-account delete removes exactly one document. Hierarchical faceting
(e.g. faceted counts) can revisit this; the schema is versioned so a change forces
a clean rebuild.

### Analyzer: one language-agnostic chain

The `body` analyzer is the default tokenizer plus three built-in token filters —
`LowerCaser`, `AsciiFoldingFilter`, `Stemmer(English)` — bounded by
`RemoveLongFilter` (matching Tantivy's `en_stem`). This gives case-insensitivity,
diacritic folding (`café` ≈ `cafe`), and regular singular/plural and verb-form
matching (`cats` ≈ `cat`). No fuzzy, synonym, semantic, per-language, or CJK
handling in MVP (tech spec). Phrase queries are supported (positions indexed).

### Query: Tantivy ranks, Postgres hydrates

`GET /v1/search` (9b) parses the query against `body` (BM25, the Tantivy default),
applies the keyword/range filters, and gets back ranked `(account_id, event_id)`
ids, which it hydrates via `Store::get_event` — Postgres stays the source of truth
for content/edits/redaction, so a hit redacted since indexing is dropped on
hydrate. Pagination is offset/limit (BM25 score doesn't compose with the timeline's
opaque cursor). Cross-account by default; `account_id` is an optional filter (tech
spec: "account_id as a facet… scope to one account or aggregate across all").

The optional `sort` query parameter selects `relevance` (the default), `newest`, or `oldest`.
Timestamp ordering uses `origin_ts` across all matching documents **before** offset/limit pagination, so a recent low-relevance match can lead the first page.
Equal timestamps are ordered by relevance, then Tantivy document address; this is deterministic for an unchanged index, rather than a snapshot guarantee across index updates.
The existing timestamp fast field supplies the ordering without a schema change or index rebuild.
The response's `score` remains the BM25 score in every sort mode.
Keep the filters and sort unchanged when following `next_cursor`; changing the sort starts a new search without a cursor.
The cursor carries only an offset, so the server cannot detect reuse with a different query or sort and does not reject it.
Such reuse can skip or repeat matches; query-bound cursors would require a separate compatibility design.
Unknown sort values return `400`, and selecting a sort alone does not allow an unbounded empty query.
Existing clients that omit `sort` retain relevance pagination and need a separate client change to request server date ordering.

The query path is the first heavy user-triggered work on the bearer-gated API, so it carries three explicit resource bounds.
**Collector memory** is bounded by capping the decoded paging offset (`MAX_OFFSET`): Tantivy keeps the top `offset + limit` documents per segment and merges them, so a forged huge offset would amplify allocation and heap maintenance.
A cursor past the cap is a `400`, and the cursor module is the single place that decodes offsets through a bounded helper.
The offset cap does not bound total CPU work: date ordering visits and scores every matching document to preserve BM25 scores and use relevance to break equal-timestamp ties, and the exact total count also requires visiting all matches.
Broad queries can therefore cost more as the matching corpus grows, even with a small page size.
**Concurrency** is bounded by a semaphore in the query adapter (`search.max_concurrent_queries`, default 8): each query runs on a blocking thread and holds a Tantivy reader, so over-limit queries queue rather than flooding the blocking pool.
The blocking task owns its permit until it finishes; timing out the response does not release it while the work is still running.
**Execution-response latency** after acquiring a permit is bounded by a per-query timeout (`search.query_timeout_ms`, default 10s), returning `503` on expiry.
Permit acquisition itself is outside that timeout, and a timed-out `spawn_blocking` closure cannot be canceled, so neither total request latency nor background completion time has this bound.
These are personal-scale defaults, tunable via `[search]`.

### Dependency direction

`axon-search` is a leaf crate (depends only on `axon-core` + `axon-store`).
`axon-sync` depends on `axon-search` directly to *notify* the actor (the work
itself flows through the store's outbox, not the dependency) — the producer *is*
`axon-sync`, there is no consumer-owned-port inversion to do (unlike
`MessageSender`), and there is no cycle.

The query side (9b) goes the *other* way: `axon-api` defines a small `SearchQuery`
**port** (the query params, ranked hits, and a HTTP-shaped error), and `axon-server`
adapts `axon-search` onto it — the same consumer-owned-port pattern as `MessageSender`,
`AccountLifecycle`, `MediaProxy`, and the other API capabilities. An earlier draft of
this ADR called for a *direct* `axon-api → axon-search` dependency ("matrix-free, like
`axon-store`, which `axon-api` already depends on directly"). That reasoning is sound
about Matrix purity and cycles but misses two things the implementation weighed: (1)
`axon-search` pulls in `tantivy`, a heavy dependency, and the port keeps it (and its
build/test cost) out of `axon-api`, whose deps stay `axon-core` + `axon-store` + `axum`;
(2) every other capability the API needs is already a port, so a lone direct search
dependency would be the odd one out. `axon-store` stays direct because it *is* the shared
data layer every route touches and carries no comparably heavy transitive deps. The port
adds ~90 LOC of near-duplicate types plus a mechanical adapter; that cost buys the
decoupling and uniformity above and is accepted.

## Consequences

- The index holds **decrypted** message text. It lives on the same disk as
  Postgres and inherits the operator's filesystem-level encryption; application-
  level index encryption would defeat search and is out of scope for MVP (tech
  spec threat model).
- Search can be disabled (`search.enabled = false`): the actor does not run and
  `GET /v1/search` returns `503` (9b). Event writes **still** append outbox rows
  (they are unconditional and cheap), so re-enabling search later catches up every
  event ingested in the meantime — at the cost of the outbox accumulating (bounded
  by ingestion) until that drain. Account deletions while disabled are likewise
  honored on re-enable via the durable purge sentinel.
- A dropped `notify`, an interrupted seed, or a crash mid-drain is self-healing:
  the durable outbox + contiguous cursor mean the next drain re-derives exactly the
  un-applied prefix, and `events` can always reconstruct a lost index. An interrupted
  seed specifically is caught because the seed-completion marker is stamped only
  after the seed commits, so a partial index is never trusted as complete.
- Indexing lags ingestion slightly (batched commits + drain latency); a just-sent
  message may take up to the idle-poll interval to become searchable. Acceptable at
  personal scale.
- The outbox adds a small write-amplification to event ingestion (one to three
  extra rows per event, in the same transaction) and moves the relation/redaction
  fan-out into the store's write statements. Accepted: it is what buys atomic,
  convergent indexing, and it removes the scattered enqueue logic from the sync
  path.

[Tantivy]: https://github.com/quickwit-oss/tantivy

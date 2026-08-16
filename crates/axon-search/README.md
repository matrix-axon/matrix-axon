# axon-search

Tantivy full-text search index, populated on event ingestion.

## Responsibility

Opens and manages the Tantivy index, indexes events as they are written by `axon-sync`, and answers full-text queries with BM25 ranking plus filter-only queries narrowed by account, room, sender substring, or time range (exposed over HTTP as `GET /v1/search` by `axon-api`). Filters use keyword fields, not Tantivy facets (ADR 0039).

## Owns vs. consumes

- **Owns:** the Tantivy index directory on disk.
- **Consumes:** `axon-core` config, `axon-store` event types.

## Public API

- `SearchIndex::open` / `spawn_indexer` — open the index and start the single-writer
  indexing actor that drains the store's `search_outbox`.
- `SearchIndex::search(&SearchParams) -> SearchResults` — BM25 query, or filter-only
  query when text is empty, with keyword/time filters, returning
  `(account_id, event_id, score)` hits to hydrate from Postgres.
- `SearchIndex::mark_for_reseed` — clear the seed-completion marker so the next `open`
  rebuilds the corpus from `events` (the `axon search reindex` operator path).

The query path is adapted onto `axon-api`'s `SearchQuery` port by `axon-server`, keeping
`axon-api` free of `tantivy` (ADR 0039).

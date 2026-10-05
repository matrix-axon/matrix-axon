//! Tantivy full-text search index, populated on event ingestion.
//!
//! The index is **derived data**, keyed by `(account_id, event_id)` and rebuilt
//! from the Postgres event store at any time — so every operation here is safe to
//! replay and a from-scratch rebuild always converges. Two halves:
//!
//! * **Write path** ([`writer`]): a single background actor owns the one Tantivy
//!   [`IndexWriter`](tantivy::IndexWriter). It drains the durable `search_outbox`
//!   change log (rows written transactionally with each event mutation), and for
//!   each work item re-derives the target's document from the store's **resolved
//!   M8 projection** (latest edited body, redaction-masked) rather than from the
//!   raw event — so indexing is order-independent (an edit or redaction that
//!   arrives before its target can't corrupt the index) and convergent (the
//!   durable outbox + a contiguous cursor mean no successful write goes unindexed).
//!   Ingestion only pokes the actor with a best-effort [`IndexHandle::notify`]
//!   hint; commits are batched.
//! * **Read path** ([`SearchIndex::search`]): BM25 ranking over the `body` field
//!   when text is present, with filter-only searches supported through
//!   `account_id` / `room_id` / sender-substring / `origin_ts` filters. Returns
//!   `(account_id, event_id)` hits the caller hydrates from Postgres.
//!
//! See `docs/mvp/implementation.md` §9 and the ADRs.

mod index;
mod schema;
mod writer;

pub use index::{SearchHit, SearchIndex, SearchParams, SearchResults, SearchSort};
pub use writer::{IndexHandle, IndexerHandles, IndexerOptions};

/// The index schema version. Bumped whenever the Tantivy schema or analyzer chain
/// changes in a way that requires re-indexing. It is written to a sidecar file
/// inside the index directory; [`SearchIndex::open`] compares the sidecar to this
/// constant and, on a mismatch (or a new/empty directory), recreates the directory
/// and signals a from-scratch corpus seed. Tracking it against the physical index
/// — not a global DB marker — is what lets a lost or moved directory be detected.
pub const SCHEMA_VERSION: &str = "1";

/// Errors from opening, building, or querying the search index.
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    /// Filesystem error creating or opening the index directory.
    #[error("search index i/o: {0}")]
    Io(#[from] std::io::Error),
    /// An error from Tantivy itself (schema, writer, query execution).
    #[error("tantivy: {0}")]
    Tantivy(#[from] tantivy::TantivyError),
    /// Failed to open the index directory.
    #[error("search index directory: {0}")]
    Directory(#[from] tantivy::directory::error::OpenDirectoryError),
    /// A read against the event store failed (used by the indexing actor and the
    /// one-time bulk build).
    #[error("store: {0}")]
    Store(#[from] axon_store::StoreError),
    /// The caller's query string could not be parsed.
    #[error("invalid search query: {0}")]
    BadQuery(String),
    /// The indexing actor was asked to shut down before seeding finished.
    #[error("search seed cancelled")]
    Cancelled,
}

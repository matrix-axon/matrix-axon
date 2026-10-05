//! The full-text-search port the `/v1/search` handler depends on.
//!
//! `axon-api` defines this trait — the capability it *needs* — rather than
//! depending on whatever provides it. The real implementation lives in
//! `axon-search` (the Tantivy index), adapted onto this port by `axon-server`. So
//! this crate stays free of `axon-search` and `tantivy`: the handler speaks only
//! [`SearchQuery`] and plain types, then hydrates each hit from the store.
//!
//! The port is async even though the underlying Tantivy query is synchronous: the
//! adapter runs it on a blocking thread so a heavier query never stalls a runtime
//! worker. Params are therefore owned (movable into that thread), not borrowed.

use async_trait::async_trait;
use uuid::Uuid;

/// Ordering applied to all matching documents before pagination.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SearchSort {
    /// Highest BM25 relevance first (the default).
    #[default]
    Relevance,
    /// Highest origin timestamp first.
    Newest,
    /// Lowest origin timestamp first.
    Oldest,
}

/// An owned search query plus its filters and offset pagination. Empty `text`
/// means a filter-only search; the HTTP handler rejects unbounded empty searches
/// before this port is called. All filters are optional; omit `account_id` to
/// search across every account (the index is one combined index).
#[derive(Debug, Clone)]
pub struct SearchQueryParams {
    /// The full-text query string, parsed against the message `body`. Empty means
    /// "match every document, then apply filters".
    pub text: String,
    /// Restrict to one account.
    pub account_id: Option<Uuid>,
    /// Restrict to one room.
    pub room_id: Option<String>,
    /// Restrict to senders whose Matrix user id contains this substring,
    /// case-insensitively.
    pub sender: Option<String>,
    /// Inclusive lower bound on `origin_ts` (ms since epoch).
    pub from_ts: Option<i64>,
    /// Inclusive upper bound on `origin_ts` (ms since epoch).
    pub to_ts: Option<i64>,
    /// Ordering applied before pagination.
    pub sort: SearchSort,
    /// Maximum hits to return.
    pub limit: usize,
    /// Number of leading hits to skip (offset pagination).
    pub offset: usize,
}

/// One ranked hit: the `(account_id, event_id)` key the handler hydrates from the
/// store, plus its BM25 score.
#[derive(Debug, Clone)]
pub struct SearchHit {
    /// The account the matching event belongs to.
    pub account_id: Uuid,
    /// The matching event's id.
    pub event_id: String,
    /// BM25 relevance score (higher is more relevant).
    pub score: f32,
}

/// A page of ranked hits plus the total match count across all pages.
#[derive(Debug, Clone)]
pub struct SearchHits {
    /// The hits on this page, in the requested sort order.
    pub hits: Vec<SearchHit>,
    /// Total number of matching documents across all pages.
    pub total: usize,
}

/// What can go wrong running a search query. Deliberately small and HTTP-shaped:
/// the adapter collapses its richer backend error into one of these so the handler
/// maps a stable set of statuses.
#[derive(Debug)]
pub enum SearchQueryError {
    /// The caller's query string could not be parsed. → `400`.
    BadQuery(String),
    /// The query exceeded its wall-clock budget (the adapter's per-query timeout).
    /// A transient overload signal, so → `503`, distinct from a hard failure.
    Timeout,
    /// The index failed to execute the query. → `500` (logged at the adapter).
    Internal,
}

impl From<SearchQueryError> for crate::response::ApiError {
    fn from(err: SearchQueryError) -> Self {
        match err {
            SearchQueryError::BadQuery(msg) => Self::bad_request(msg),
            SearchQueryError::Timeout => Self::service_unavailable("search timed out"),
            SearchQueryError::Internal => Self::internal(),
        }
    }
}

/// Runs queries against the search index. Implemented outside this
/// crate; held in [`AppState`](crate::AppState) as `Option<Arc<dyn SearchQuery>>`,
/// where `None` means search is disabled and `/v1/search` returns `503`.
#[async_trait]
pub trait SearchQuery: Send + Sync {
    /// Run a full-text or filter-only search, order all matches as requested,
    /// and return the page of hits plus the total match count.
    async fn search(&self, params: &SearchQueryParams) -> Result<SearchHits, SearchQueryError>;
}

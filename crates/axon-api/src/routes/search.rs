//! Full-text search endpoint (M9b).
//!
//! `GET /v1/search` runs a relevance- or timestamp-ordered query against the `axon-search` index (via the
//! [`SearchQuery`] port) and hydrates each hit into the same resolved [`EventDto`]
//! the rest of the read API returns. The index holds only `(account_id, event_id)`
//! keys, so hydration is a per-hit store read; an index/DB race (a hit whose row
//! was since deleted) drops that hit rather than failing the page.

use std::sync::Arc;

use axon_store::Store;
use axum::extract::State;
use serde::Deserialize;
use utoipa::IntoParams;
use uuid::Uuid;

use crate::cursor;
use crate::dto::{EventDto, SearchPage, SearchResultDto};
use crate::extract::Query;
use crate::response::{ApiError, ApiResponse};
use crate::search::{SearchQuery, SearchQueryParams, SearchSort};

const NARROWING_FILTERS: &[SearchFilter] = &[
    SearchFilter::AccountId,
    SearchFilter::RoomId,
    SearchFilter::Sender,
    SearchFilter::From,
    SearchFilter::To,
];

/// Default page size when `limit` is omitted.
const DEFAULT_LIMIT: i64 = 50;
/// Hard cap on page size, regardless of the requested `limit`.
const MAX_LIMIT: i64 = 200;
/// Hard cap on the paging offset a cursor may decode to. Offset pagination is
/// skip-N work at the index (Tantivy's collector keeps the top `offset + limit`
/// docs), so an attacker-forged cursor for a huge offset would amplify one request
/// into large allocation/CPU. Cursors past this bound are rejected as a `400`
/// rather than executed. At `MAX_LIMIT` per page this is still thousands of pages.
const MAX_OFFSET: usize = 100_000;
/// Max index pages fetched to fill one response page when the membership filter
/// (ADR 0044) drops hits. Bounds the per-request index work so a query dominated
/// by left-room matches can't turn one request into unbounded index paging; if the
/// budget is hit the page is returned short (with a cursor) rather than empty.
const MAX_FILL_ROUNDS: usize = 8;

/// Query parameters for `GET /v1/search`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SearchQueryDto {
    /// The full-text query string. Parsed against message bodies; all terms are
    /// required (AND). May be omitted when at least one narrowing filter is
    /// present.
    pub q: Option<String>,
    /// Restrict to one account. Omit to search across all accounts.
    pub account_id: Option<Uuid>,
    /// Restrict to one room.
    pub room_id: Option<String>,
    /// Restrict to senders whose Matrix user id contains this substring,
    /// case-insensitively.
    pub sender: Option<String>,
    /// Inclusive lower bound on `origin_server_ts`, Unix milliseconds.
    pub from: Option<i64>,
    /// Inclusive upper bound on `origin_server_ts`, Unix milliseconds.
    pub to: Option<i64>,
    /// Sort all matches before pagination: relevance (default), newest, or oldest.
    /// Date ordering uses the event's origin timestamp, not arrival time.
    #[serde(default)]
    pub sort: SearchSort,
    /// Page size (default 50, max 200).
    pub limit: Option<i64>,
    /// Opaque cursor from a previous page's `next_cursor`; omit for the first page.
    pub cursor: Option<String>,
}

/// Search across the index, BM25-ranked when a full-text query is present,
/// or timestamp-ordered when `sort=newest` or `sort=oldest`, then paginated.
/// Keep the query filters and sort unchanged while following cursors.
/// Changing the sort requires starting again without a cursor.
///
/// `503` when search is disabled (`search.enabled = false`). A missing/empty `q`
/// is allowed only with at least one narrowing filter (`account_id`, `room_id`,
/// `sender`, `from`, or `to`); an unbounded empty query or malformed `cursor` is
/// a `400`. Results are the resolved read-API event view (latest edited body,
/// redaction-masked) plus each hit's score.
#[utoipa::path(
    get,
    path = "/v1/search",
    params(SearchQueryDto),
    responses(
        (status = 200, description = "A page of search results in the requested order", body = ApiResponse<SearchPage>),
        (status = 400, description = "Unbounded empty query or malformed cursor", body = crate::response::ErrorResponse),
        (status = 503, description = "Search is disabled", body = crate::response::ErrorResponse),
    ),
    tag = "search",
)]
pub async fn search(
    State(store): State<Store>,
    State(search): State<Option<Arc<dyn SearchQuery>>>,
    Query(q): Query<SearchQueryDto>,
) -> Result<ApiResponse<SearchPage>, ApiError> {
    let Some(search) = search else {
        return Err(ApiError::service_unavailable("search is disabled"));
    };

    let text = q.q.as_deref().unwrap_or_default().trim();
    if text.is_empty() && !has_narrowing_filter(&q) {
        return Err(ApiError::bad_request(format!(
            "q is required unless at least one filter is present ({})",
            narrowing_filter_names()
        )));
    }

    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as usize;
    let offset = match q.cursor.as_deref() {
        Some(c) => cursor::decode_offset_bounded(c, MAX_OFFSET)
            .ok_or_else(|| ApiError::bad_request("invalid cursor"))?,
        None => 0,
    };

    let mut params = SearchQueryParams {
        text: text.to_owned(),
        account_id: q.account_id,
        room_id: q.room_id,
        sender: q.sender,
        from_ts: q.from,
        to_ts: q.to,
        sort: q.sort,
        limit,
        offset,
    };

    // The index returns `(account_id, event_id)` keys; hydrate each from the store.
    // A hit whose row was deleted between indexing and now is skipped, not a 500,
    // and `get_event_if_joined` also drops hits in rooms the account has left/been
    // banned from (the M10 membership filter, ADR 0044). Because both can null out
    // whole index pages, we *fill* the response page: fetch further index pages
    // (bounded by `MAX_FILL_ROUNDS`) until we have `limit` surviving hits or the
    // index is exhausted — so a query dominated by left-room matches returns a
    // short-or-cursored page, never a run of empty ones.
    let mut results: Vec<SearchResultDto> = Vec::with_capacity(limit);
    let mut index_offset = offset;
    let mut total = 0usize;
    for _ in 0..MAX_FILL_ROUNDS {
        let deficit = limit - results.len();
        params.limit = deficit;
        params.offset = index_offset;
        let found = search.search(&params).await?;
        total = found.total;
        let got = found.hits.len();
        for hit in &found.hits {
            if let Some(row) = store
                .get_event_if_joined(hit.account_id, &hit.event_id)
                .await?
            {
                results.push(SearchResultDto {
                    event: EventDto::from_row(hit.account_id, row),
                    score: hit.score,
                });
            }
        }
        // Advance by the hits the index returned (pre-filter), so a dropped row
        // doesn't desync paging.
        index_offset = index_offset.saturating_add(got);
        // Stop once the page is full or the index is exhausted (a short page).
        if results.len() >= limit || got < deficit {
            break;
        }
    }

    // More pages remain while the consumed index offset is short of the total.
    let next_cursor = (index_offset < total).then(|| cursor::encode_offset(index_offset));

    Ok(ApiResponse::new(SearchPage {
        results,
        total,
        next_cursor,
    }))
}

fn has_narrowing_filter(q: &SearchQueryDto) -> bool {
    NARROWING_FILTERS.iter().any(|filter| filter.is_present(q))
}

fn narrowing_filter_names() -> String {
    NARROWING_FILTERS
        .iter()
        .map(|filter| filter.query_name())
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug, Clone, Copy)]
enum SearchFilter {
    AccountId,
    RoomId,
    Sender,
    From,
    To,
}

impl SearchFilter {
    fn query_name(self) -> &'static str {
        match self {
            Self::AccountId => "account_id",
            Self::RoomId => "room_id",
            Self::Sender => "sender",
            Self::From => "from",
            Self::To => "to",
        }
    }

    fn is_present(self, q: &SearchQueryDto) -> bool {
        match self {
            Self::AccountId => q.account_id.is_some(),
            Self::RoomId => q.room_id.as_deref().is_some_and(nonempty),
            Self::Sender => q.sender.as_deref().is_some_and(nonempty),
            Self::From => q.from.is_some(),
            Self::To => q.to.is_some(),
        }
    }
}

fn nonempty(value: &str) -> bool {
    !value.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_defaults_to_relevance_and_rejects_unknown_values() {
        let query: SearchQueryDto =
            serde_json::from_value(serde_json::json!({"q": "needle"})).unwrap();
        assert_eq!(query.sort, SearchSort::Relevance);
        for (value, expected) in [
            ("relevance", SearchSort::Relevance),
            ("newest", SearchSort::Newest),
            ("oldest", SearchSort::Oldest),
        ] {
            let query: SearchQueryDto =
                serde_json::from_value(serde_json::json!({"q": "needle", "sort": value})).unwrap();
            assert_eq!(query.sort, expected);
        }
        assert!(serde_json::from_value::<SearchQueryDto>(
            serde_json::json!({"q": "needle", "sort": "invalid"})
        )
        .is_err());
    }
}

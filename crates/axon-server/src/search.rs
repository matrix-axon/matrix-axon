//! The `axon search` subcommand (M9b) and the search-query adapter.
//!
//! The subcommand is the operator path for the full-text index. `reindex` clears
//! the index's seed-completion marker so the next server start rebuilds the corpus
//! from Postgres — it touches only the sidecar marker, never the live index, so it
//! is safe to run against a running server (which keeps serving and reseeds when it
//! next restarts).
//!
//! [`SearchAdapter`] is the binary-side glue that implements the API's
//! [`SearchQuery`](axon_api::SearchQuery) port over the `axon-search` Tantivy
//! index, keeping `axon-api` free of `tantivy`.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use axon_api::{SearchHit, SearchHits, SearchQuery, SearchQueryError, SearchQueryParams};
use axon_core::Config;
use axon_search::{SearchIndex, SearchParams};
use tokio::sync::Semaphore;

use crate::cli::SearchAction;

/// Run a `search` subcommand against the configured search index. Index-only: no
/// database connection, no sync engine, no HTTP listener.
pub fn run(action: SearchAction, config: &Config) -> anyhow::Result<()> {
    match action {
        SearchAction::Reindex => {
            SearchIndex::mark_for_reseed(&config.search.index_path)
                .context("clearing the search index seed marker")?;
            println!("Cleared search index; it will rebuild on next server start.");
            if !config.search.enabled {
                println!(
                    "Note: search is currently disabled (search.enabled = false); \
                     the rebuild will run once search is enabled and the server starts."
                );
            }
        }
    }
    Ok(())
}

/// Adapts the `axon-search` index onto the API's [`SearchQuery`] port. Holds the
/// read side of the index (the writer is owned by the indexing actor); queries run
/// on a blocking thread so a heavier search never stalls a runtime worker.
///
/// Two resource bounds gate the query path (ADR 0039, "Resource boundedness"): a
/// [`Semaphore`] caps concurrent searches so bearer-gated queries can't flood the
/// blocking pool (over-limit requests queue, they aren't rejected), and a per-query
/// timeout caps client-perceived latency. The concurrency permit is held by the
/// blocking task itself, so a timed-out search still counts against the cap until it
/// drains. Together with the handler's offset cap — which bounds each query's *work*
/// — they bound the endpoint's resource use.
pub struct SearchAdapter {
    index: Arc<SearchIndex>,
    /// Bounds concurrent in-flight searches.
    permits: Arc<Semaphore>,
    /// Per-query wall-clock budget.
    timeout: Duration,
}

impl SearchAdapter {
    /// Build an adapter over `index`, allowing at most `max_concurrent` searches at
    /// once and giving each `timeout` before it's abandoned with a `503`.
    pub fn new(index: Arc<SearchIndex>, max_concurrent: usize, timeout: Duration) -> Self {
        // `max(1)` so a misconfigured `0` doesn't deadlock every query.
        let permits = Arc::new(Semaphore::new(max_concurrent.max(1)));
        Self {
            index,
            permits,
            timeout,
        }
    }
}

#[async_trait]
impl SearchQuery for SearchAdapter {
    async fn search(&self, params: &SearchQueryParams) -> Result<SearchHits, SearchQueryError> {
        // Bound concurrency: acquire an *owned* permit and move it into the
        // blocking closure so it counts against the cap until the search actually
        // finishes — not just until this future returns. A timed-out closure can't
        // be cancelled (see below), so releasing the permit on the timeout branch
        // would let a still-running search escape the cap while a new request took
        // its place. The semaphore is never closed, so acquire only fails on a
        // poisoned runtime — treat as internal.
        let permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| SearchQueryError::Internal)?;

        let index = self.index.clone();
        let params = params.clone();
        let query = tokio::task::spawn_blocking(move || {
            // Held until the blocking search returns, then dropped here — this is
            // what keeps an abandoned (timed-out) search counting against the cap.
            let _permit = permit;
            let p = SearchParams {
                text: &params.text,
                account_id: params.account_id,
                room_id: params.room_id.as_deref(),
                sender: params.sender.as_deref(),
                from_ts: params.from_ts,
                to_ts: params.to_ts,
                sort: match params.sort {
                    axon_api::SearchSort::Relevance => axon_search::SearchSort::Relevance,
                    axon_api::SearchSort::Newest => axon_search::SearchSort::Newest,
                    axon_api::SearchSort::Oldest => axon_search::SearchSort::Oldest,
                },
                limit: params.limit,
                offset: params.offset,
            };
            index.search(&p)
        });

        // Bound latency. A timed-out `spawn_blocking` closure can't be cancelled, so
        // it runs to completion on the blocking pool — acceptable because the
        // handler's offset cap already bounds each query's work, and its permit
        // (moved into the closure above) keeps it counted against the concurrency
        // cap until it drains.
        let result = match tokio::time::timeout(self.timeout, query).await {
            Ok(joined) => joined,
            Err(_elapsed) => {
                tracing::warn!(timeout = ?self.timeout, "search query exceeded its timeout");
                return Err(SearchQueryError::Timeout);
            }
        };

        match result {
            Ok(Ok(results)) => Ok(SearchHits {
                hits: results
                    .hits
                    .into_iter()
                    .map(|h| SearchHit {
                        account_id: h.account_id,
                        event_id: h.event_id,
                        score: h.score,
                    })
                    .collect(),
                total: results.total,
            }),
            Ok(Err(axon_search::SearchError::BadQuery(msg))) => {
                Err(SearchQueryError::BadQuery(msg))
            }
            Ok(Err(err)) => {
                tracing::error!(error = %err, "search query failed");
                Err(SearchQueryError::Internal)
            }
            Err(err) => {
                tracing::error!(error = %err, "search query task panicked");
                Err(SearchQueryError::Internal)
            }
        }
    }
}

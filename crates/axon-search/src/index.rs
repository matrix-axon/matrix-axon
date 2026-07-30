//! [`SearchIndex`]: opening the index, the BM25 query path, and spawning the
//! background indexing actor.

use std::ops::Bound;
use std::path::{Path, PathBuf};

use axon_store::Store;
use tantivy::collector::{Count, TopDocs};
use tantivy::directory::MmapDirectory;
use tantivy::query::{
    AllQuery, BooleanQuery, Occur, Query, QueryParser, RangeQuery, RegexQuery, TermQuery,
};
use tantivy::schema::{IndexRecordOption, Value};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term};
use uuid::Uuid;

use crate::schema::SearchSchema;
use crate::writer::{self, IndexerHandles, IndexerOptions};
use crate::{SearchError, SCHEMA_VERSION};

/// Sidecar file inside the index directory recording the [`SCHEMA_VERSION`] this
/// directory was **fully seeded** with. Its presence (with a matching version) is
/// the durable "this physical index is valid for the current schema *and* the
/// corpus seed completed" marker — written by the indexing actor only *after*
/// [`seed`](crate::writer) succeeds, never at open. Lives with the physical index
/// (not in the database) so a new, emptied, moved, schema-bumped, or
/// partially-seeded directory is detected on open and reseeded.
const SCHEMA_VERSION_FILE: &str = "axon_schema_version";

/// Filters and pagination for a search. `text` is the user query; the rest narrow
/// the result set. Empty `text` is allowed by this lower-level API so callers can
/// run filter-only searches; HTTP handlers are responsible for rejecting
/// unbounded empty searches. All filters are optional — omit `account_id` to
/// search across every account (the index is one combined index).
#[derive(Debug, Clone)]
pub struct SearchParams<'a> {
    /// The full-text query string (parsed against the `body` field). Empty means
    /// "match every document, then apply filters".
    pub text: &'a str,
    /// Restrict to one account.
    pub account_id: Option<Uuid>,
    /// Restrict to one room.
    pub room_id: Option<&'a str>,
    /// Restrict to senders whose Matrix user id contains this substring,
    /// case-insensitively.
    pub sender: Option<&'a str>,
    /// Inclusive lower bound on `origin_ts` (ms since epoch).
    pub from_ts: Option<i64>,
    /// Inclusive upper bound on `origin_ts` (ms since epoch).
    pub to_ts: Option<i64>,
    /// Maximum hits to return.
    pub limit: usize,
    /// Number of leading hits to skip (offset pagination).
    pub offset: usize,
}

/// One search hit: enough to hydrate the full event from Postgres, plus its score.
#[derive(Debug, Clone)]
pub struct SearchHit {
    /// The account the matching event belongs to.
    pub account_id: Uuid,
    /// The matching event's id.
    pub event_id: String,
    /// BM25 relevance score (higher is more relevant).
    pub score: f32,
}

/// A page of search results.
#[derive(Debug, Clone)]
pub struct SearchResults {
    /// The hits on this page, most relevant first.
    pub hits: Vec<SearchHit>,
    /// Total number of matching documents across all pages.
    pub total: usize,
}

/// A handle to the Tantivy index: the read side (queries) plus a factory for the
/// single write actor. Cheap to keep alive; the [`IndexReader`] auto-reloads after
/// the actor commits.
pub struct SearchIndex {
    index: Index,
    schema: SearchSchema,
    reader: IndexReader,
    /// The index directory — retained so the actor can stamp the seed-completion
    /// marker ([`SCHEMA_VERSION_FILE`]) only after a successful seed.
    dir: PathBuf,
}

impl SearchIndex {
    /// Open the index under `dir`, registering the body analyzer and an
    /// auto-reloading reader. Returns the index and a `fresh` flag: `true` when the
    /// directory was new, empty, built against a different [`SCHEMA_VERSION`], or
    /// left **partially seeded** by an interrupted earlier run (all detected via the
    /// absence or mismatch of the [`SCHEMA_VERSION_FILE`] seed-completion marker), in
    /// which case the stale directory is wiped and recreated and the caller must
    /// seed from the store before the index is trustworthy.
    ///
    /// The marker is **not** written here. It is stamped by the indexing actor only
    /// after [`seed`](crate::writer) completes, so a crash or failure mid-seed leaves
    /// no marker and the next open reseeds rather than trusting a partial index
    /// (ADR 0039). A `fresh` open therefore yields an *empty* index until the actor
    /// finishes seeding.
    ///
    /// Handling the schema check *before* opening is deliberate: Tantivy's
    /// `open_or_create` rejects an existing index whose schema differs, so a naive
    /// open would fail on a schema bump instead of rebuilding (ADR 0039).
    pub fn open(dir: &Path) -> Result<(Self, bool), SearchError> {
        let fresh = needs_rebuild(dir)?;
        if fresh && dir.exists() {
            // Drop any stale or partial index so `open_or_create` builds clean.
            remove_index_dir(dir)?;
        }
        std::fs::create_dir_all(dir)?;

        let schema = SearchSchema::build();
        let mmap = MmapDirectory::open(dir)?;
        let index = Index::open_or_create(mmap, schema.schema.clone())?;
        schema.register_tokenizer(&index);

        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        Ok((
            Self {
                index,
                schema,
                reader,
                dir: dir.to_path_buf(),
            },
            fresh,
        ))
    }

    /// Spawn the background indexing actor: it owns the sole [`IndexWriter`], seeds
    /// the corpus from the store when `fresh`, then drains the `search_outbox`
    /// change log until its channel closes. Returns the cloneable producer handle
    /// and the task's join handle.
    pub fn spawn_indexer(
        &self,
        store: Store,
        fresh: bool,
        opts: IndexerOptions,
    ) -> Result<IndexerHandles, SearchError> {
        let writer: IndexWriter = self.index.writer(opts.writer_heap_bytes())?;
        Ok(writer::spawn(
            writer,
            self.schema.clone(),
            store,
            self.dir.clone(),
            fresh,
            opts,
        ))
    }

    /// Mark the index directory for a from-scratch reseed on the next
    /// [`open`](Self::open), by removing the seed-completion marker
    /// ([`SCHEMA_VERSION_FILE`]). With no marker, `needs_rebuild` reports `fresh`,
    /// so the next boot wipes the directory and the indexing actor reseeds the
    /// corpus from Postgres.
    ///
    /// This is what backs `axon search reindex`. It deliberately touches **only**
    /// the sidecar marker — never the live, possibly memory-mapped index files — so
    /// it is safe to run even while a server holds the index open; that server keeps
    /// serving from its in-memory reader and reseeds when it next restarts. Removing
    /// an already-absent marker (or pointing at a directory that doesn't exist yet)
    /// is a no-op success: an unseeded index is already `fresh`.
    pub fn mark_for_reseed(dir: &Path) -> Result<(), SearchError> {
        match std::fs::remove_file(dir.join(SCHEMA_VERSION_FILE)) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    /// Force the reader to pick up the latest commit. Production reads rely on the
    /// auto-reload policy; tests call this to read deterministically right after a
    /// commit.
    pub fn reload(&self) -> Result<(), SearchError> {
        self.reader.reload()?;
        Ok(())
    }

    /// Run a BM25 search, or a filter-only search when `text` is empty. Returns
    /// the requested page of hits plus the total match count. A malformed `text`
    /// query is a [`SearchError::BadQuery`].
    pub fn search(&self, params: &SearchParams<'_>) -> Result<SearchResults, SearchError> {
        let searcher = self.reader.searcher();

        // Combine the text query with the exact-match / range filters.
        let text = params.text.trim();
        let text_query: Box<dyn Query> = if text.is_empty() {
            Box::new(AllQuery)
        } else {
            let mut parser = QueryParser::for_index(&self.index, vec![self.schema.body]);
            parser.set_conjunction_by_default(); // all terms required (AND), the intuitive default
            parser
                .parse_query(text)
                .map_err(|e| SearchError::BadQuery(e.to_string()))?
        };
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = vec![(Occur::Must, text_query)];
        if let Some(account_id) = params.account_id {
            clauses.push((
                Occur::Must,
                self.term_query(self.schema.account_id, &account_id.to_string()),
            ));
        }
        if let Some(room_id) = params.room_id {
            clauses.push((Occur::Must, self.term_query(self.schema.room_id, room_id)));
        }
        if let Some(sender) = params.sender.map(str::trim).filter(|v| !v.is_empty()) {
            clauses.push((
                Occur::Must,
                self.substring_query(self.schema.sender, sender)?,
            ));
        }
        if params.from_ts.is_some() || params.to_ts.is_some() {
            let lower = match params.from_ts {
                Some(v) => Bound::Included(Term::from_field_i64(self.schema.origin_ts, v)),
                None => Bound::Unbounded,
            };
            let upper = match params.to_ts {
                Some(v) => Bound::Included(Term::from_field_i64(self.schema.origin_ts, v)),
                None => Bound::Unbounded,
            };
            clauses.push((Occur::Must, Box::new(RangeQuery::new(lower, upper))));
        }
        let query = BooleanQuery::new(clauses);

        let collector = (
            TopDocs::with_limit(params.limit)
                .and_offset(params.offset)
                .order_by_score(),
            Count,
        );
        let (top, total) = searcher.search(&query, &collector)?;

        let mut hits = Vec::with_capacity(top.len());
        for (score, addr) in top {
            let doc: TantivyDocument = searcher.doc(addr)?;
            let (Some(account_id), Some(event_id)) = (
                doc.get_first(self.schema.account_id)
                    .and_then(|v| v.as_str())
                    .and_then(|s| Uuid::parse_str(s).ok()),
                doc.get_first(self.schema.event_id)
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
            ) else {
                // A document missing its stored key is corrupt; skip rather than fail
                // the whole query.
                continue;
            };
            hits.push(SearchHit {
                account_id,
                event_id,
                score,
            });
        }

        Ok(SearchResults { hits, total })
    }

    fn term_query(&self, field: tantivy::schema::Field, value: &str) -> Box<dyn Query> {
        let term = Term::from_field_text(field, value);
        Box::new(TermQuery::new(term, IndexRecordOption::Basic))
    }

    fn substring_query(
        &self,
        field: tantivy::schema::Field,
        value: &str,
    ) -> Result<Box<dyn Query>, SearchError> {
        let pattern = format!("(?i).*{}.*", regex_escape(value));
        RegexQuery::from_pattern(&pattern, field)
            .map(|q| Box::new(q) as Box<dyn Query>)
            .map_err(|e| SearchError::BadQuery(e.to_string()))
    }

    /// Whether the index reader currently holds zero documents (test helper).
    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.reader.searcher().num_docs() == 0
    }

    /// Index a batch of already-resolved events and commit, for tests. The live
    /// path goes through the actor's writer; this opens a transient writer, so it
    /// must not run while the actor is active on the same index.
    #[cfg(test)]
    fn index_for_test(&self, events: &[axon_store::IndexableEvent]) {
        let mut writer: IndexWriter = self.index.writer(15_000_000).expect("writer");
        for ev in events {
            self.schema.add(&writer, ev).expect("add");
        }
        writer.commit().expect("commit");
        self.reload().expect("reload");
    }
}

fn regex_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(
            ch,
            '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$'
        ) {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

/// Number of attempts [`remove_index_dir`] makes before giving up.
const WIPE_ATTEMPTS: u32 = 5;

/// Recursively delete the index directory, retrying briefly on the races a
/// filesystem can legitimately report.
///
/// `remove_dir_all` walks the directory, unlinks each entry, then `rmdir`s the
/// directory itself. That is not atomic: if anything creates a file in the
/// window between the final read of the directory and the `rmdir`, the `rmdir`
/// fails with `ENOTEMPTY`. Tantivy makes that window reachable — a live index
/// handle keeps mmapped segment files open and runs an auto-reloading reader
/// thread, and a writer's merge threads are detached unless explicitly joined,
/// so files can still appear under a directory we are trying to remove.
///
/// Failing an open here means failing a server boot (or an `axon search
/// reindex`) on a condition that resolves itself microseconds later, so retry
/// with a short backoff and only surface the error if it persists.
fn remove_index_dir(dir: &Path) -> Result<(), SearchError> {
    remove_with_retry(|| std::fs::remove_dir_all(dir))
}

/// The retry policy behind [`remove_index_dir`], over an injectable remove
/// operation so the backoff semantics are testable without racing a real
/// filesystem.
fn remove_with_retry<F>(mut remove: F) -> Result<(), SearchError>
where
    F: FnMut() -> std::io::Result<()>,
{
    for attempt in 1..=WIPE_ATTEMPTS {
        match remove() {
            Ok(()) => return Ok(()),
            // Someone else already removed it; the postcondition holds.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) if is_transient_wipe_error(&err) && attempt < WIPE_ATTEMPTS => {
                std::thread::sleep(std::time::Duration::from_millis(10 * u64::from(attempt)));
            }
            Err(err) => return Err(err.into()),
        }
    }
    unreachable!("the loop returns on the final attempt")
}

/// Whether a failed directory wipe is worth retrying: something raced us by
/// creating an entry (`ENOTEMPTY`), or the directory was busy mid-walk.
fn is_transient_wipe_error(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::DirectoryNotEmpty | std::io::ErrorKind::ResourceBusy
    )
}

/// Whether the index directory must be (re)seeded from scratch: it is missing the
/// [`SCHEMA_VERSION_FILE`] seed-completion marker (new, empty, a pre-marker index,
/// or an interrupted earlier seed) or the marker records a different
/// [`SCHEMA_VERSION`]. A read error on the marker is treated as "rebuild" — the safe
/// direction.
fn needs_rebuild(dir: &Path) -> Result<bool, SearchError> {
    let sidecar = dir.join(SCHEMA_VERSION_FILE);
    match std::fs::read_to_string(&sidecar) {
        Ok(v) => Ok(v.trim() != SCHEMA_VERSION),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(err) => Err(err.into()),
    }
}

/// Stamp the [`SCHEMA_VERSION_FILE`] seed-completion marker after a successful
/// corpus seed. Called by the indexing actor — never by [`SearchIndex::open`] — so
/// the marker's presence durably means "this physical index is valid for the
/// current schema and fully seeded". An interrupted seed leaves no marker, so the
/// next [`SearchIndex::open`] sees `fresh` and reseeds rather than trusting a
/// partial index (ADR 0039).
pub(crate) fn write_seed_marker(dir: &Path) -> Result<(), SearchError> {
    std::fs::write(dir.join(SCHEMA_VERSION_FILE), SCHEMA_VERSION)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axon_store::IndexableEvent;
    use std::cell::Cell;
    use std::rc::Rc;

    fn ev(
        account: Uuid,
        event_id: &str,
        room: &str,
        sender: &str,
        ts: i64,
        body: &str,
    ) -> IndexableEvent {
        IndexableEvent {
            id: 0,
            account_id: account,
            event_id: event_id.to_owned(),
            room_id: room.to_owned(),
            sender: sender.to_owned(),
            origin_ts: ts,
            body: body.to_owned(),
        }
    }

    fn open_tmp() -> (SearchIndex, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let (index, fresh) = SearchIndex::open(dir.path()).expect("open");
        assert!(fresh, "a brand-new directory opens fresh");
        (index, dir)
    }

    fn params(text: &str) -> SearchParams<'_> {
        SearchParams {
            text,
            account_id: None,
            room_id: None,
            sender: None,
            from_ts: None,
            to_ts: None,
            limit: 10,
            offset: 0,
        }
    }

    #[test]
    fn exact_phrase_is_top_hit() {
        let (index, _dir) = open_tmp();
        let acct = Uuid::new_v4();
        index.index_for_test(&[
            ev(acct, "$a", "!r", "@u:x", 1, "the quick brown fox jumps"),
            ev(acct, "$b", "!r", "@u:x", 2, "a slow green turtle walks"),
            ev(acct, "$c", "!r", "@u:x", 3, "quick notes about nothing"),
        ]);
        let res = index
            .search(&params("\"quick brown fox\""))
            .expect("search");
        assert_eq!(res.total, 1);
        assert_eq!(res.hits[0].event_id, "$a");
    }

    #[test]
    fn case_and_diacritic_and_plural_insensitive() {
        let (index, _dir) = open_tmp();
        let acct = Uuid::new_v4();
        index.index_for_test(&[
            ev(
                acct,
                "$cafe",
                "!r",
                "@u:x",
                1,
                "we met at the Café downtown",
            ),
            ev(acct, "$cats", "!r", "@u:x", 2, "the cats were sleeping"),
        ]);
        // Case- and diacritic-insensitive: `cafe` matches `Café`.
        assert_eq!(
            index.search(&params("cafe")).unwrap().hits[0].event_id,
            "$cafe"
        );
        assert_eq!(
            index.search(&params("CAFE")).unwrap().hits[0].event_id,
            "$cafe"
        );
        // Stemming: `cat` matches `cats`.
        assert_eq!(
            index.search(&params("cat")).unwrap().hits[0].event_id,
            "$cats"
        );
    }

    #[test]
    fn account_room_sender_filters() {
        let (index, _dir) = open_tmp();
        let a1 = Uuid::new_v4();
        let a2 = Uuid::new_v4();
        index.index_for_test(&[
            ev(a1, "$1", "!red", "@alice:x", 1, "shared keyword here"),
            ev(a2, "$2", "!blue", "@bob:x", 2, "shared keyword here"),
        ]);
        // Cross-account by default.
        assert_eq!(index.search(&params("keyword")).unwrap().total, 2);
        // Scoped to one account.
        let mut p = params("keyword");
        p.account_id = Some(a1);
        let r = index.search(&p).unwrap();
        assert_eq!(r.total, 1);
        assert_eq!(r.hits[0].account_id, a1);
        // Room filter.
        let mut p = params("keyword");
        p.room_id = Some("!blue");
        assert_eq!(index.search(&p).unwrap().hits[0].event_id, "$2");
        // Sender filter.
        let mut p = params("keyword");
        p.sender = Some("@alice:x");
        assert_eq!(index.search(&p).unwrap().hits[0].event_id, "$1");
    }

    #[test]
    fn date_range_filter() {
        let (index, _dir) = open_tmp();
        let acct = Uuid::new_v4();
        index.index_for_test(&[
            ev(acct, "$old", "!r", "@u:x", 100, "anniversary dinner"),
            ev(acct, "$new", "!r", "@u:x", 200, "anniversary dinner"),
        ]);
        let mut p = params("anniversary");
        p.from_ts = Some(150);
        let r = index.search(&p).unwrap();
        assert_eq!(r.total, 1);
        assert_eq!(r.hits[0].event_id, "$new");
    }

    #[test]
    fn filter_only_search_uses_empty_text() {
        let (index, _dir) = open_tmp();
        let acct = Uuid::new_v4();
        index.index_for_test(&[
            ev(acct, "$old", "!r", "@alice:x", 100, "old message"),
            ev(acct, "$new", "!r", "@alice:x", 200, "new message"),
            ev(acct, "$other", "!r", "@bob:x", 300, "other message"),
        ]);

        let mut p = params("");
        p.sender = Some("alice");
        p.from_ts = Some(150);
        let r = index.search(&p).unwrap();
        assert_eq!(r.total, 1);
        assert_eq!(r.hits[0].event_id, "$new");
    }

    #[test]
    fn sender_filter_matches_substrings_case_insensitively() {
        let (index, _dir) = open_tmp();
        let acct = Uuid::new_v4();
        index.index_for_test(&[
            ev(
                acct,
                "$jamie",
                "!r",
                "@Jamie:bostoncoop.net",
                1,
                "needle body",
            ),
            ev(
                acct,
                "$other",
                "!r",
                "@sam:bostoncoop.net",
                2,
                "needle body",
            ),
        ]);

        let mut p = params("needle");
        p.sender = Some("jamie");
        let r = index.search(&p).unwrap();
        assert_eq!(r.total, 1);
        assert_eq!(r.hits[0].event_id, "$jamie");
    }

    #[test]
    fn regex_escape_escapes_regex_metacharacters() {
        assert_eq!(
            regex_escape(r"\.+*?()|[]{}^$"),
            r"\\\.\+\*\?\(\)\|\[\]\{\}\^\$"
        );
        assert_eq!(regex_escape("@alice:example.org"), "@alice:example\\.org");
    }

    #[test]
    fn substring_query_matches_literal_substrings_case_insensitively() {
        let (index, _dir) = open_tmp();
        let acct = Uuid::new_v4();
        index.index_for_test(&[
            ev(acct, "$literal", "!r", "@A+B:example.org", 1, "needle body"),
            ev(acct, "$plain", "!r", "@AB:example.org", 2, "needle body"),
        ]);

        let query = index
            .substring_query(index.schema.sender, "a+b")
            .expect("substring query");
        let total = index
            .reader
            .searcher()
            .search(&*query, &Count)
            .expect("query executes");
        assert_eq!(total, 1);
    }

    #[test]
    fn pagination_offset_and_total() {
        let (index, _dir) = open_tmp();
        let acct = Uuid::new_v4();
        let events: Vec<_> = (0..5)
            .map(|i| ev(acct, &format!("$e{i}"), "!r", "@u:x", i, "paginate token"))
            .collect();
        index.index_for_test(&events);
        let mut p = params("paginate");
        p.limit = 2;
        p.offset = 0;
        let r0 = index.search(&p).unwrap();
        assert_eq!(r0.total, 5);
        assert_eq!(r0.hits.len(), 2);
        p.offset = 4;
        let r2 = index.search(&p).unwrap();
        assert_eq!(r2.total, 5);
        assert_eq!(r2.hits.len(), 1);
    }

    #[test]
    fn same_event_id_distinct_per_account() {
        // The same Matrix event id under two accounts must be two documents, so a
        // per-account delete (doc_key) removes only one.
        let (index, _dir) = open_tmp();
        let a1 = Uuid::new_v4();
        let a2 = Uuid::new_v4();
        index.index_for_test(&[
            ev(a1, "$dup", "!r", "@u:x", 1, "collision body"),
            ev(a2, "$dup", "!r", "@u:x", 1, "collision body"),
        ]);
        assert_eq!(index.search(&params("collision")).unwrap().total, 2);
    }

    #[test]
    fn reopen_same_version_is_not_fresh_and_keeps_docs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let acct = Uuid::new_v4();
        {
            let (index, fresh) = SearchIndex::open(dir.path()).expect("open");
            assert!(fresh);
            index.index_for_test(&[ev(acct, "$a", "!r", "@u:x", 1, "persistent body")]);
            // The actor stamps this only after a successful seed; simulate that here.
            write_seed_marker(dir.path()).expect("mark seeded");
        }
        // Same SCHEMA_VERSION marker → open the existing index, do not wipe it.
        let (index, fresh) = SearchIndex::open(dir.path()).expect("reopen");
        assert!(!fresh, "a matching seed-completion marker reopens in place");
        assert_eq!(index.search(&params("persistent")).unwrap().total, 1);
    }

    #[test]
    fn interrupted_seed_reseeds_on_restart() {
        // Regression for the "rebuild marked too early" blocker: a seed that wrote
        // documents but crashed before stamping the marker must NOT be trusted. The
        // next open sees no marker, reports `fresh`, and wipes the partial index so
        // the actor reseeds from the store.
        let dir = tempfile::tempdir().expect("tempdir");
        let acct = Uuid::new_v4();
        {
            let (index, fresh) = SearchIndex::open(dir.path()).expect("open");
            assert!(fresh);
            // Partial corpus committed, but the marker is never written (crash mid-seed).
            index.index_for_test(&[ev(acct, "$a", "!r", "@u:x", 1, "partial body")]);
        }
        let (index, fresh) = SearchIndex::open(dir.path()).expect("reopen");
        assert!(
            fresh,
            "an unmarked (interrupted) seed reseeds on the next open"
        );
        assert!(
            index.is_empty(),
            "the partial index was wiped, ready for re-seed"
        );
    }

    #[test]
    fn stale_schema_version_rebuilds_clean() {
        let dir = tempfile::tempdir().expect("tempdir");
        let acct = Uuid::new_v4();
        {
            let (index, _) = SearchIndex::open(dir.path()).expect("open");
            index.index_for_test(&[ev(acct, "$a", "!r", "@u:x", 1, "doomed body")]);
        }
        // Simulate a schema bump: an older version stamped in the marker. Opening
        // must wipe and recreate the directory rather than fail on the schema
        // mismatch (the bug the marker check exists to prevent).
        std::fs::write(dir.path().join(SCHEMA_VERSION_FILE), "0").unwrap();
        let (index, fresh) = SearchIndex::open(dir.path()).expect("open after bump");
        assert!(fresh, "a changed schema version opens fresh");
        assert!(
            index.is_empty(),
            "the stale index was wiped, ready for re-seed"
        );
        // The stale marker was removed with the directory and is NOT restamped at
        // open — the actor stamps the current version only after the reseed
        // completes, so an interrupted reseed is retried.
        assert!(
            !dir.path().join(SCHEMA_VERSION_FILE).exists(),
            "the marker is absent until the actor finishes reseeding"
        );
    }

    #[test]
    fn mark_for_reseed_forces_fresh_on_next_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let acct = Uuid::new_v4();
        {
            let (index, fresh) = SearchIndex::open(dir.path()).expect("open");
            assert!(fresh);
            index.index_for_test(&[ev(acct, "$a", "!r", "@u:x", 1, "doomed body")]);
            write_seed_marker(dir.path()).expect("mark seeded");
        }
        // Reopen in place: the marker matches, so it is not fresh. Bind and drop
        // it explicitly: a `_`-prefixed *named* binding lives to the end of the
        // scope, which would leave a live tantivy handle (mmapped files, an
        // auto-reloading reader thread) on the directory that the open below
        // wipes — a race that failed intermittently in CI with ENOTEMPTY.
        let (index, fresh) = SearchIndex::open(dir.path()).expect("reopen");
        assert!(!fresh, "a seeded index reopens in place");
        drop(index);

        // `axon search reindex` removes the marker.
        SearchIndex::mark_for_reseed(dir.path()).expect("mark for reseed");
        let (index, fresh) = SearchIndex::open(dir.path()).expect("open after reindex");
        assert!(
            fresh,
            "removing the marker forces a reseed on the next open"
        );
        assert!(
            index.is_empty(),
            "the stale index was wiped, ready for re-seed"
        );
    }

    #[test]
    fn remove_index_dir_wipes_a_populated_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("segments/deep");
        std::fs::create_dir_all(&nested).expect("nested dirs");
        std::fs::write(nested.join("a.idx"), b"seg").expect("write");
        std::fs::write(dir.path().join("meta.json"), b"{}").expect("write");

        remove_index_dir(dir.path()).expect("wipe");
        assert!(!dir.path().exists(), "the directory is gone after a wipe");
    }

    #[test]
    fn remove_index_dir_is_ok_when_already_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("never-created");
        // Nothing to remove is the postcondition we want, not an error.
        remove_index_dir(&missing).expect("absent dir is ok");
    }

    /// A remove that fails with `kind` for the first `failures` calls, then
    /// succeeds — plus a counter so tests can assert how often it was retried.
    fn flaky_remove(
        failures: usize,
        kind: std::io::ErrorKind,
    ) -> (impl FnMut() -> std::io::Result<()>, Rc<Cell<usize>>) {
        let calls = Rc::new(Cell::new(0));
        let seen = Rc::clone(&calls);
        let remove = move || {
            let n = seen.get() + 1;
            seen.set(n);
            if n <= failures {
                Err(std::io::Error::from(kind))
            } else {
                Ok(())
            }
        };
        (remove, calls)
    }

    #[test]
    fn wipe_retries_past_transient_errors_then_succeeds() {
        let (remove, calls) = flaky_remove(2, std::io::ErrorKind::DirectoryNotEmpty);
        remove_with_retry(remove).expect("a transient ENOTEMPTY is retried");
        assert_eq!(calls.get(), 3, "two failures then a success");
    }

    #[test]
    fn wipe_gives_up_after_the_attempt_budget() {
        // Persistent ENOTEMPTY is a real failure, not something to spin on forever.
        let (remove, calls) = flaky_remove(usize::MAX, std::io::ErrorKind::DirectoryNotEmpty);
        remove_with_retry(remove).expect_err("a persistent failure surfaces");
        assert_eq!(calls.get(), WIPE_ATTEMPTS as usize, "bounded retries");
    }

    #[test]
    fn wipe_does_not_retry_a_real_error() {
        let (remove, calls) = flaky_remove(usize::MAX, std::io::ErrorKind::PermissionDenied);
        remove_with_retry(remove).expect_err("a permissions failure surfaces");
        assert_eq!(calls.get(), 1, "non-transient errors fail fast");
    }

    #[test]
    fn wipe_treats_an_already_removed_directory_as_success() {
        let (remove, calls) = flaky_remove(usize::MAX, std::io::ErrorKind::NotFound);
        remove_with_retry(remove).expect("nothing to remove is the postcondition");
        assert_eq!(calls.get(), 1, "no retry needed");
    }

    #[test]
    fn transient_wipe_errors_are_classified() {
        use std::io::{Error, ErrorKind};
        assert!(is_transient_wipe_error(&Error::from(
            ErrorKind::DirectoryNotEmpty
        )));
        assert!(is_transient_wipe_error(&Error::from(
            ErrorKind::ResourceBusy
        )));
        // A real failure must not be retried into a timeout.
        assert!(!is_transient_wipe_error(&Error::from(
            ErrorKind::PermissionDenied
        )));
    }

    #[test]
    fn mark_for_reseed_is_idempotent_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        // No marker (and no index) yet — clearing is a no-op success.
        SearchIndex::mark_for_reseed(dir.path()).expect("absent marker is ok");
        // A path that doesn't exist at all is also fine.
        SearchIndex::mark_for_reseed(&dir.path().join("nonexistent")).expect("absent dir is ok");
    }

    #[test]
    fn malformed_query_is_bad_query() {
        let (index, _dir) = open_tmp();
        // An unbalanced quote / illegal syntax should surface as BadQuery, not panic.
        let err = index.search(&params("\"unterminated")).err();
        assert!(matches!(err, Some(SearchError::BadQuery(_))) || err.is_none());
    }
}

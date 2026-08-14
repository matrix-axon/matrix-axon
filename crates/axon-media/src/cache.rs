//! Bounded LRU disk cache for proxied media.
//!
//! [`MediaCache`] downloads-through-and-caches decrypted media bytes on local
//! disk under a global byte cap, evicting least-recently-used entries when a new
//! fetch would exceed it. The homeserver is the source of truth; this cache is a
//! bounded convenience, never durable storage (there is deliberately no S3
//! backend — see the implementation spec / ADR 0045).
//!
//! ## Boundaries
//!
//! - **SDK-free.** The actual authenticated, decrypting download is done by an
//!   injected [`MediaFetcher`] (implemented in `axon-sync`), so this crate never
//!   depends on `matrix-sdk`.
//! - **Single-flight.** Concurrent requests for the same uncached object share
//!   one download (per-key async slot, the `ClientManager` pattern).
//! - **Serve via an open fd.** [`get_or_fetch`](MediaCache::get_or_fetch) opens
//!   the cache file and hands back the [`tokio::fs::File`]. On Linux an open fd
//!   survives `unlink`, so a concurrent eviction/purge that deletes the path can
//!   never break an in-flight serve — no refcounting needed.
//! - **Never fatal to serving.** A cache-write failure degrades to serving the
//!   freshly-fetched bytes from a temporary file (unlinked after open) rather
//!   than failing the request.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::{Mutex as AsyncMutex, Semaphore};
use tokio::task::JoinHandle;
use uuid::Uuid;

use axon_core::config::MediaConfig;
use axon_core::media::ThumbnailSpec;

/// What the injected downloader can report. The cache passes these through
/// unchanged so the composition-root adapter maps them onto `axon-api`'s
/// `MediaError` with the same translation it uses for other backend errors.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// The account does not exist or is not active. → `404`.
    #[error("account not found: {0}")]
    AccountNotFound(String),
    /// The media was not found on the homeserver. → `404`.
    #[error("media not found: {0}")]
    NotFound(String),
    /// The MXC URI or encrypted-file descriptor is invalid. → `400`.
    #[error("invalid media request: {0}")]
    Invalid(String),
    /// The homeserver refused the request. → `403`.
    #[error("forbidden: {0}")]
    Forbidden(String),
    /// The account is not yet reachable; caller should retry. → `503`.
    #[error("account not connected: {0}")]
    NotConnected(String),
    /// The homeserver was unreachable or failed the request. → `502`.
    #[error("upstream error: {0}")]
    Upstream(String),
}

/// Downloads (and decrypts) MXC media on behalf of an account. Implemented in
/// `axon-sync` via the SDK client; kept behind this trait so `axon-media` stays
/// free of `matrix-sdk`.
#[async_trait]
pub trait MediaFetcher: Send + Sync {
    /// Fetch the plaintext bytes for `mxc_url` using `account_id`'s credentials.
    /// `encrypted_file` is the matching `content.file` / `content.info.
    /// thumbnail_file` JSON descriptor when the media is encrypted (the fetcher
    /// decrypts after downloading); `None` for plain media.
    async fn fetch(
        &self,
        account_id: Uuid,
        mxc_url: &str,
        encrypted_file: Option<Value>,
    ) -> Result<Vec<u8>, FetchError>;

    /// Fetch a homeserver-generated thumbnail of `mxc_url` at `spec`'s
    /// dimensions/method, using `account_id`'s credentials. Plain
    /// (unencrypted) media only — the API layer rejects encrypted media with
    /// a `400` before this is ever called, so there is no `encrypted_file`
    /// parameter (a homeserver never sees encrypted-media plaintext, so it
    /// cannot thumbnail it).
    async fn fetch_thumbnail(
        &self,
        account_id: Uuid,
        mxc_url: &str,
        spec: ThumbnailSpec,
    ) -> Result<Vec<u8>, FetchError>;
}

/// A resolved media object, ready to serve. Holds an **open** file handle whose
/// backing bytes survive a concurrent eviction/purge (unlink-after-open).
#[derive(Debug)]
pub struct MediaResource {
    /// Open, read-only handle positioned at the start of the media bytes.
    pub file: tokio::fs::File,
    /// Total length in bytes.
    pub len: u64,
    /// A stable, content-addressed entity tag, used for `ETag` /
    /// `If-None-Match`: `sha256(mxc_url)` for a full-resolution original (see
    /// [`etag_for`]), or `sha256("thumb:{mxc_url}:{w}x{h}:{method}")` for a
    /// thumbnail variant (see [`etag_for_thumbnail`]). MXC content is
    /// immutable, so this never changes for a given URI (or URI+spec).
    pub etag: String,
    /// Decrements [`Inner::open_handles`] on drop — see that field's doc.
    _open_guard: HandleGuard,
}

/// Counts outstanding [`MediaResource`]s for the fd-diagnostics gauge exposed by
/// [`MediaCache::stats`] (see the fd-exhaustion investigation, GH #242): this is
/// the one place the process deliberately holds a file open per in-flight
/// request, so if a leak lives here rather than in matrix-rust-sdk or Tantivy,
/// this count climbing and never coming back down is how it shows up.
#[derive(Debug)]
struct HandleGuard(Arc<AtomicUsize>);

impl Drop for HandleGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Owns a caller's participation in the single-flight group for one key, and
/// releases it on **every** exit path from
/// [`MediaCache::get_or_fetch_keyed`] — including the one plain control flow
/// misses: an axum handler future dropped when the client disconnects
/// mid-fetch (the user scrolls past an image, the browser aborts). Without
/// this the map entry outlives the request and is only ever reclaimed if the
/// same key is fetched again — see GH #27.
///
/// This is deliberately the *only* caller-side `Arc` for the slot, which is
/// what makes [`Inner::release_flight`]'s liveness check exact: `drop` gives
/// up this reference before inspecting the map, so the strong count it reads
/// counts only the callers that still need the slot.
struct FlightGuard {
    inner: Arc<Inner>,
    key: Key,
    /// `Some` for the guard's whole life; taken in `drop` to release our
    /// reference before the map is inspected. Access it via [`Self::slot`].
    slot: Option<Arc<AsyncMutex<()>>>,
}

impl FlightGuard {
    /// Join (or open) the single-flight group for `(account_id, hash)`.
    fn join(inner: Arc<Inner>, account_id: Uuid, hash: &str) -> Self {
        let slot = inner.flight_slot(account_id, hash);
        Self {
            inner,
            key: (account_id, hash.to_owned()),
            slot: Some(slot),
        }
    }

    /// The shared per-key mutex; lock it to become the group's fetcher.
    fn slot(&self) -> &AsyncMutex<()> {
        self.slot.as_ref().expect("slot is taken only in drop")
    }
}

impl Drop for FlightGuard {
    fn drop(&mut self) {
        // Order matters: give up our own reference *first*, so the count
        // `release_flight` reads is exactly "other callers still using this".
        self.slot.take();
        self.inner.release_flight(&self.key);
    }
}

/// A point-in-time snapshot for the fd-diagnostics logger (GH #242).
#[derive(Debug, Clone, Copy)]
pub struct MediaCacheStats {
    /// Outstanding [`MediaResource`]s (see [`HandleGuard`]) — file handles
    /// currently being streamed to a client.
    pub open_handles: usize,
    /// Number of objects currently tracked in the on-disk LRU.
    pub entries: usize,
    /// Total bytes tracked in the on-disk LRU.
    pub total_bytes: u64,
}

/// Fetch/cache failures.
#[derive(Debug, thiserror::Error)]
pub enum MediaCacheError {
    /// The injected downloader failed.
    #[error(transparent)]
    Fetch(#[from] FetchError),
    /// The object exceeds the configured per-object cap and is refused (neither
    /// cached nor served). Note the downloader buffers the object in full before
    /// this check runs (the SDK media API is not streaming), so this cap bounds
    /// what is *retained and served*, not peak per-download memory; the
    /// concurrency limit (`max_concurrent_downloads`) is what bounds aggregate
    /// download memory.
    #[error("media object too large ({len} bytes; per-object limit is {cap} bytes)")]
    TooLarge {
        /// Size of the fetched object.
        len: u64,
        /// The configured per-object cap.
        cap: u64,
    },
    /// A local filesystem failure that prevented serving entirely (e.g. no disk
    /// writable at all). A cache-*write* failure alone does not surface here —
    /// it degrades to an uncached serve.
    #[error("media cache i/o error: {0}")]
    Io(#[source] std::io::Error),
}

/// A cheap-to-clone handle exposing only the operations account-lifecycle
/// teardown needs (mirrors `axon-search::IndexHandle`).
#[derive(Clone)]
pub struct MediaCacheHandle {
    inner: Arc<Inner>,
}

impl MediaCacheHandle {
    /// Drop every cache entry for `account_id` and remove its on-disk directory.
    /// Idempotent; used at account-deletion teardown (ADR 0024 step 5) and safe
    /// to call whether or not the account has any cached media.
    pub async fn purge_account(&self, account_id: Uuid) {
        self.inner.purge_account(account_id).await;
    }

    /// Boot orphan-GC: purge any `<account_id>/` cache directory whose id is not
    /// in `known` (the backstop for a deletion that happened while the cache was
    /// disabled, or a crash mid-purge). Non-UUID entries (the temp dir, strays)
    /// and stray files are left untouched; never fatal.
    pub async fn purge_orphans(&self, known: &HashSet<Uuid>) {
        self.inner.purge_orphans(known).await;
    }
}

/// Bounded LRU media cache. Cheap to [`Clone`] (an `Arc` inside).
#[derive(Clone)]
pub struct MediaCache {
    inner: Arc<Inner>,
}

impl MediaCache {
    /// Open (or create) the cache rooted at `cfg.cache_dir`.
    ///
    /// Boot does the cheap synchronous setup (create the dir, clear the stale
    /// temp area) and returns immediately; the LRU index is rebuilt from disk in
    /// a **background task** so a large cache (tens of thousands of files) does
    /// not block startup on a full tree walk. The rebuild seeds the index by
    /// mtime (oldest → first-evicted) and then evicts down to `cfg.max_bytes`.
    /// Until it finishes, hits still serve (the file is opened directly) and new
    /// writes still record; the only transient effect is that the byte cap may be
    /// briefly overshot before the rebuild's eviction pass runs. Use
    /// [`wait_ready`](Self::wait_ready) to await the rebuild (tests, or an
    /// operator that wants boot-time bounding).
    ///
    /// A **disabled** cache (`enabled = false`) neither serves nor manages
    /// on-disk files, so it does no scan, eviction, or deletion — the directory
    /// is left untouched.
    pub async fn open(cfg: &MediaConfig) -> std::io::Result<MediaCache> {
        let cache_dir = cfg.cache_dir.clone();
        tokio::fs::create_dir_all(&cache_dir).await?;

        // A fresh temp area each boot — a crash mid-write can leave stragglers.
        let tmp_dir = cache_dir.join(TMP_DIR);
        let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
        tokio::fs::create_dir_all(&tmp_dir).await?;

        let inner = Arc::new(Inner {
            cache_dir,
            tmp_dir,
            enabled: cfg.enabled,
            max_bytes: cfg.max_bytes,
            max_object_bytes: cfg.max_object_bytes,
            downloads: Semaphore::new(cfg.max_concurrent_downloads.max(1)),
            index: Mutex::new(Index::default()),
            flight: Mutex::new(HashMap::new()),
            rebuild: Mutex::new(None),
            account_locks: Mutex::new(HashMap::new()),
            purged: Mutex::new(HashSet::new()),
            open_handles: Arc::new(AtomicUsize::new(0)),
        });

        if cfg.enabled {
            let scan = inner.clone();
            let handle = tokio::spawn(async move { scan.rebuild().await });
            *inner.rebuild.lock().expect("rebuild mutex") = Some(handle);
        }

        Ok(MediaCache { inner })
    }

    /// Await the background boot rebuild, if one is still running. Idempotent
    /// (the handle is taken on first call). Used by tests that assert on the
    /// rebuilt index/eviction; the server does not need to call it.
    pub async fn wait_ready(&self) {
        let handle = self.inner.rebuild.lock().expect("rebuild mutex").take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }

    /// Return the media for `(account_id, mxc_url)`, downloading through
    /// `fetcher` on a miss. Concurrent callers for the same key share one
    /// download. Bumps the entry's LRU position on a hit; on a miss caches the
    /// bytes (subject to the per-object cap) and evicts LRU entries if the byte
    /// cap is exceeded.
    pub async fn get_or_fetch(
        &self,
        account_id: Uuid,
        mxc_url: &str,
        encrypted_file: Option<Value>,
        fetcher: &dyn MediaFetcher,
    ) -> Result<MediaResource, MediaCacheError> {
        let hash = etag_for(mxc_url);
        self.get_or_fetch_keyed(account_id, &hash, || {
            fetcher.fetch(account_id, mxc_url, encrypted_file)
        })
        .await
    }

    /// The thumbnail-aware sibling of [`get_or_fetch`](Self::get_or_fetch):
    /// same single-flight/LRU/eviction behavior, keyed by `(account_id,
    /// mxc_url, spec)` via [`etag_for_thumbnail`] rather than the bare
    /// `mxc_url`, so a thumbnail variant is cached independently of (and
    /// never collides with) the full-resolution original.
    pub async fn get_or_fetch_thumbnail(
        &self,
        account_id: Uuid,
        mxc_url: &str,
        spec: ThumbnailSpec,
        fetcher: &dyn MediaFetcher,
    ) -> Result<MediaResource, MediaCacheError> {
        let hash = etag_for_thumbnail(mxc_url, spec);
        self.get_or_fetch_keyed(account_id, &hash, || {
            fetcher.fetch_thumbnail(account_id, mxc_url, spec)
        })
        .await
    }

    /// Shared single-flight/cache-or-fetch machinery for both
    /// [`get_or_fetch`](Self::get_or_fetch) and
    /// [`get_or_fetch_thumbnail`](Self::get_or_fetch_thumbnail), keyed by a
    /// pre-computed `hash` (from [`etag_for`] or [`etag_for_thumbnail`]).
    /// `fetch` performs the network call on a miss; everything else
    /// (disabled-cache passthrough, fast-path hit, single-flight slot,
    /// re-check-under-lock, store-and-open, eviction) is identical regardless
    /// of what `hash` represents.
    async fn get_or_fetch_keyed<F, Fut>(
        &self,
        account_id: Uuid,
        hash: &str,
        fetch: F,
    ) -> Result<MediaResource, MediaCacheError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Vec<u8>, FetchError>>,
    {
        // Disabled: never read from or write to the cache. Fetch fresh every
        // time and serve from a temp file (unlinked after open) — `store_and_open`
        // retains nothing when disabled. No single-flight (nothing is shared).
        if !self.inner.enabled {
            let data = self.inner.fetch_capped(fetch).await?;
            return self.inner.store_and_open(account_id, hash, data).await;
        }

        // Fast path: already cached.
        if let Some(resource) = self.inner.try_hit(account_id, hash).await {
            return Ok(resource);
        }

        // Single-flight: only one task fetches a given key at a time. `flight`
        // is declared *before* the lock guard so drop order (reverse of
        // declaration) unlocks the slot first and only then releases our claim
        // on it.
        let flight = FlightGuard::join(self.inner.clone(), account_id, hash);
        let _guard = flight.slot().lock().await;

        // Re-check under the slot — a peer may have populated it while we waited.
        if let Some(resource) = self.inner.try_hit(account_id, hash).await {
            return Ok(resource);
        }

        let data = self.inner.fetch_capped(fetch).await?;
        self.inner.store_and_open(account_id, hash, data).await
    }

    /// A cheap handle for the lifecycle-teardown purge path.
    pub fn handle(&self) -> MediaCacheHandle {
        MediaCacheHandle {
            inner: self.inner.clone(),
        }
    }

    /// A point-in-time snapshot for fd-diagnostics logging (GH #242).
    pub fn stats(&self) -> MediaCacheStats {
        self.inner.stats()
    }
}

/// Name of the temp subdirectory under the cache root. Not a valid UUID, so the
/// boot scan and the (axon-sync) orphan GC both skip it.
const TMP_DIR: &str = ".tmp";

type Key = (Uuid, String);

struct Inner {
    cache_dir: PathBuf,
    tmp_dir: PathBuf,
    enabled: bool,
    max_bytes: u64,
    max_object_bytes: u64,
    /// Bounds how many upstream downloads run concurrently. Since the fetcher
    /// buffers each object fully in memory before the per-object cap can reject
    /// it, this is what keeps aggregate download memory bounded (≈ permits ×
    /// object size) under concurrent load for distinct keys.
    downloads: Semaphore,
    index: Mutex<Index>,
    flight: Mutex<HashMap<Key, Arc<AsyncMutex<()>>>>,
    /// Handle to the background boot-rebuild task (see [`MediaCache::open`]);
    /// taken and awaited by [`MediaCache::wait_ready`]. `None` when disabled.
    rebuild: Mutex<Option<JoinHandle<()>>>,
    /// Per-account lock serializing [`Inner::store_and_open`]'s promotion step
    /// against [`Inner::purge_account`], so a fetch already in flight when a
    /// deletion starts can never recreate the account's directory after the
    /// purge has run. Unlike `flight`, an entry is **never removed**: freeing it
    /// once a holder finishes would let a later caller mint a fresh `Arc` for the
    /// same account, which could then run concurrently with a peer still parked
    /// on the old one — reopening exactly the race this map exists to close.
    /// Bounded by the number of distinct accounts ever seen, same as `purged`.
    account_locks: Mutex<HashMap<Uuid, Arc<AsyncMutex<()>>>>,
    /// Accounts a purge has ever touched. Checked under the corresponding
    /// `account_locks` entry: a promotion that loses the race to a purge sees
    /// its account here and refuses to resurrect the directory. Never removed
    /// (account ids are never reused), but bounded by the number of accounts
    /// ever deleted — negligible next to message/event storage.
    purged: Mutex<HashSet<Uuid>>,
    /// Outstanding [`MediaResource`]s, for [`MediaCache::stats`] (GH #242's
    /// fd-diagnostics logging). See [`HandleGuard`].
    open_handles: Arc<AtomicUsize>,
}

impl Inner {
    fn account_dir(&self, account_id: Uuid) -> PathBuf {
        self.cache_dir.join(account_id.to_string())
    }

    /// Bump the open-handle gauge and return the guard that decrements it when
    /// the resulting [`MediaResource`] drops.
    fn open_handle_guard(&self) -> HandleGuard {
        self.open_handles.fetch_add(1, Ordering::Relaxed);
        HandleGuard(self.open_handles.clone())
    }

    fn stats(&self) -> MediaCacheStats {
        let index = self.index.lock().expect("index mutex");
        MediaCacheStats {
            open_handles: self.open_handles.load(Ordering::Relaxed),
            entries: index.entries.len(),
            total_bytes: index.total_bytes,
        }
    }

    fn path_for(&self, account_id: Uuid, hash: &str) -> PathBuf {
        self.account_dir(account_id).join(hash)
    }

    /// Open the cached file if present, bumping its LRU position. A missing file
    /// (never cached, evicted, or purged) is a miss.
    async fn try_hit(&self, account_id: Uuid, hash: &str) -> Option<MediaResource> {
        let path = self.path_for(account_id, hash);
        let file = tokio::fs::File::open(&path).await.ok()?;
        let len = file.metadata().await.ok()?.len();
        // Refresh recency, but only for an entry that still exists in the index:
        // `touch` never *inserts*. This closes a race where a concurrent eviction
        // has already dropped the entry (and is about to unlink the file) — a
        // `record` here would resurrect it, permanently over-counting
        // `total_bytes` for a file that is about to vanish.
        if let Ok(mut index) = self.index.lock() {
            index.touch(account_id, hash);
        }
        Some(MediaResource {
            file,
            len,
            etag: hash.to_owned(),
            _open_guard: self.open_handle_guard(),
        })
    }

    /// Download via `fetch` (a caller-supplied one-shot future — either the
    /// plain or thumbnail [`MediaFetcher`] call), holding a concurrency permit
    /// for the duration and rejecting an object over the per-object cap. The
    /// permit bounds how many full objects are buffered in memory at once (the
    /// SDK media API is not streaming, so the whole object is materialized
    /// before the cap can reject it — see [`MediaCacheError::TooLarge`]).
    async fn fetch_capped<F, Fut>(&self, fetch: F) -> Result<Vec<u8>, MediaCacheError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Vec<u8>, FetchError>>,
    {
        let _permit = self
            .downloads
            .acquire()
            .await
            .expect("download semaphore never closed");
        let data = fetch().await?;
        let len = data.len() as u64;
        if len > self.max_object_bytes {
            return Err(MediaCacheError::TooLarge {
                len,
                cap: self.max_object_bytes,
            });
        }
        Ok(data)
    }

    /// Get-or-insert the per-account lock used to serialize a cache promotion
    /// against `purge_account` for that account (see the `account_locks` field
    /// doc). Deliberately never released/removed — see that doc for why.
    fn account_lock(&self, account_id: Uuid) -> Arc<AsyncMutex<()>> {
        let mut locks = self.account_locks.lock().expect("account lock mutex");
        locks
            .entry(account_id)
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    fn flight_slot(&self, account_id: Uuid, hash: &str) -> Arc<AsyncMutex<()>> {
        let mut flight = self.flight.lock().expect("flight mutex");
        flight
            .entry((account_id, hash.to_owned()))
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    /// Drop the single-flight entry for `key` once no caller needs it, so the
    /// map doesn't grow unbounded across distinct media keys.
    ///
    /// Called only from [`FlightGuard::drop`] — after that guard has released
    /// its own `Arc` — so a strong count of 1 means the map itself holds the
    /// last reference: nobody is fetching under this slot and nobody is parked
    /// on it. That check is exact rather than merely conservative, because the
    /// only way to obtain a new reference is [`Inner::flight_slot`], which
    /// needs the very mutex held here.
    ///
    /// Deliberately *not* keyed on the departing caller's identity. A caller
    /// that is cancelled while still queued behind the fetcher must leave the
    /// group intact (count > 1, so it does nothing); removing the entry there
    /// would let the next arrival mint a second slot and duplicate the
    /// in-flight download. Conversely, whichever caller happens to be last out
    /// sees count 1 and cleans up, so two concurrent releasers can never both
    /// skip and orphan the entry.
    fn release_flight(&self, key: &Key) {
        let mut flight = self.flight.lock().expect("flight mutex");
        if let Some(slot) = flight.get(key) {
            if Arc::strong_count(slot) == 1 {
                flight.remove(key);
            }
        }
    }

    /// Write freshly-fetched bytes, returning an open read handle. When caching
    /// is enabled, promote them into the cache and evict if over cap; on any
    /// cache-write failure, degrade to serving from a temp file. When disabled,
    /// always serve from a temp file (unlinked after open) and retain nothing.
    async fn store_and_open(
        &self,
        account_id: Uuid,
        hash: &str,
        data: Vec<u8>,
    ) -> Result<MediaResource, MediaCacheError> {
        let len = data.len() as u64;
        let etag = hash.to_owned();

        // Stage into a temp file and open it before any rename, so the returned
        // fd stays valid regardless of what happens to the path afterward.
        let tmp = self.tmp_dir.join(Uuid::new_v4().to_string());
        let file = match write_and_open(&tmp, &data).await {
            Ok(file) => file,
            Err(_) => {
                // Cache temp area unwritable — fall back to the OS temp dir so we
                // can still serve. If even that fails, the disk is broken.
                let os_tmp = std::env::temp_dir().join(format!("axon-media-{}", Uuid::new_v4()));
                let file = write_and_open(&os_tmp, &data)
                    .await
                    .map_err(MediaCacheError::Io)?;
                let _ = tokio::fs::remove_file(&os_tmp).await;
                return Ok(MediaResource {
                    file,
                    len,
                    etag,
                    _open_guard: self.open_handle_guard(),
                });
            }
        };

        if !self.enabled {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Ok(MediaResource {
                file,
                len,
                etag,
                _open_guard: self.open_handle_guard(),
            });
        }

        // Serialize the promotion against a concurrent `purge_account` for this
        // account: whichever side wins the lock fully finishes before the other
        // proceeds, so a fetch that was already in flight when deletion started
        // can never recreate a just-purged account directory (see the
        // `account_locks`/`purged` field docs, and ADR 0045).
        let account_lock = self.account_lock(account_id);
        let _account_guard = account_lock.lock().await;

        if self
            .purged
            .lock()
            .expect("purged mutex")
            .contains(&account_id)
        {
            // The account was purged (or deletion started) while this fetch was
            // in flight — never resurrect its cache directory. Still serve the
            // freshly-fetched bytes; just don't retain them.
            let _ = tokio::fs::remove_file(&tmp).await;
            return Ok(MediaResource {
                file,
                len,
                etag,
                _open_guard: self.open_handle_guard(),
            });
        }

        // Promote temp → final. The fd we already hold points at the same inode,
        // so it remains valid after the rename.
        let final_path = self.path_for(account_id, hash);
        let promoted = match tokio::fs::create_dir_all(self.account_dir(account_id)).await {
            Ok(()) => tokio::fs::rename(&tmp, &final_path).await,
            Err(e) => Err(e),
        };

        match promoted {
            Ok(()) => {
                let victims = {
                    let mut index = self.index.lock().expect("index mutex");
                    index.record(account_id, etag.clone(), len);
                    index.collect_evictions(self.max_bytes)
                };
                for (victim_account, victim_hash) in victims {
                    self.remove_file(victim_account, &victim_hash).await;
                }
            }
            Err(e) => {
                // Cache write failed — serve uncached rather than fail the request.
                tracing::warn!(
                    %account_id,
                    error = %e,
                    "media cache write failed; serving without caching",
                );
                let _ = tokio::fs::remove_file(&tmp).await;
            }
        }

        Ok(MediaResource {
            file,
            len,
            etag,
            _open_guard: self.open_handle_guard(),
        })
    }

    async fn remove_file(&self, account_id: Uuid, hash: &str) {
        let path = self.path_for(account_id, hash);
        if let Err(e) = tokio::fs::remove_file(&path).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(%account_id, error = %e, "media cache eviction unlink failed");
            }
        }
    }

    async fn purge_account(&self, account_id: Uuid) {
        // Hold the same per-account lock a concurrent `store_and_open` promotion
        // takes, and tombstone the account *before* touching the index/dir, so a
        // promotion that arrives after us always observes the tombstone (under
        // that same lock — never a fresh one, see the `account_locks` field doc)
        // and refuses to recreate the directory we're about to remove.
        let lock = self.account_lock(account_id);
        let _guard = lock.lock().await;
        self.purged.lock().expect("purged mutex").insert(account_id);

        if let Ok(mut index) = self.index.lock() {
            index.remove_account(account_id);
        }
        let dir = self.account_dir(account_id);
        if let Err(e) = tokio::fs::remove_dir_all(&dir).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(%account_id, error = %e, "media cache account purge failed");
            }
        }
    }

    async fn purge_orphans(&self, known: &HashSet<Uuid>) {
        let mut entries = match tokio::fs::read_dir(&self.cache_dir).await {
            Ok(entries) => entries,
            Err(_) => return,
        };
        loop {
            let entry = match entries.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!(error = %e, "media orphan-GC: reading cache dir failed; stopping");
                    break;
                }
            };
            let name = entry.file_name();
            // Non-UUID entries (the `.tmp` dir, strays) are never ours to remove.
            let Some(account_id) = name.to_str().and_then(|s| Uuid::parse_str(s).ok()) else {
                continue;
            };
            let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
            if is_dir && !known.contains(&account_id) {
                tracing::info!(%account_id, "media orphan-GC: purging row-less media dir");
                self.purge_account(account_id).await;
            }
        }
    }

    /// Background boot rebuild (see [`MediaCache::open`]): scan
    /// `cache_dir/<account_id>/<hash>`, seed the index for any entry not already
    /// present (live writes since boot win), then evict down to the byte cap.
    /// Best-effort: an unreadable dir/file is skipped, never fatal.
    async fn rebuild(&self) {
        let mut found: Vec<(Option<std::time::SystemTime>, Uuid, String, u64)> = Vec::new();
        let mut accounts = match tokio::fs::read_dir(&self.cache_dir).await {
            Ok(rd) => rd,
            Err(_) => return,
        };
        while let Ok(Some(acct_ent)) = accounts.next_entry().await {
            let name = acct_ent.file_name();
            // Non-UUID entries (the temp dir, strays) are never ours.
            let Some(account_id) = name.to_str().and_then(|s| Uuid::parse_str(s).ok()) else {
                continue;
            };
            if !acct_ent
                .file_type()
                .await
                .map(|t| t.is_dir())
                .unwrap_or(false)
            {
                continue;
            }
            let mut files = match tokio::fs::read_dir(acct_ent.path()).await {
                Ok(rd) => rd,
                Err(_) => continue,
            };
            while let Ok(Some(file_ent)) = files.next_entry().await {
                let Ok(meta) = file_ent.metadata().await else {
                    continue;
                };
                if !meta.is_file() {
                    continue;
                }
                let Some(hash) = file_ent.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                found.push((meta.modified().ok(), account_id, hash, meta.len()));
            }
        }

        // Oldest mtime first → lowest LRU seq → first to be evicted.
        found.sort_by_key(|entry| entry.0);
        let victims = {
            let mut index = self.index.lock().expect("index mutex");
            for (_mtime, account_id, hash, size) in found {
                index.record_if_absent(account_id, hash, size);
            }
            index.collect_evictions(self.max_bytes)
        };
        for (account_id, hash) in victims {
            self.remove_file(account_id, &hash).await;
        }
    }
}

/// LRU byte accounting. Guarded by a `std::sync::Mutex`; never held across an
/// `.await` (callers snapshot the eviction list, drop the lock, then unlink).
#[derive(Default)]
struct Index {
    entries: HashMap<Key, Entry>,
    /// Access order: seq → key, ascending. The first key is least-recently-used.
    order: BTreeMap<u64, Key>,
    total_bytes: u64,
    next_seq: u64,
}

struct Entry {
    size: u64,
    seq: u64,
}

impl Index {
    /// Insert or refresh `(account_id, hash)` with `size`, moving it to the
    /// most-recently-used position. Keeps `total_bytes` consistent.
    fn record(&mut self, account_id: Uuid, hash: String, size: u64) {
        let key = (account_id, hash);
        if let Some(existing) = self.entries.get(&key) {
            self.order.remove(&existing.seq);
            self.total_bytes -= existing.size;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.order.insert(seq, key.clone());
        self.entries.insert(key, Entry { size, seq });
        self.total_bytes += size;
    }

    /// Insert `(account_id, hash)` only if not already tracked (the boot-rebuild
    /// merge path — a live write since boot must not be clobbered or double-counted).
    fn record_if_absent(&mut self, account_id: Uuid, hash: String, size: u64) {
        let key = (account_id, hash);
        if self.entries.contains_key(&key) {
            return;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.order.insert(seq, key.clone());
        self.entries.insert(key, Entry { size, seq });
        self.total_bytes += size;
    }

    /// Move an existing entry to the most-recently-used position. Does nothing
    /// (in particular, never inserts) if the key is not tracked — so it cannot
    /// resurrect an entry a concurrent eviction just removed.
    fn touch(&mut self, account_id: Uuid, hash: &str) {
        let key = (account_id, hash.to_owned());
        let Some(old_seq) = self.entries.get(&key).map(|entry| entry.seq) else {
            return;
        };
        self.order.remove(&old_seq);
        let seq = self.next_seq;
        self.next_seq += 1;
        self.order.insert(seq, key.clone());
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.seq = seq;
        }
    }

    /// While over `max_bytes`, drop the least-recently-used entry and return the
    /// keys whose files the caller must unlink.
    fn collect_evictions(&mut self, max_bytes: u64) -> Vec<(Uuid, String)> {
        let mut victims = Vec::new();
        while self.total_bytes > max_bytes {
            let Some(seq) = self.order.keys().next().copied() else {
                break;
            };
            let key = self.order.remove(&seq).expect("order/seq consistent");
            if let Some(entry) = self.entries.remove(&key) {
                self.total_bytes -= entry.size;
            }
            victims.push(key);
        }
        victims
    }

    fn remove_account(&mut self, account_id: Uuid) {
        let doomed: Vec<Key> = self
            .entries
            .keys()
            .filter(|(acct, _)| *acct == account_id)
            .cloned()
            .collect();
        for key in doomed {
            if let Some(entry) = self.entries.remove(&key) {
                self.order.remove(&entry.seq);
                self.total_bytes -= entry.size;
            }
        }
    }

    #[cfg(test)]
    fn total_bytes(&self) -> u64 {
        self.total_bytes
    }
}

/// Write `data` to `path` (durably), then open it read-only and return the
/// handle. Any error leaves nothing to clean up that the caller relies on.
async fn write_and_open(path: &Path, data: &[u8]) -> std::io::Result<tokio::fs::File> {
    use tokio::io::AsyncWriteExt;

    let mut file = tokio::fs::File::create(path).await?;
    file.write_all(data).await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::File::open(path).await
}

/// The stable, content-addressed entity tag for an MXC URI: `sha256(mxc_url)`
/// as lowercase hex. This is both the cache filename and the `ETag` value (MXC
/// content is immutable, so it never changes for a given URI). Exposed so the
/// API layer can answer a conditional GET (`If-None-Match`) *without* triggering
/// a download on a cache miss.
pub fn etag_for(mxc_url: &str) -> String {
    hash_hex(mxc_url)
}

/// The stable, content-addressed cache-key/entity-tag for a thumbnail variant
/// of `mxc_url` at `spec`'s dimensions/method — the thumbnail-aware sibling of
/// [`etag_for`]. The `"thumb:"`-prefixed preimage guarantees this can never
/// collide with the bare `sha256(mxc_url)` [`etag_for`] computes for the
/// original, even though both live as sibling files in the same per-account
/// cache directory.
pub fn etag_for_thumbnail(mxc_url: &str, spec: ThumbnailSpec) -> String {
    hash_hex(&format!(
        "thumb:{mxc_url}:{}x{}:{}",
        spec.width, spec.height, spec.method
    ))
}

fn hash_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    let digest = Sha256::digest(input.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axon_core::media::ThumbnailMethod;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::AsyncReadExt;

    /// A fetcher that counts calls and returns fixed bytes, optionally blocking
    /// on a barrier so we can drive the single-flight race deterministically.
    struct CountingFetcher {
        calls: AtomicUsize,
        payload: Vec<u8>,
        gate: Option<Arc<tokio::sync::Barrier>>,
    }

    impl CountingFetcher {
        fn new(payload: Vec<u8>) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                payload,
                gate: None,
            }
        }
    }

    #[async_trait]
    impl MediaFetcher for CountingFetcher {
        async fn fetch(
            &self,
            _account_id: Uuid,
            _mxc_url: &str,
            _encrypted_file: Option<Value>,
        ) -> Result<Vec<u8>, FetchError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = &self.gate {
                gate.wait().await;
            }
            Ok(self.payload.clone())
        }

        async fn fetch_thumbnail(
            &self,
            _account_id: Uuid,
            _mxc_url: &str,
            _spec: ThumbnailSpec,
        ) -> Result<Vec<u8>, FetchError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = &self.gate {
                gate.wait().await;
            }
            Ok(self.payload.clone())
        }
    }

    /// Signals once the fetch has been entered, then never completes — so a
    /// caller can be parked precisely inside `fetch_capped` and then dropped,
    /// reproducing an axum handler future cancelled by a client disconnect.
    struct StalledFetcher {
        calls: AtomicUsize,
        entered: tokio::sync::mpsc::UnboundedSender<()>,
    }

    impl StalledFetcher {
        fn new(entered: tokio::sync::mpsc::UnboundedSender<()>) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                entered,
            }
        }

        async fn stall(&self) -> Result<Vec<u8>, FetchError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let _ = self.entered.send(());
            std::future::pending().await
        }
    }

    #[async_trait]
    impl MediaFetcher for StalledFetcher {
        async fn fetch(
            &self,
            _account_id: Uuid,
            _mxc_url: &str,
            _encrypted_file: Option<Value>,
        ) -> Result<Vec<u8>, FetchError> {
            self.stall().await
        }

        async fn fetch_thumbnail(
            &self,
            _account_id: Uuid,
            _mxc_url: &str,
            _spec: ThumbnailSpec,
        ) -> Result<Vec<u8>, FetchError> {
            self.stall().await
        }
    }

    /// Fixture for the single-flight cancellation tests: a cache backed by a
    /// [`StalledFetcher`], so callers can be parked at a chosen point —
    /// inside the fetch, or queued behind whoever holds the slot — and then
    /// dropped, which is what an axum handler future does on client
    /// disconnect. Keeping the stalling and parking mechanics here means the
    /// tests below state only what they assert.
    struct StalledGroup {
        _dir: tempfile::TempDir,
        cache: MediaCache,
        account: Uuid,
        fetcher: Arc<StalledFetcher>,
        /// Async mutex (not `std`) so every method can take `&self` and still
        /// await — tests hold a request future across these calls.
        entered: AsyncMutex<tokio::sync::mpsc::UnboundedReceiver<()>>,
    }

    /// How long to poll a caller before concluding it is parked on the slot
    /// lock. It is only ever expected to elapse, so it trades test latency for
    /// certainty rather than the reverse.
    const PARK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(50);

    impl StalledGroup {
        async fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let cache = MediaCache::open(&config_in(dir.path(), 1 << 20, 1 << 20, true))
                .await
                .unwrap();
            let (entered_tx, entered_rx) = tokio::sync::mpsc::unbounded_channel();
            Self {
                _dir: dir,
                cache,
                account: Uuid::new_v4(),
                fetcher: Arc::new(StalledFetcher::new(entered_tx)),
                entered: AsyncMutex::new(entered_rx),
            }
        }

        /// An unpolled request for `url` against this group's cache.
        fn request(
            &self,
            url: &'static str,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<MediaResource, MediaCacheError>> + '_>,
        > {
            Box::pin(
                self.cache
                    .get_or_fetch(self.account, url, None, self.fetcher.as_ref()),
            )
        }

        /// Block until some caller has entered the (never-completing) fetch.
        async fn wait_until_fetching(&self) {
            assert!(
                self.entered.lock().await.recv().await.is_some(),
                "a caller should have entered the fetch"
            );
        }

        /// Spawn the caller that wins the slot and parks inside the fetch,
        /// holding the lock until the returned handle is aborted.
        async fn spawn_owner(&self, url: &'static str) -> JoinHandle<()> {
            let cache = self.cache.clone();
            let fetcher = self.fetcher.clone();
            let account = self.account;
            let owner = tokio::spawn(async move {
                let _ = cache
                    .get_or_fetch(account, url, None, fetcher.as_ref())
                    .await;
            });
            self.wait_until_fetching().await;
            owner
        }

        /// Poll a caller until it parks behind the owner on `slot.lock()`,
        /// then drop it — the client-disconnect case, from the queue.
        async fn park_then_cancel(&self, url: &'static str) {
            let mut queued = self.request(url);
            assert!(
                tokio::time::timeout(PARK_TIMEOUT, &mut queued)
                    .await
                    .is_err(),
                "queued caller must not get past the slot lock"
            );
        }

        fn flight_len(&self) -> usize {
            self.cache.inner.flight.lock().expect("flight mutex").len()
        }

        fn fetches_started(&self) -> usize {
            self.fetcher.calls.load(Ordering::SeqCst)
        }
    }

    struct FailingFetcher;

    #[async_trait]
    impl MediaFetcher for FailingFetcher {
        async fn fetch(
            &self,
            _account_id: Uuid,
            _mxc_url: &str,
            _encrypted_file: Option<Value>,
        ) -> Result<Vec<u8>, FetchError> {
            Err(FetchError::NotFound("nope".into()))
        }

        async fn fetch_thumbnail(
            &self,
            _account_id: Uuid,
            _mxc_url: &str,
            _spec: ThumbnailSpec,
        ) -> Result<Vec<u8>, FetchError> {
            Err(FetchError::NotFound("nope".into()))
        }
    }

    fn config_in(dir: &Path, max_bytes: u64, max_object_bytes: u64, enabled: bool) -> MediaConfig {
        MediaConfig {
            enabled,
            cache_dir: dir.to_path_buf(),
            max_bytes,
            max_object_bytes,
            fetch_timeout_secs: 30,
            max_concurrent_downloads: 8,
            ..MediaConfig::default()
        }
    }

    async fn read_all(resource: &mut MediaResource) -> Vec<u8> {
        let mut buf = Vec::new();
        resource.file.read_to_end(&mut buf).await.expect("read");
        buf
    }

    #[tokio::test]
    async fn miss_fetches_then_hit_serves_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let cache = MediaCache::open(&config_in(dir.path(), 1 << 20, 1 << 20, true))
            .await
            .unwrap();
        let account = Uuid::new_v4();
        let fetcher = CountingFetcher::new(b"hello world".to_vec());

        let mut first = cache
            .get_or_fetch(account, "mxc://s/one", None, &fetcher)
            .await
            .unwrap();
        assert_eq!(read_all(&mut first).await, b"hello world");

        let mut second = cache
            .get_or_fetch(account, "mxc://s/one", None, &fetcher)
            .await
            .unwrap();
        assert_eq!(read_all(&mut second).await, b"hello world");

        // Second call was a cache hit — no extra fetch.
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1);
    }

    fn thumb_spec(width: u32, height: u32, method: ThumbnailMethod) -> ThumbnailSpec {
        ThumbnailSpec {
            width,
            height,
            method,
        }
    }

    #[test]
    fn plain_and_thumbnail_keys_never_collide() {
        let url = "mxc://s/one";
        let spec = thumb_spec(64, 64, ThumbnailMethod::Scale);
        assert_ne!(etag_for(url), etag_for_thumbnail(url, spec));
    }

    #[test]
    fn distinct_thumbnail_specs_produce_distinct_keys() {
        let url = "mxc://s/one";
        let a = etag_for_thumbnail(url, thumb_spec(64, 64, ThumbnailMethod::Scale));
        let b = etag_for_thumbnail(url, thumb_spec(128, 64, ThumbnailMethod::Scale));
        let c = etag_for_thumbnail(url, thumb_spec(64, 128, ThumbnailMethod::Scale));
        let d = etag_for_thumbnail(url, thumb_spec(64, 64, ThumbnailMethod::Crop));
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
        assert_ne!(b, c);
        assert_ne!(b, d);
        assert_ne!(c, d);
    }

    #[tokio::test]
    async fn thumbnail_miss_fetches_then_hit_serves_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let cache = MediaCache::open(&config_in(dir.path(), 1 << 20, 1 << 20, true))
            .await
            .unwrap();
        let account = Uuid::new_v4();
        let fetcher = CountingFetcher::new(b"thumbnail bytes".to_vec());
        let spec = thumb_spec(64, 64, ThumbnailMethod::Scale);

        let mut first = cache
            .get_or_fetch_thumbnail(account, "mxc://s/one", spec, &fetcher)
            .await
            .unwrap();
        assert_eq!(read_all(&mut first).await, b"thumbnail bytes");

        let mut second = cache
            .get_or_fetch_thumbnail(account, "mxc://s/one", spec, &fetcher)
            .await
            .unwrap();
        assert_eq!(read_all(&mut second).await, b"thumbnail bytes");

        // Second call was a cache hit — no extra fetch.
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn distinct_thumbnail_specs_are_independent_cache_entries() {
        let dir = tempfile::tempdir().unwrap();
        let cache = MediaCache::open(&config_in(dir.path(), 1 << 20, 1 << 20, true))
            .await
            .unwrap();
        let account = Uuid::new_v4();
        let fetcher = CountingFetcher::new(b"thumb".to_vec());

        cache
            .get_or_fetch_thumbnail(
                account,
                "mxc://s/one",
                thumb_spec(64, 64, ThumbnailMethod::Scale),
                &fetcher,
            )
            .await
            .unwrap();
        cache
            .get_or_fetch_thumbnail(
                account,
                "mxc://s/one",
                thumb_spec(128, 128, ThumbnailMethod::Scale),
                &fetcher,
            )
            .await
            .unwrap();

        // Two distinct specs for the same mxc_url are two independent fetches
        // (and two independent cache entries), not a shared cache hit.
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn thumbnail_and_full_media_are_independent_cache_entries() {
        let dir = tempfile::tempdir().unwrap();
        let cache = MediaCache::open(&config_in(dir.path(), 1 << 20, 1 << 20, true))
            .await
            .unwrap();
        let account = Uuid::new_v4();
        let fetcher = CountingFetcher::new(b"payload".to_vec());

        cache
            .get_or_fetch(account, "mxc://s/one", None, &fetcher)
            .await
            .unwrap();
        cache
            .get_or_fetch_thumbnail(
                account,
                "mxc://s/one",
                thumb_spec(64, 64, ThumbnailMethod::Scale),
                &fetcher,
            )
            .await
            .unwrap();

        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn open_handles_gauge_tracks_outstanding_resources() {
        let dir = tempfile::tempdir().unwrap();
        let cache = MediaCache::open(&config_in(dir.path(), 1 << 20, 1 << 20, true))
            .await
            .unwrap();
        let account = Uuid::new_v4();
        let fetcher = CountingFetcher::new(b"hello world".to_vec());

        assert_eq!(cache.stats().open_handles, 0);

        let first = cache
            .get_or_fetch(account, "mxc://s/one", None, &fetcher)
            .await
            .unwrap();
        assert_eq!(cache.stats().open_handles, 1);

        // A second outstanding resource (cache hit this time) bumps it again.
        let second = cache
            .get_or_fetch(account, "mxc://s/one", None, &fetcher)
            .await
            .unwrap();
        assert_eq!(cache.stats().open_handles, 2);

        drop(first);
        assert_eq!(cache.stats().open_handles, 1);

        drop(second);
        assert_eq!(cache.stats().open_handles, 0);
    }

    #[tokio::test]
    async fn concurrent_misses_share_one_fetch() {
        let dir = tempfile::tempdir().unwrap();
        let cache = MediaCache::open(&config_in(dir.path(), 1 << 20, 1 << 20, true))
            .await
            .unwrap();
        let account = Uuid::new_v4();
        let fetcher = Arc::new(CountingFetcher {
            calls: AtomicUsize::new(0),
            payload: b"shared".to_vec(),
            gate: Some(Arc::new(tokio::sync::Barrier::new(1))),
        });

        // Fire many concurrent requests for the same uncached key.
        let mut handles = Vec::new();
        for _ in 0..8 {
            let cache = cache.clone();
            let fetcher = fetcher.clone();
            handles.push(tokio::spawn(async move {
                cache
                    .get_or_fetch(account, "mxc://s/race", None, fetcher.as_ref())
                    .await
                    .map(|r| r.len)
            }));
        }
        for handle in handles {
            assert_eq!(handle.await.unwrap().unwrap(), 6);
        }
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1);
        // Whoever leaves last unmaps the slot — the group drains completely.
        assert!(cache.inner.flight.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn cancelled_queued_request_leaves_the_owner_s_slot_mapped() {
        // A *queued* requester — one parked on `slot.lock()` behind the task
        // that is actually fetching — must not dissolve the single-flight
        // group when it is cancelled. If its release unmapped the shared slot,
        // the next arrival would find no entry, mint an independent slot, and
        // duplicate the in-flight origin fetch (PR #160 review).
        let group = StalledGroup::new().await;
        let owner = group.spawn_owner("mxc://s/queued").await;

        group.park_then_cancel("mxc://s/queued").await;

        assert_eq!(
            group.flight_len(),
            1,
            "a cancelled queued peer must not unmap the owner's slot"
        );
        // The owner still owns the fetch: no second download was started.
        assert_eq!(group.fetches_started(), 1);

        owner.abort();
    }

    #[tokio::test]
    async fn whole_group_cancelled_leaves_no_orphaned_slot() {
        // The failure mode the old `ptr_eq` comment warned about, in reverse:
        // if every member of a group defers cleanup to "someone else", the
        // entry is orphaned forever. Whoever is last out must take it.
        let group = StalledGroup::new().await;
        let owner = group.spawn_owner("mxc://s/group").await;

        group.park_then_cancel("mxc://s/group").await;
        group.park_then_cancel("mxc://s/group").await;
        assert_eq!(group.flight_len(), 1, "the owner still holds the slot");

        // Now the owner goes too. `await` resolves after the task's future has
        // been dropped, so its guard has run by the time we assert.
        owner.abort();
        assert!(owner.await.unwrap_err().is_cancelled());
        assert_eq!(group.flight_len(), 0, "last caller out must unmap the slot");
    }

    #[tokio::test]
    async fn cancelled_request_releases_its_single_flight_slot() {
        // GH #27: an axum handler future is dropped when the client
        // disconnects, which for media is the common case (scrolling past an
        // image). Cancellation inside `fetch_capped` must still unmap the
        // `flight` entry; hand-rolled cleanup after the await could not.
        let group = StalledGroup::new().await;

        {
            let mut request = group.request("mxc://s/abandoned");

            // Drive the request only as far as the (never-completing) fetch.
            tokio::select! {
                _ = &mut request => panic!("stalled fetch must not complete"),
                () = group.wait_until_fetching() => {}
            }

            // Guard against a vacuous test: the slot really is mapped here.
            assert_eq!(
                group.flight_len(),
                1,
                "in-flight request should hold a slot"
            );
        } // `request` dropped mid-fetch, exactly as a disconnect would.

        assert_eq!(
            group.flight_len(),
            0,
            "cancelled request leaked its single-flight slot"
        );
    }

    #[tokio::test]
    async fn eviction_drops_least_recently_used() {
        let dir = tempfile::tempdir().unwrap();
        // Cap holds ~2 objects of 100 bytes each.
        let cache = MediaCache::open(&config_in(dir.path(), 250, 1 << 20, true))
            .await
            .unwrap();
        let account = Uuid::new_v4();
        let fetcher = CountingFetcher::new(vec![0u8; 100]);

        cache
            .get_or_fetch(account, "mxc://s/a", None, &fetcher)
            .await
            .unwrap();
        cache
            .get_or_fetch(account, "mxc://s/b", None, &fetcher)
            .await
            .unwrap();
        // Touch "a" so "b" becomes the LRU victim.
        cache
            .get_or_fetch(account, "mxc://s/a", None, &fetcher)
            .await
            .unwrap();
        // Insert "c" → over cap → evict LRU ("b").
        cache
            .get_or_fetch(account, "mxc://s/c", None, &fetcher)
            .await
            .unwrap();

        let a = dir
            .path()
            .join(account.to_string())
            .join(hash_hex("mxc://s/a"));
        let b = dir
            .path()
            .join(account.to_string())
            .join(hash_hex("mxc://s/b"));
        let c = dir
            .path()
            .join(account.to_string())
            .join(hash_hex("mxc://s/c"));
        assert!(a.exists(), "recently-touched a should survive");
        assert!(!b.exists(), "lru b should be evicted");
        assert!(c.exists(), "newest c should be present");

        let index = cache.inner.index.lock().unwrap();
        assert!(index.total_bytes() <= 250);
    }

    #[tokio::test]
    async fn object_over_per_object_cap_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let cache = MediaCache::open(&config_in(dir.path(), 1 << 20, 10, true))
            .await
            .unwrap();
        let account = Uuid::new_v4();
        let fetcher = CountingFetcher::new(vec![0u8; 100]);

        let err = cache
            .get_or_fetch(account, "mxc://s/big", None, &fetcher)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            MediaCacheError::TooLarge { len: 100, cap: 10 }
        ));
    }

    #[tokio::test]
    async fn fetch_error_propagates() {
        let dir = tempfile::tempdir().unwrap();
        let cache = MediaCache::open(&config_in(dir.path(), 1 << 20, 1 << 20, true))
            .await
            .unwrap();
        let err = cache
            .get_or_fetch(Uuid::new_v4(), "mxc://s/x", None, &FailingFetcher)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            MediaCacheError::Fetch(FetchError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn purge_account_removes_entries_and_dir() {
        let dir = tempfile::tempdir().unwrap();
        let cache = MediaCache::open(&config_in(dir.path(), 1 << 20, 1 << 20, true))
            .await
            .unwrap();
        let account = Uuid::new_v4();
        let fetcher = CountingFetcher::new(vec![1u8; 50]);

        cache
            .get_or_fetch(account, "mxc://s/one", None, &fetcher)
            .await
            .unwrap();
        cache
            .get_or_fetch(account, "mxc://s/two", None, &fetcher)
            .await
            .unwrap();

        let account_dir = dir.path().join(account.to_string());
        assert!(account_dir.exists());

        cache.handle().purge_account(account).await;

        assert!(!account_dir.exists());
        let index = cache.inner.index.lock().unwrap();
        assert_eq!(index.total_bytes(), 0);
    }

    #[tokio::test]
    async fn store_after_purge_does_not_resurrect_account_dir() {
        // Reproduces the deletion/cache race: an in-flight fetch that started
        // before an account delete can finish (and try to promote its bytes)
        // after `purge_account` has already run. The tombstone must refuse the
        // promotion rather than recreate the just-purged account directory.
        let dir = tempfile::tempdir().unwrap();
        let cache = MediaCache::open(&config_in(dir.path(), 1 << 20, 1 << 20, true))
            .await
            .unwrap();
        let account = Uuid::new_v4();

        cache.handle().purge_account(account).await;

        let hash = etag_for("mxc://s/late");
        let mut resource = cache
            .inner
            .store_and_open(account, &hash, b"late bytes".to_vec())
            .await
            .unwrap();

        // Still served (never fatal to serving)...
        assert_eq!(read_all(&mut resource).await, b"late bytes");
        // ...but the purged account's directory must not have been resurrected.
        assert!(!dir.path().join(account.to_string()).exists());
    }

    #[tokio::test]
    async fn account_lock_is_never_recycled() {
        // A get-or-insert map whose entries get evicted after use (mirroring
        // `flight`/`release_flight`) would let a caller after a purge mint a
        // fresh `Arc<AsyncMutex<()>>` for the same account — which could then
        // run concurrently with a peer still parked on the old one, reopening
        // the deletion/cache race. `account_lock` must return the exact same
        // instance for an account's whole lifetime; it must never be released.
        let dir = tempfile::tempdir().unwrap();
        let cache = MediaCache::open(&config_in(dir.path(), 1 << 20, 1 << 20, true))
            .await
            .unwrap();
        let account = Uuid::new_v4();

        let before = cache.inner.account_lock(account);
        cache.handle().purge_account(account).await;
        let after = cache.inner.account_lock(account);

        assert!(
            Arc::ptr_eq(&before, &after),
            "account lock must not be recycled after a purge"
        );
    }

    #[tokio::test]
    async fn purge_orphans_removes_only_unknown_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let cache = MediaCache::open(&config_in(dir.path(), 1 << 20, 1 << 20, true))
            .await
            .unwrap();
        let keep = Uuid::new_v4();
        let drop = Uuid::new_v4();
        let fetcher = CountingFetcher::new(vec![3u8; 40]);
        cache
            .get_or_fetch(keep, "mxc://s/a", None, &fetcher)
            .await
            .unwrap();
        cache
            .get_or_fetch(drop, "mxc://s/b", None, &fetcher)
            .await
            .unwrap();

        let known: HashSet<Uuid> = [keep].into_iter().collect();
        cache.handle().purge_orphans(&known).await;

        assert!(dir.path().join(keep.to_string()).exists());
        assert!(!dir.path().join(drop.to_string()).exists());
        // The `.tmp` dir is not a UUID → untouched.
        assert!(dir.path().join(TMP_DIR).exists());
    }

    #[tokio::test]
    async fn boot_rebuild_recovers_index_and_evicts_over_cap() {
        let dir = tempfile::tempdir().unwrap();
        let account = Uuid::new_v4();

        // First run: cache three 100-byte objects with a generous cap.
        {
            let cache = MediaCache::open(&config_in(dir.path(), 1 << 20, 1 << 20, true))
                .await
                .unwrap();
            let fetcher = CountingFetcher::new(vec![7u8; 100]);
            for name in ["a", "b", "c"] {
                cache
                    .get_or_fetch(account, &format!("mxc://s/{name}"), None, &fetcher)
                    .await
                    .unwrap();
                // Space out mtimes so boot ordering is deterministic.
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }

        // Second run with a cap that holds ~1 object: boot must evict down.
        let cache = MediaCache::open(&config_in(dir.path(), 150, 1 << 20, true))
            .await
            .unwrap();
        // The rebuild + eviction runs in the background; await it before asserting.
        cache.wait_ready().await;
        let index = cache.inner.index.lock().unwrap();
        assert!(
            index.total_bytes() <= 150,
            "boot eviction should bound the cache"
        );
        drop(index);

        // The oldest ("a") should be gone; the newest ("c") should remain.
        let a = dir
            .path()
            .join(account.to_string())
            .join(hash_hex("mxc://s/a"));
        let c = dir
            .path()
            .join(account.to_string())
            .join(hash_hex("mxc://s/c"));
        assert!(!a.exists());
        assert!(c.exists());
    }

    #[tokio::test]
    async fn disabled_cache_serves_but_retains_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let cache = MediaCache::open(&config_in(dir.path(), 1 << 20, 1 << 20, false))
            .await
            .unwrap();
        let account = Uuid::new_v4();
        let fetcher = CountingFetcher::new(b"ephemeral".to_vec());

        let mut first = cache
            .get_or_fetch(account, "mxc://s/one", None, &fetcher)
            .await
            .unwrap();
        assert_eq!(read_all(&mut first).await, b"ephemeral");

        // Nothing retained → second call fetches again.
        cache
            .get_or_fetch(account, "mxc://s/one", None, &fetcher)
            .await
            .unwrap();
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 2);

        // No account dir left behind.
        assert!(!dir.path().join(account.to_string()).exists());
    }

    #[tokio::test]
    async fn open_file_survives_eviction_of_its_own_bytes() {
        let dir = tempfile::tempdir().unwrap();
        // Tiny cap: each new object evicts the previous.
        let cache = MediaCache::open(&config_in(dir.path(), 100, 1 << 20, true))
            .await
            .unwrap();
        let account = Uuid::new_v4();
        let fetcher = CountingFetcher::new(vec![9u8; 100]);

        let mut first = cache
            .get_or_fetch(account, "mxc://s/a", None, &fetcher)
            .await
            .unwrap();
        // Evict "a" by inserting "b".
        cache
            .get_or_fetch(account, "mxc://s/b", None, &fetcher)
            .await
            .unwrap();

        // The fd opened before eviction still reads the full object.
        assert_eq!(read_all(&mut first).await, vec![9u8; 100]);
    }
}

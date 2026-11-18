//! Streamed downloads through the cache (ADR 0139 phase 1).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::stream::{self, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use uuid::Uuid;

use axon_core::config::MediaConfig;
use axon_core::media::ThumbnailSpec;

use super::{FetchError, MediaBody, MediaCache, MediaCacheError, MediaFetcher};
use crate::stream::{GrowingMedia, MediaStream, VERDICT_TAIL};

const KIB: usize = 1024;
const TAIL: usize = VERDICT_TAIL as usize;

type Chunk = Result<Bytes, FetchError>;

/// A fetcher whose streamed body the test feeds by hand: each
/// [`fetch_stream`](MediaFetcher::fetch_stream) call takes the next queued
/// script, and closing the script's sender is the body's clean end.
struct ScriptedFetcher {
    calls: AtomicUsize,
    scripts: Mutex<Vec<(Option<u64>, mpsc::UnboundedReceiver<Chunk>)>>,
}

impl ScriptedFetcher {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            scripts: Mutex::new(Vec::new()),
        })
    }

    /// Queue one upstream response declaring `len`, returning its body feed.
    fn script(&self, len: Option<u64>) -> mpsc::UnboundedSender<Chunk> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.scripts.lock().unwrap().push((len, rx));
        tx
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl MediaFetcher for ScriptedFetcher {
    /// Buffered media: a 1 MiB object, to fill the cache.
    async fn fetch(&self, _: Uuid, _: &str, _: Option<Value>) -> Result<Vec<u8>, FetchError> {
        Ok(vec![0; 1 << 20])
    }

    async fn fetch_thumbnail(
        &self,
        _: Uuid,
        _: &str,
        _: ThumbnailSpec,
    ) -> Result<Vec<u8>, FetchError> {
        unreachable!("streaming tests fetch no thumbnails")
    }

    async fn fetch_stream(
        &self,
        _: Uuid,
        _: &str,
        _: Option<Value>,
    ) -> Result<MediaStream, FetchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut scripts = self.scripts.lock().unwrap();
        assert!(!scripts.is_empty(), "an unscripted stream was opened");
        let (len, rx) = scripts.remove(0);
        let body = stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|c| (c, rx)) });
        Ok(MediaStream {
            len,
            body: body.boxed(),
        })
    }
}

/// A 64 MiB cache with 1 MiB buffered objects, so the stream limit and the
/// reservation room are both 63 MiB unless a test narrows them.
fn stream_config(dir: &std::path::Path) -> MediaConfig {
    MediaConfig {
        enabled: true,
        cache_dir: dir.to_path_buf(),
        max_bytes: 64 << 20,
        max_object_bytes: 1 << 20,
        fetch_timeout_secs: 30,
        stall_timeout_secs: 30,
        max_concurrent_streams: 4,
        ..MediaConfig::default()
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    cache: MediaCache,
    fetcher: Arc<ScriptedFetcher>,
    account: Uuid,
}

impl Harness {
    async fn new() -> Self {
        Self::with(|_| {}).await
    }

    async fn with(tweak: impl FnOnce(&mut MediaConfig)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = stream_config(dir.path());
        tweak(&mut cfg);
        let cache = MediaCache::open(&cfg).await.unwrap();
        cache.wait_ready().await;
        Self {
            _dir: dir,
            cache,
            fetcher: ScriptedFetcher::new(),
            account: Uuid::new_v4(),
        }
    }

    async fn get(
        &self,
        url: &str,
        descriptor: Option<Value>,
    ) -> Result<MediaBody, MediaCacheError> {
        self.cache
            .get_or_stream(self.account, url, descriptor, self.fetcher.as_ref())
            .await
    }

    async fn growing(&self, url: &str) -> GrowingMedia {
        match self.get(url, None).await.expect("stream opens") {
            MediaBody::Growing(media) => media,
            MediaBody::Complete(_) => panic!("expected a download in flight"),
        }
    }

    /// Wait until no streamed download is in flight.
    async fn settle(&self) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !self.cache.inner.streaming.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the download task should finish");
    }

    fn reserved(&self) -> u64 {
        self.cache.inner.index.lock().unwrap().reserved
    }
}

fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

fn send(tx: &mpsc::UnboundedSender<Chunk>, bytes: &[u8]) {
    tx.send(Ok(Bytes::copy_from_slice(bytes)))
        .expect("body feed open");
}

/// Read a body to its end, or to its first error.
async fn collect(
    mut body: futures_util::stream::BoxStream<'static, std::io::Result<Bytes>>,
) -> (Vec<u8>, Option<std::io::Error>) {
    let mut out = Vec::new();
    while let Some(item) = body.next().await {
        match item {
            Ok(chunk) => out.extend_from_slice(&chunk),
            Err(error) => return (out, Some(error)),
        }
    }
    (out, None)
}

/// Read until `want` bytes have arrived, failing the test if they do not
/// arrive promptly.
async fn read_exactly(
    body: &mut futures_util::stream::BoxStream<'static, std::io::Result<Bytes>>,
    want: usize,
) -> Vec<u8> {
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while out.len() < want {
            let chunk = body.next().await.expect("body open").expect("no error");
            out.extend_from_slice(&chunk);
        }
    })
    .await
    .expect("bytes should be readable");
    out
}

/// Assert the body yields nothing more for a short while.
async fn assert_blocked(
    body: &mut futures_util::stream::BoxStream<'static, std::io::Result<Bytes>>,
) {
    assert!(
        tokio::time::timeout(Duration::from_millis(100), body.next())
            .await
            .is_err(),
        "the body should be waiting"
    );
}

#[tokio::test]
async fn a_range_is_served_while_the_download_runs_then_the_object_is_cached() {
    let h = Harness::new().await;
    let data = payload(256 * KIB);
    let feed = h.fetcher.script(Some(data.len() as u64));

    let media = h.growing("mxc://s/video").await;
    assert_eq!(media.known_len(), Some(data.len() as u64));

    // Half the file arrives; a range inside it is answered in full before
    // the download finishes.
    send(&feed, &data[..128 * KIB]);
    let (head, error) = collect(media.into_body(0, Some(16 * KIB as u64))).await;
    assert!(error.is_none());
    assert_eq!(head, data[..16 * KIB]);

    // A second request joins the same download rather than starting another.
    let joined = h.growing("mxc://s/video").await;
    send(&feed, &data[128 * KIB..]);
    drop(feed);
    let (all, error) = collect(joined.into_body(0, None)).await;
    assert!(error.is_none());
    assert_eq!(all, data);

    h.settle().await;
    assert_eq!(h.reserved(), 0, "the reservation became the cache entry");
    match h.get("mxc://s/video", None).await.unwrap() {
        MediaBody::Complete(resource) => assert_eq!(resource.len, data.len() as u64),
        MediaBody::Growing(_) => panic!("a verified download should be a cache hit"),
    }
    assert_eq!(h.fetcher.calls(), 1);
}

#[tokio::test]
async fn the_last_64_kib_wait_for_the_verdict() {
    let h = Harness::new().await;
    let data = payload(192 * KIB);
    let feed = h.fetcher.script(Some(data.len() as u64));

    let mut body = h.growing("mxc://s/tail").await.into_body(0, None);
    // Every byte has arrived, but the body has not ended yet, so the tail
    // has no verdict and is held back.
    send(&feed, &data);
    let head = read_exactly(&mut body, data.len() - TAIL).await;
    assert_eq!(head, data[..data.len() - TAIL]);
    assert_blocked(&mut body).await;

    // A range that starts inside the tail waits too.
    let mut suffix = h
        .growing("mxc://s/tail")
        .await
        .into_body((data.len() - 10) as u64, None);
    assert_blocked(&mut suffix).await;

    drop(feed);
    let (rest, error) = collect(body).await;
    assert!(error.is_none());
    assert_eq!(rest, data[data.len() - TAIL..]);
    let (suffix, error) = collect(suffix).await;
    assert!(error.is_none());
    assert_eq!(suffix, data[data.len() - 10..]);
}

#[tokio::test]
async fn a_short_body_aborts_readers_and_is_never_cached() {
    let h = Harness::new().await;
    let data = payload(256 * KIB);
    let feed = h.fetcher.script(Some(data.len() as u64));

    let early = h.growing("mxc://s/short").await;
    let late = h.growing("mxc://s/short").await;
    send(&feed, &data[..200 * KIB]);
    let mut late = late.into_body(0, None);
    read_exactly(&mut late, 128 * KIB).await;
    drop(feed); // ends 56 KiB short of its Content-Length

    // A range that ended before the tail still completes.
    let (head, error) = collect(early.into_body(0, Some(8 * KIB as u64))).await;
    assert!(error.is_none());
    assert_eq!(head, data[..8 * KIB]);
    // A reader still reading is aborted, never handed a clean end.
    let (_, error) = collect(late).await;
    assert!(error.is_some(), "a truncated object must end in an error");

    h.settle().await;
    assert_eq!(h.reserved(), 0);
    // Not cached: the next request downloads again.
    let feed = h.fetcher.script(Some(data.len() as u64));
    assert!(matches!(
        h.get("mxc://s/short", None).await.unwrap(),
        MediaBody::Growing(_)
    ));
    assert_eq!(h.fetcher.calls(), 2);
    drop(feed);
}

#[tokio::test]
async fn a_body_past_its_content_length_fails() {
    let h = Harness::new().await;
    let feed = h.fetcher.script(Some(100 * KIB as u64));
    let body = h.growing("mxc://s/long").await.into_body(0, None);
    send(&feed, &payload(101 * KIB));
    let (_, error) = collect(body).await;
    assert!(error.is_some());
    h.settle().await;
}

fn descriptor(key: &str) -> Value {
    json!({
        "url": "mxc://s/enc",
        "key": { "k": key, "alg": "A256CTR", "ext": true, "kty": "oct", "key_ops": ["encrypt", "decrypt"] },
        "iv": "AAAAAAAAAAAAAAAAAAAAAA",
        "hashes": { "sha256": "abc" },
        "v": "v2",
    })
}

#[tokio::test]
async fn a_failed_hash_is_remembered_for_that_descriptor_only() {
    let h = Harness::new().await;
    let feed = h.fetcher.script(Some(128 * KIB as u64));
    let media = match h
        .get("mxc://s/enc", Some(descriptor("good")))
        .await
        .unwrap()
    {
        MediaBody::Growing(media) => media,
        MediaBody::Complete(_) => panic!("expected a download"),
    };
    send(&feed, &payload(128 * KIB));
    feed.send(Err(FetchError::Undecryptable("hash mismatch".into())))
        .unwrap();
    let (_, error) = collect(media.into_body(0, None)).await;
    assert!(error.is_some());
    h.settle().await;

    // The same descriptor fails at once, with no download.
    let error = h
        .get("mxc://s/enc", Some(descriptor("good")))
        .await
        .expect_err("a recent hash failure is remembered");
    assert!(matches!(
        error,
        MediaCacheError::Fetch(FetchError::Undecryptable(_))
    ));
    assert_eq!(h.fetcher.calls(), 1);

    // Another event's descriptor for the same URI downloads anew.
    let _feed = h.fetcher.script(Some(128 * KIB as u64));
    assert!(h
        .get("mxc://s/enc", Some(descriptor("other")))
        .await
        .is_ok());
    assert_eq!(h.fetcher.calls(), 2);
}

/// ADR 0138 section 4: the bytes served depend on the descriptor that decrypts
/// them, so two events naming one URI with different descriptors never share
/// a streamed download or its cached result.
#[tokio::test]
async fn each_descriptor_of_a_uri_streams_apart() {
    let h = Harness::new().await;
    let (first, second) = (payload(200 * KIB), payload(150 * KIB));
    let first_feed = h.fetcher.script(Some(first.len() as u64));
    let second_feed = h.fetcher.script(Some(second.len() as u64));

    let mut readers = Vec::new();
    for key in ["first", "second"] {
        match h
            .get("mxc://s/enc", Some(descriptor(key)))
            .await
            .expect("stream opens")
        {
            MediaBody::Growing(media) => readers.push(media),
            MediaBody::Complete(_) => panic!("expected a download"),
        }
    }
    assert_eq!(h.fetcher.calls(), 2, "the second descriptor did not join");

    send(&first_feed, &first);
    send(&second_feed, &second);
    drop((first_feed, second_feed));
    for (media, expected) in readers.into_iter().zip([&first, &second]) {
        let (bytes, error) = collect(media.into_body(0, None)).await;
        assert!(error.is_none());
        assert_eq!(&bytes, expected);
    }
    h.settle().await;

    for (key, expected) in [("first", &first), ("second", &second)] {
        match h.get("mxc://s/enc", Some(descriptor(key))).await.unwrap() {
            MediaBody::Complete(mut resource) => {
                let mut bytes = Vec::new();
                tokio::io::AsyncReadExt::read_to_end(&mut resource.file, &mut bytes)
                    .await
                    .unwrap();
                assert_eq!(&bytes, expected, "{key} is cached as its own object");
            }
            MediaBody::Growing(_) => panic!("{key} should be cached"),
        }
    }
    assert_eq!(h.fetcher.calls(), 2);
}

#[tokio::test]
async fn the_download_finishes_after_every_reader_has_gone() {
    let h = Harness::new().await;
    let data = payload(300 * KIB);
    let feed = h.fetcher.script(Some(data.len() as u64));
    drop(h.growing("mxc://s/abandoned").await);

    send(&feed, &data);
    drop(feed);
    h.settle().await;

    match h.get("mxc://s/abandoned", None).await.unwrap() {
        MediaBody::Complete(mut resource) => {
            let mut bytes = Vec::new();
            tokio::io::AsyncReadExt::read_to_end(&mut resource.file, &mut bytes)
                .await
                .unwrap();
            assert_eq!(bytes, data);
        }
        MediaBody::Growing(_) => panic!("the abandoned download should have been cached"),
    }
}

#[tokio::test]
async fn an_unknown_length_is_learned_at_the_end() {
    let h = Harness::new().await;
    let data = payload(200 * KIB);
    let feed = h.fetcher.script(None);

    let mut media = h.growing("mxc://s/chunked").await;
    assert_eq!(media.known_len(), None);
    assert_eq!(
        media.wait_for_len(Duration::from_millis(50)).await.unwrap(),
        None
    );

    let mut body = h.growing("mxc://s/chunked").await.into_body(0, None);
    send(&feed, &data);
    // With no declared length, the most recent 64 KiB are held back.
    read_exactly(&mut body, data.len() - TAIL).await;
    assert_blocked(&mut body).await;
    assert_eq!(h.reserved(), data.len() as u64, "the reservation grew");

    drop(feed);
    let (rest, error) = collect(body).await;
    assert!(error.is_none());
    assert_eq!(rest, data[data.len() - TAIL..]);
    assert_eq!(
        media.wait_for_len(Duration::from_secs(5)).await.unwrap(),
        Some(data.len() as u64)
    );
}

#[tokio::test]
async fn a_declared_length_over_the_stream_limit_is_refused_before_reading() {
    let h = Harness::with(|cfg| cfg.max_stream_bytes = Some(100 * KIB as u64)).await;
    let _feed = h.fetcher.script(Some(101 * KIB as u64));
    let error = h.get("mxc://s/huge", None).await.expect_err("too large");
    assert!(matches!(error, MediaCacheError::TooLarge { .. }));
    assert_eq!(h.reserved(), 0);
}

#[tokio::test]
async fn a_chunked_body_growing_past_the_stream_limit_fails() {
    let h = Harness::with(|cfg| cfg.max_stream_bytes = Some(100 * KIB as u64)).await;
    let feed = h.fetcher.script(None);
    let mut media = h.growing("mxc://s/huge").await;
    send(&feed, &payload(101 * KIB));
    let error = media
        .wait_for_len(Duration::from_secs(5))
        .await
        .expect_err("too large");
    assert!(matches!(error, MediaCacheError::TooLarge { .. }));
    h.settle().await;
    assert_eq!(h.reserved(), 0);
}

#[tokio::test]
async fn a_stream_that_cannot_reserve_room_is_busy() {
    // 4 MiB cache, 1 MiB buffered objects: streams may reserve 3 MiB.
    let h = Harness::with(|cfg| cfg.max_bytes = 4 << 20).await;
    let _first = h.fetcher.script(Some(2 << 20));
    let _held = h.growing("mxc://s/first").await;
    assert_eq!(h.reserved(), 2 << 20);

    let _second = h.fetcher.script(Some(2 << 20));
    let error = h.get("mxc://s/second", None).await.expect_err("no room");
    assert!(matches!(error, MediaCacheError::Busy(_)), "{error}");
    // The refused stream released its permit and reserved nothing.
    assert_eq!(h.reserved(), 2 << 20);
    assert_eq!(h.cache.inner.streams.available_permits(), 3);
}

#[tokio::test]
async fn a_reservation_evicts_completed_entries() {
    let h = Harness::with(|cfg| cfg.max_bytes = 4 << 20).await;
    for url in ["mxc://s/a", "mxc://s/b", "mxc://s/c"] {
        h.cache
            .get_or_fetch(h.account, url, None, h.fetcher.as_ref())
            .await
            .unwrap();
    }
    assert_eq!(h.cache.stats().total_bytes, 3 << 20);

    let _feed = h.fetcher.script(Some(2 << 20));
    let _media = h.growing("mxc://s/video").await;
    let index = h.cache.inner.index.lock().unwrap();
    assert!(index.total_bytes + index.reserved <= 4 << 20);
    assert_eq!(index.total_bytes, 2 << 20, "the oldest entry was evicted");
}

#[tokio::test]
async fn a_stream_waits_for_a_permit_only_until_the_fetch_deadline() {
    let h = Harness::with(|cfg| {
        cfg.max_concurrent_streams = 1;
        cfg.fetch_timeout_secs = 1;
    })
    .await;
    let _first = h.fetcher.script(Some(1 << 20));
    let _held = h.growing("mxc://s/first").await;

    let started = std::time::Instant::now();
    let error = h
        .get("mxc://s/second", None)
        .await
        .expect_err("no permit within the deadline");
    assert!(matches!(error, MediaCacheError::Busy(_)), "{error}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(
        h.fetcher.calls(),
        1,
        "the second stream never reached upstream"
    );
}

#[tokio::test]
async fn a_stalled_download_fails() {
    let h = Harness::with(|cfg| cfg.stall_timeout_secs = 1).await;
    let feed = h.fetcher.script(Some(1 << 20));
    let mut body = h.growing("mxc://s/stalled").await.into_body(0, None);
    send(&feed, &payload(KIB));
    read_exactly(&mut body, KIB).await;
    // The feed stays open but silent, so only the stall timeout ends it.
    let (_, error) = tokio::time::timeout(Duration::from_secs(10), collect(body))
        .await
        .expect("the stall timeout should fail the download");
    let error = error.expect("a stalled download ends in an error");
    assert!(error.to_string().contains("stalled"), "{error}");
    h.settle().await;
    assert_eq!(h.reserved(), 0);
    drop(feed);
}

#[tokio::test]
async fn a_disabled_cache_streams_but_keeps_nothing() {
    let h = Harness::with(|cfg| cfg.enabled = false).await;
    let data = payload(100 * KIB);
    let feed = h.fetcher.script(Some(data.len() as u64));
    let body = h.growing("mxc://s/video").await.into_body(0, None);
    send(&feed, &data);
    drop(feed);
    let (all, error) = collect(body).await;
    assert!(error.is_none());
    assert_eq!(all, data);
    h.settle().await;
    assert_eq!(h.cache.stats().entries, 0);
    assert_eq!(h.reserved(), 0);
    assert!(!h.cache.inner.account_dir(h.account).exists());
}

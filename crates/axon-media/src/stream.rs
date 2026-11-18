//! Reading a streamed download while it is still being written (ADR 0139).
//!
//! A streamed video or audio download writes its plaintext to a staging file
//! and reports how far it has got on a [`tokio::sync::watch`] channel. Each
//! request reads that file through a [`GrowingMedia`], whose body follows the
//! download: it reads what has been written and waits on the channel when it
//! catches up.
//!
//! The download's verdict — the upstream body was exactly as long as promised
//! and, for encrypted media, its SHA-256 matched — arrives only after the last
//! byte. So no reader is handed the last [`VERDICT_TAIL`] bytes, and with them
//! a clean end of body, before that verdict. On a failed verdict every reader
//! still reading yields an error instead, which aborts its response.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::stream::{self, BoxStream, StreamExt};
use tokio::sync::watch;

use crate::cache::{FetchError, HandleGuard, MediaCacheError};

/// How many bytes at the end of the object every reader holds back until the
/// download's verdict (ADR 0139 section 3). While the length is unknown, the
/// most recent this-many bytes written are held back instead, since any chunk
/// could be the last.
pub(crate) const VERDICT_TAIL: u64 = 64 * 1024;

/// The largest chunk a reader hands its response at once.
const READ_CHUNK: u64 = 64 * 1024;

/// An upstream download whose body arrives as a stream (ADR 0139), returned by
/// [`MediaFetcher::fetch_stream`](crate::MediaFetcher::fetch_stream) once the
/// homeserver's response headers are in.
pub struct MediaStream {
    /// The object's length, from the upstream `Content-Length`; `None` for a
    /// chunked response. For encrypted media this is also the plaintext
    /// length, because AES-CTR preserves it.
    pub len: Option<u64>,
    /// The plaintext, chunk by chunk. A clean end of stream means the body
    /// ended cleanly and, for encrypted media, its hash matched; a failed hash
    /// is a final [`FetchError::Undecryptable`] item instead.
    pub body: BoxStream<'static, Result<Bytes, FetchError>>,
}

impl std::fmt::Debug for MediaStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaStream")
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

/// How far a streamed download has got. Published on the download's watch
/// channel after every chunk and once more with the verdict.
#[derive(Clone, Debug)]
pub(crate) struct Progress {
    /// Bytes written to the staging file and readable from it.
    pub(crate) written: u64,
    /// The object's length: the upstream `Content-Length`, or, for a chunked
    /// response, the final byte count once the download has been verified.
    pub(crate) len: Option<u64>,
    pub(crate) outcome: Outcome,
}

impl Progress {
    pub(crate) fn started(len: Option<u64>) -> Self {
        Self {
            written: 0,
            len,
            outcome: Outcome::Running,
        }
    }
}

/// Where a streamed download stands.
#[derive(Clone, Debug)]
pub(crate) enum Outcome {
    /// Still downloading; no verdict yet.
    Running,
    /// Every byte arrived, the length matched and, for encrypted media, the
    /// hash matched. Every byte may be served.
    Verified,
    /// The download failed. Every reader that has not already finished its
    /// range is aborted, and the object is never cached.
    Failed(Failure),
}

/// Why a streamed download failed, in a form every reader can hold a copy of.
#[derive(Clone, Debug)]
pub(crate) enum Failure {
    /// The upstream body failed: an error from the fetcher, a stall, or a
    /// length that did not match the promised one.
    Fetch(FetchError),
    /// The body grew past `max_stream_bytes`.
    TooLarge { len: u64, cap: u64 },
    /// A chunked body could not grow its cache reservation.
    Busy(String),
    /// Writing the staging file failed.
    Io(String),
}

impl Failure {
    pub(crate) fn into_error(self) -> MediaCacheError {
        match self {
            Self::Fetch(error) => MediaCacheError::Fetch(error),
            Self::TooLarge { len, cap } => MediaCacheError::TooLarge { len, cap },
            Self::Busy(message) => MediaCacheError::Busy(message),
            Self::Io(message) => MediaCacheError::Io(std::io::Error::other(message)),
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fetch(error) => error.fmt(f),
            Self::TooLarge { len, cap } => write!(
                f,
                "media object too large ({len} bytes so far; stream limit is {cap} bytes)"
            ),
            Self::Busy(message) | Self::Io(message) => f.write_str(message),
        }
    }
}

/// One streamed download in flight, shared by every request for its object.
pub(crate) struct Partial {
    /// A read handle on the staging file, opened before anything can rename
    /// or unlink it, so every reader keeps the same inode whatever happens to
    /// the path. Readers use positional reads, so they share no cursor.
    file: Arc<std::fs::File>,
    progress: watch::Receiver<Progress>,
}

impl Partial {
    pub(crate) fn new(file: std::fs::File, progress: watch::Receiver<Progress>) -> Self {
        Self {
            file: Arc::new(file),
            progress,
        }
    }

    /// The failure, if the download has already failed.
    pub(crate) fn failure(&self) -> Option<Failure> {
        match &self.progress.borrow().outcome {
            Outcome::Failed(failure) => Some(failure.clone()),
            _ => None,
        }
    }

    /// A new reader of this download.
    pub(crate) fn reader(&self, etag: String, open_guard: HandleGuard) -> GrowingMedia {
        GrowingMedia {
            file: self.file.clone(),
            progress: self.progress.clone(),
            etag,
            _open_guard: open_guard,
        }
    }
}

/// A media object whose streamed download is still running, ready to serve
/// ranges from as they arrive (ADR 0139 section 3).
#[derive(Debug)]
pub struct GrowingMedia {
    file: Arc<std::fs::File>,
    progress: watch::Receiver<Progress>,
    /// The same content-addressed entity tag a complete object carries.
    pub etag: String,
    /// Counts this reader in the open-handle gauge, like a
    /// [`MediaResource`](crate::MediaResource).
    _open_guard: HandleGuard,
}

impl GrowingMedia {
    /// The object's length, if the upstream declared one or the download has
    /// finished.
    pub fn known_len(&self) -> Option<u64> {
        self.progress.borrow().len
    }

    /// Wait up to `wait` for the object's length to be known.
    ///
    /// Returns at once when the upstream declared a `Content-Length`;
    /// otherwise it waits for the download to finish. `Ok(None)` means `wait`
    /// passed with the length still unknown. A download that fails while
    /// waiting returns its error.
    pub async fn wait_for_len(&mut self, wait: Duration) -> Result<Option<u64>, MediaCacheError> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            {
                let progress = self.progress.borrow_and_update();
                if let Outcome::Failed(failure) = &progress.outcome {
                    return Err(failure.clone().into_error());
                }
                if progress.len.is_some() {
                    return Ok(progress.len);
                }
            }
            match tokio::time::timeout_at(deadline, self.progress.changed()).await {
                Err(_elapsed) => return Ok(None),
                Ok(Err(_closed)) => {
                    return Err(MediaCacheError::Io(abandoned()));
                }
                Ok(Ok(())) => {}
            }
        }
    }

    /// The bytes from `start` up to `end` (exclusive), or to the end of the
    /// object when `end` is `None`, read as the download writes them.
    ///
    /// The stream ends cleanly once the range has been read. A range that
    /// reaches the object's last [`VERDICT_TAIL`] bytes waits there for the
    /// download's verdict. If the download fails before the range has been
    /// read, the stream yields an error and ends, so the response is aborted
    /// rather than finished.
    pub fn into_body(
        self,
        start: u64,
        end: Option<u64>,
    ) -> BoxStream<'static, std::io::Result<Bytes>> {
        let cursor = Cursor {
            media: self,
            pos: start,
            end,
            done: false,
        };
        stream::unfold(cursor, |mut cursor| async move {
            if cursor.done {
                return None;
            }
            match cursor.next_chunk().await {
                Ok(Some(chunk)) => Some((Ok(chunk), cursor)),
                Ok(None) => None,
                Err(error) => {
                    cursor.done = true;
                    Some((Err(error), cursor))
                }
            }
        })
        .boxed()
    }
}

/// A reader's position in a [`GrowingMedia`] body.
struct Cursor {
    media: GrowingMedia,
    pos: u64,
    /// Exclusive end of the requested range; `None` reads to the object's end.
    end: Option<u64>,
    done: bool,
}

impl Cursor {
    /// The next chunk of the range, waiting for the download when the reader
    /// has caught up with it. `Ok(None)` is the clean end of the range.
    async fn next_chunk(&mut self) -> std::io::Result<Option<Bytes>> {
        loop {
            let limit = {
                let progress = self.media.progress.borrow_and_update();
                let end = self.end.or(progress.len);
                // A range that has been read in full completes normally, even
                // if the download has since failed: those bytes were served
                // before the verdict (ADR 0139 section 4's trade-off).
                if end.is_some_and(|end| self.pos >= end) {
                    return Ok(None);
                }
                let readable = match &progress.outcome {
                    Outcome::Failed(failure) => {
                        return Err(std::io::Error::other(failure.to_string()));
                    }
                    Outcome::Verified => progress.written,
                    Outcome::Running => match progress.len {
                        Some(len) => progress.written.min(len.saturating_sub(VERDICT_TAIL)),
                        None => progress.written.saturating_sub(VERDICT_TAIL),
                    },
                };
                end.map_or(readable, |end| readable.min(end))
            };

            if limit > self.pos {
                let want = (limit - self.pos).min(READ_CHUNK);
                let chunk = read_at(self.media.file.clone(), self.pos, want).await?;
                if chunk.is_empty() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "media staging file is shorter than its download progress",
                    ));
                }
                self.pos += chunk.len() as u64;
                return Ok(Some(chunk));
            }

            if self.media.progress.changed().await.is_err() {
                return Err(abandoned());
            }
        }
    }
}

/// The download task went away without a verdict: the runtime is shutting
/// down, or the task panicked.
fn abandoned() -> std::io::Error {
    std::io::Error::other("media download ended without a verdict")
}

/// Read up to `len` bytes at `offset` without touching any shared cursor.
async fn read_at(file: Arc<std::fs::File>, offset: u64, len: u64) -> std::io::Result<Bytes> {
    tokio::task::spawn_blocking(move || {
        let len = usize::try_from(len).map_err(std::io::Error::other)?;
        let mut buf = vec![0u8; len];
        let mut filled = 0;
        while filled < len {
            let n = positional_read(&file, &mut buf[filled..], offset + filled as u64)?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        buf.truncate(filled);
        Ok(Bytes::from(buf))
    })
    .await
    .map_err(std::io::Error::other)?
}

#[cfg(unix)]
fn positional_read(file: &std::fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buf, offset)
}

/// `seek_read` moves the handle's cursor, but every read here passes its own
/// offset, so the shared cursor is never relied on.
#[cfg(windows)]
fn positional_read(file: &std::fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(buf, offset)
}

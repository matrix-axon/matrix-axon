//! Wire-neutral content types for outbound message mutations.
//!
//! [`Formatted`] is the rich-text companion to a message's plain `body`. Like the
//! read-side [`LiveEvent`](crate::LiveEvent), it lives in this lowest crate so the
//! two sibling crates that pass it — `axon-api` (the `MessageSender` port) and
//! `axon-sync` (the SDK gateway) — share one type without depending on each other.
//! The composition root in `axon-server` therefore forwards it verbatim, with no
//! mapping.

/// Rich-text rendering of a message body, supplied alongside the plain `body` on
/// send/edit. `format` names the markup (Matrix defines only
/// `org.matrix.custom.html`) and `body` is the rendered source (HTML).
///
/// Borrowed because it lives only for the duration of a single send/edit call.
/// Validated at the API boundary (both fields present together, recognized
/// `format`); the gateway carries it verbatim onto the Matrix event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Formatted<'a> {
    /// The markup name — `org.matrix.custom.html`.
    pub format: &'a str,
    /// The rendered body (HTML).
    pub body: &'a str,
}

/// How an outbound message relates to an existing event, via `m.relates_to`.
///
/// Both fields are optional and independent; the default (`reply_to: None`,
/// `thread_root: None`) is a plain, unrelated message. Like [`Formatted`], it is
/// borrowed for one send call and carried verbatim by the gateway, which builds
/// the concrete `m.relates_to` envelope:
///
/// - `reply_to` only → a plain reply (`m.in_reply_to`).
/// - `thread_root` only → a thread member (`rel_type: m.thread`), not a reply.
/// - `thread_root` + `reply_to` → a reply scoped to the thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Relation<'a> {
    /// The event this message replies to (`m.in_reply_to`).
    pub reply_to: Option<&'a str>,
    /// The thread root this message belongs to (`rel_type: m.thread`).
    pub thread_root: Option<&'a str>,
}

impl Relation<'_> {
    /// Whether any relation is set; `false` means a plain message.
    pub fn is_some(&self) -> bool {
        self.reply_to.is_some() || self.thread_root.is_some()
    }
}

/// Supported Matrix media message shape for a staged upload: `m.image`,
/// `m.file`, `m.video` or `m.audio` (ADR 0142 section 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaSendKind {
    Image,
    File,
    Video,
    Audio,
}

/// Claimed media bytes and metadata passed from the API upload substrate to the
/// outbound message gateway.
///
/// `width`, `height` and `duration_ms` are what the event's `info` declares,
/// already validated for `kind` (ADR 0142 section 4). `thumbnail`, when set,
/// has been normalised to ADR 0142 section 2 and is published best-effort:
/// a failed thumbnail upload sends the event without it (section 6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaAttachment {
    pub kind: MediaSendKind,
    pub filename: String,
    pub content_type: Option<String>,
    pub size_bytes: u64,
    pub bytes: Vec<u8>,
    pub width: Option<u64>,
    pub height: Option<u64>,
    pub duration_ms: Option<u64>,
    pub thumbnail: Option<MediaThumbnail>,
}

/// A claimed attachment and its optional, best-effort image inspection.
pub struct MediaSend {
    pub attachment: MediaAttachment,
    pub image:
        Option<std::pin::Pin<Box<dyn std::future::Future<Output = SendImageMetadata> + Send>>>,
}

impl From<MediaAttachment> for MediaSend {
    fn from(attachment: MediaAttachment) -> Self {
        Self {
            attachment,
            image: None,
        }
    }
}

/// Image metadata read from the original during the upload.
#[derive(Default)]
pub struct SendImageMetadata {
    pub dimensions: Option<(u32, u32)>,
    pub thumbnail: Option<MediaThumbnail>,
}

impl SendImageMetadata {
    pub fn apply(self, attachment: &mut MediaAttachment) {
        if let Some((width, height)) = self.dimensions {
            attachment.width = Some(u64::from(width));
            attachment.height = Some(u64::from(height));
        }
        if attachment.thumbnail.is_none() {
            attachment.thumbnail = self.thumbnail;
        }
    }
}

/// A thumbnail or poster ready to publish beside a media event (ADR 0142
/// section 2): upright, fitted inside 800 × 600, JPEG or PNG, with no EXIF.
/// `width`, `height` and `mimetype` describe `bytes` exactly, so they become
/// the event's `thumbnail_info` as they are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaThumbnail {
    pub bytes: Vec<u8>,
    pub mimetype: String,
    pub width: u32,
    pub height: u32,
}

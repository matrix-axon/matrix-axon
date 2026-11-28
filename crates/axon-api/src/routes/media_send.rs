//! What every `send-media` shares, with or without link previews (ADR 0142
//! section 4): validating `info` and `txn_id`, claiming the original and its
//! thumbnail inside the task that owns the send, and joining a retry to the
//! send its `txn_id` names.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axon_core::{MediaAttachment, MediaSend, MediaSendKind};
use tokio::sync::watch;
use uuid::Uuid;

use crate::dto::{SendMediaInfo, SendMediaRequest, SendResultDto};
use crate::response::{ApiError, ApiResponse};
use crate::uploads::{SendThumbnail, StagedUploadService};

/// The largest integer a Matrix event may carry (canonical JSON).
const MAX_MATRIX_INT: u64 = (1 << 53) - 1;
/// Everything a client thumbnail costs before the send: the decoder permit
/// wait, the decode, the normalisation and the encode (ADR 0142 section 4).
const CLIENT_THUMBNAIL_BUDGET: Duration = Duration::from_secs(5);
/// How long a successful send answers a retry of its `txn_id`.
const TXN_REMEMBERED_FOR: Duration = Duration::from_secs(10 * 60);
/// The most `txn_id`s remembered per account.
const TXNS_PER_ACCOUNT: usize = 256;
/// The longest a retry waits for the send its `txn_id` names. Each step of
/// that send is bounded (ADR 0142 section 8: 5 s for a client thumbnail, 60 s
/// for the original's upload, 10 s for the thumbnail's and 30 s for the
/// event, 105 s with the defaults), but its claims read the database and
/// local disk untimed, so the wait gets a bound of its own: a retry that
/// hits it gets a `504`, and the next one joins again or gets the event.
const JOIN_WAIT: Duration = Duration::from_secs(120);

/// The fields ADR 0142 adds to `send-media`, shared by both send paths.
#[derive(Debug, Clone, Default)]
pub(super) struct MediaFields {
    pub upload_id: Uuid,
    pub info: Option<SendMediaInfo>,
    pub thumbnail_upload_id: Option<Uuid>,
    pub txn_id: Option<String>,
    /// What the request asks to send, apart from upload ids, which change
    /// when a client stages the file again for a retry. A `txn_id` hit with a
    /// different fingerprint is another message reusing the id. It does not
    /// identify the file itself; issue 1257 adds that.
    pub fingerprint: u64,
}

impl From<&SendMediaRequest> for MediaFields {
    fn from(r: &SendMediaRequest) -> Self {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        r.caption
            .as_deref()
            .filter(|c| !c.is_empty())
            .hash(&mut hasher);
        (&r.format, &r.formatted_body, &r.reply_to, &r.thread_root).hash(&mut hasher);
        r.info
            .as_ref()
            .map(|i| (i.width, i.height, i.duration_ms))
            .hash(&mut hasher);
        r.thumbnail_upload_id.is_some().hash(&mut hasher);
        r.url_previews
            .as_ref()
            .map(|previews| {
                previews
                    .iter()
                    .map(|p| (&p.matched_url, &p.title, &p.description, &p.site_name))
                    .collect::<Vec<_>>()
            })
            .hash(&mut hasher);
        Self {
            upload_id: r.upload_id,
            info: r.info.clone(),
            thumbnail_upload_id: r.thumbnail_upload_id,
            txn_id: r.txn_id.clone(),
            fingerprint: hasher.finish(),
        }
    }
}

impl MediaFields {
    /// Everything that can be checked before the original's kind is known,
    /// so before any claim: the shape of `info`, the `txn_id`, and that the
    /// thumbnail is not the original.
    pub fn validate(&self) -> Result<(), ApiError> {
        if let Some(info) = &self.info {
            if info.width.is_some() != info.height.is_some() {
                return Err(ApiError::bad_request(
                    "info.width and info.height must be provided together",
                ));
            }
            for (name, value) in [
                ("width", info.width),
                ("height", info.height),
                ("duration_ms", info.duration_ms),
            ] {
                if value.is_some_and(|v| v == 0 || v > MAX_MATRIX_INT) {
                    return Err(ApiError::bad_request(format!(
                        "info.{name} must be between 1 and {MAX_MATRIX_INT}"
                    )));
                }
            }
        }
        if let Some(txn_id) = &self.txn_id {
            let valid = (1..=64).contains(&txn_id.len())
                && txn_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._~-".contains(&b));
            if !valid {
                return Err(ApiError::bad_request(
                    "txn_id must be 1 to 64 characters of A-Z, a-z, 0-9, '.', '_', '~' and '-'",
                ));
            }
        }
        if self.thumbnail_upload_id == Some(self.upload_id) {
            return Err(ApiError::bad_request(
                "upload IDs must be distinct within a send",
            ));
        }
        Ok(())
    }

    /// The staged uploads this request names, which a retry joined to an
    /// earlier send deletes.
    pub fn upload_ids(&self) -> impl Iterator<Item = Uuid> + '_ {
        std::iter::once(self.upload_id).chain(self.thumbnail_upload_id)
    }
}

/// Refuse `info` fields and a thumbnail the original's kind cannot carry.
fn check_kind(kind: MediaSendKind, fields: &MediaFields) -> Result<(), ApiError> {
    let info = fields.info.clone().unwrap_or_default();
    let (sized, timed, thumbnailed) = match kind {
        MediaSendKind::Image => (true, false, true),
        MediaSendKind::Video => (true, true, true),
        MediaSendKind::Audio => (false, true, false),
        MediaSendKind::File => (false, false, true),
    };
    let name = match kind {
        MediaSendKind::Image => "image",
        MediaSendKind::Video => "video",
        MediaSendKind::Audio => "audio",
        MediaSendKind::File => "file",
    };
    if !sized && info.width.is_some() {
        return Err(ApiError::bad_request(format!(
            "info.width and info.height are not accepted for {name} uploads"
        )));
    }
    if !timed && info.duration_ms.is_some() {
        return Err(ApiError::bad_request(format!(
            "info.duration_ms is not accepted for {name} uploads"
        )));
    }
    if !thumbnailed && fields.thumbnail_upload_id.is_some() {
        return Err(ApiError::bad_request(format!(
            "thumbnail_upload_id is not accepted for {name} uploads"
        )));
    }
    Ok(())
}

/// Claim the original and, when there is one, its thumbnail, recording each
/// claim in `claimed` so the caller settles it whatever happens next. Runs in
/// the task that owns the send (ADR 0142 section 4).
///
/// A wrong kind or an invalid thumbnail is a `400`. A thumbnail the decoder
/// could not finish within its budget stays claimed and is left out.
pub(super) async fn claim_attachment(
    uploads: &dyn StagedUploadService,
    account_id: Uuid,
    fields: &MediaFields,
    claimed: &mut Vec<Uuid>,
) -> Result<MediaAttachment, ApiError> {
    let upload = uploads.claim_upload(account_id, fields.upload_id).await?;
    claimed.push(fields.upload_id);
    let mut attachment: MediaAttachment = upload.into();
    check_kind(attachment.kind, fields)?;
    if let Some(info) = &fields.info {
        attachment.width = info.width;
        attachment.height = info.height;
        attachment.duration_ms = info.duration_ms;
    }
    if let Some(id) = fields.thumbnail_upload_id {
        let thumbnail = uploads
            .claim_send_thumbnail(account_id, id, CLIENT_THUMBNAIL_BUDGET)
            .await?;
        claimed.push(id);
        match thumbnail {
            SendThumbnail::Ready(thumbnail) => attachment.thumbnail = Some(thumbnail),
            SendThumbnail::Skipped(error_kind) => {
                tracing::warn!(%account_id, upload_id = %id, error_kind, "client thumbnail not ready within its budget; sending without it");
            }
        }
    }
    Ok(attachment)
}

/// Inspect only image sends. A supplied thumbnail takes priority, including
/// when the client-thumbnail validation ran out of time.
pub(super) fn inspect_image(
    attachment: MediaAttachment,
    thumbnailer: Option<std::sync::Arc<dyn crate::SendThumbnailer>>,
    account_id: Uuid,
    room_id: &str,
    fields: &MediaFields,
) -> MediaSend {
    let mut send = MediaSend::from(attachment);
    if let Some(thumbnailer) = thumbnailer.filter(|_| send.attachment.kind == MediaSendKind::Image)
    {
        let room_id = room_id.to_owned();
        let upload_id = fields.upload_id;
        let generate = fields.thumbnail_upload_id.is_none();
        send.image = Some(Box::pin(async move {
            thumbnailer
                .inspect(account_id, &room_id, upload_id, generate)
                .await
        }));
    }
    send
}

/// The uploads one send has claimed, owned by the task that sends.
///
/// [`settle`](Self::settle) consumes them after a success or releases them
/// for a retry. Dropped unsettled, because the task panicked, it releases
/// them from a task of its own, so they are not left `sending` until the
/// next boot, where a retry could not claim them again.
pub(super) struct Claims {
    uploads: std::sync::Arc<dyn StagedUploadService>,
    account_id: Uuid,
    room_id: String,
    /// Pushed to as each claim succeeds.
    pub ids: Vec<Uuid>,
}

impl Claims {
    pub fn new(
        uploads: std::sync::Arc<dyn StagedUploadService>,
        account_id: Uuid,
        room_id: &str,
    ) -> Self {
        Self {
            uploads,
            account_id,
            room_id: room_id.to_owned(),
            ids: Vec::new(),
        }
    }

    pub async fn settle(mut self, sent: bool) {
        let ids = std::mem::take(&mut self.ids);
        settle_claims(&*self.uploads, self.account_id, &self.room_id, ids, sent).await;
    }
}

impl Drop for Claims {
    fn drop(&mut self) {
        let ids = std::mem::take(&mut self.ids);
        if ids.is_empty() {
            return;
        }
        let (account_id, room_id) = (self.account_id, std::mem::take(&mut self.room_id));
        tracing::warn!(%account_id, %room_id, count = ids.len(), "send ended without settling its uploads; releasing them");
        let uploads = self.uploads.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    settle_claims(&*uploads, account_id, &room_id, ids, false).await;
                });
            }
            Err(_) => {
                tracing::warn!(%account_id, "no runtime to release uploads; boot reconciliation will");
            }
        }
    }
}

/// Consume the claimed uploads after a success, or release them for a retry.
async fn settle_claims(
    uploads: &dyn StagedUploadService,
    account_id: Uuid,
    room_id: &str,
    claimed: Vec<Uuid>,
    sent: bool,
) {
    for id in claimed {
        let cleanup = if sent {
            uploads.complete_upload(account_id, id).await
        } else {
            uploads.release_upload(account_id, id).await
        };
        if let Err(error) = cleanup {
            // Never format error detail: it can contain private input.
            let kind = error.diagnostic_kind();
            tracing::warn!(%account_id, %room_id, upload_id = %id, sent, error_kind = kind, "upload bookkeeping failed; boot reconciliation/expiry will recover");
        }
    }
}

type TxnKey = (String, String);

struct TxnEntry {
    state: TxnState,
    /// The send's [`MediaFields::fingerprint`].
    fingerprint: u64,
    /// The uploads the send named. A joined retry never deletes these: they
    /// belong to the send, which claims and settles them itself.
    uploads: Vec<Uuid>,
}

enum TxnState {
    /// The owner's task sends the event id here when it succeeds, and drops
    /// the sender without one when it fails.
    InFlight(watch::Receiver<Option<String>>),
    Sent {
        event_id: String,
        at: Instant,
    },
}

impl TxnEntry {
    fn expired(&self, now: Instant) -> bool {
        matches!(self.state, TxnState::Sent { at, .. } if now.duration_since(at) >= TXN_REMEMBERED_FOR)
    }
}

/// Recent `txn_id`s per account (ADR 0142 section 4), in memory only: after
/// a restart a retry sends again, and the homeserver's transaction ids are
/// what keep it from posting twice.
#[derive(Default)]
struct MediaTxns {
    state: Mutex<TxnsState>,
}

#[derive(Default)]
struct TxnsState {
    accounts: HashMap<Uuid, HashMap<TxnKey, TxnEntry>>,
    /// When every account's expired entries were last dropped.
    swept_at: Option<Instant>,
}

/// How often a `begin` also expires other accounts' entries, so an account
/// that stops sending media does not keep them, without scanning every
/// account on every send.
const SWEEP_EVERY: Duration = Duration::from_secs(60);

impl MediaTxns {
    /// The map, even after a panic while another thread held the lock: every
    /// update leaves it consistent, and a poisoned lock that panicked in
    /// [`TxnGuard`]'s `Drop` during unwinding would abort the process.
    fn lock(&self) -> std::sync::MutexGuard<'_, TxnsState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

static MEDIA_TXNS: LazyLock<MediaTxns> = LazyLock::new(MediaTxns::default);

enum Begin {
    /// No send holds this id: this request sends, and settles the id through
    /// the guard. `None` when the account already remembers 256 live ids.
    Owner(Option<TxnGuard>),
    /// The send holding the id, and the uploads it named.
    Wait(watch::Receiver<Option<String>>, Vec<Uuid>),
    Sent(String, Vec<Uuid>),
    /// The id belongs to a different message.
    Mismatch,
}

impl MediaTxns {
    fn begin(
        &'static self,
        account_id: Uuid,
        key: TxnKey,
        fingerprint: u64,
        uploads: Vec<Uuid>,
    ) -> Begin {
        let mut state = self.lock();
        let now = Instant::now();
        if state
            .swept_at
            .is_none_or(|at| now.duration_since(at) >= SWEEP_EVERY)
        {
            state.accounts.retain(|_, txns| {
                txns.retain(|_, entry| !entry.expired(now));
                !txns.is_empty()
            });
            state.swept_at = Some(now);
        }
        let txns = state.accounts.entry(account_id).or_default();
        txns.retain(|_, entry| !entry.expired(now));
        if let Some(entry) = txns.get(&key) {
            if entry.fingerprint != fingerprint {
                return Begin::Mismatch;
            }
            return match &entry.state {
                TxnState::Sent { event_id, .. } => {
                    Begin::Sent(event_id.clone(), entry.uploads.clone())
                }
                TxnState::InFlight(rx) => Begin::Wait(rx.clone(), entry.uploads.clone()),
            };
        }
        // Full of ids still inside their window: send without remembering
        // this one rather than evict a live one, so every remembered id keeps
        // its 10 minutes. The homeserver's transaction id still deduplicates
        // a retry of the unremembered send.
        if txns.len() >= TXNS_PER_ACCOUNT {
            return Begin::Owner(None);
        }
        let (tx, rx) = watch::channel(None);
        txns.insert(
            key.clone(),
            TxnEntry {
                state: TxnState::InFlight(rx),
                fingerprint,
                uploads,
            },
        );
        Begin::Owner(Some(TxnGuard {
            txns: self,
            account_id,
            key,
            tx,
            settled: false,
        }))
    }
}

/// Settles one `txn_id` when its send ends. Dropped without
/// [`settle`](Self::settle), as when the task panics, it forgets the id, so a
/// waiting retry runs as a new send.
pub(super) struct TxnGuard {
    txns: &'static MediaTxns,
    account_id: Uuid,
    key: TxnKey,
    tx: watch::Sender<Option<String>>,
    settled: bool,
}

impl TxnGuard {
    /// Remember a success for its retries; forget a failure, so its retry
    /// runs as a new send.
    pub fn settle<T>(mut self, result: &Result<String, T>) {
        self.settled = true;
        let mut state = self.txns.lock();
        let accounts = &mut state.accounts;
        let Some(txns) = accounts.get_mut(&self.account_id) else {
            return;
        };
        match result {
            Ok(event_id) => {
                if let Some(entry) = txns.get_mut(&self.key) {
                    entry.state = TxnState::Sent {
                        event_id: event_id.clone(),
                        at: Instant::now(),
                    };
                }
                self.tx.send_replace(Some(event_id.clone()));
            }
            Err(_) => {
                txns.remove(&self.key);
                if txns.is_empty() {
                    accounts.remove(&self.account_id);
                }
            }
        }
    }
}

impl Drop for TxnGuard {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        let mut state = self.txns.lock();
        let accounts = &mut state.accounts;
        if let Some(txns) = accounts.get_mut(&self.account_id) {
            txns.remove(&self.key);
            if txns.is_empty() {
                accounts.remove(&self.account_id);
            }
        }
    }
}

/// Run `send` once per `txn_id` in a room (ADR 0142 section 4).
///
/// Without a `txn_id`, `send` just runs. With one, a retry whose id is in
/// flight waits for that send (up to [`JOIN_WAIT`]), and one whose id
/// succeeded recently gets its event id back; either way it sends nothing,
/// and the staged uploads it named are deleted, except those the earlier send
/// named itself. A retry whose request differs from the earlier send's is a
/// `409`, and its uploads are kept.
///
/// A failed send is not remembered, and that includes a `504`, whose outcome
/// Axon cannot learn once it stops waiting. So a retry after it, or one that
/// was waiting on it, sends anew. What keeps that from posting twice is the
/// Matrix transaction id the `txn_id` becomes: a homeserver that did complete
/// the first send returns that event for the same id in the same room.
///
/// `send` receives the guard for the id, and must settle it in the task that
/// owns the send, so a client that disconnects cannot leave the id stuck.
pub(super) async fn deduplicated<F, Fut>(
    uploads: &dyn StagedUploadService,
    account_id: Uuid,
    room_id: &str,
    fields: &MediaFields,
    send: F,
) -> Result<ApiResponse<SendResultDto>, ApiError>
where
    F: FnOnce(Option<TxnGuard>) -> Fut,
    Fut: Future<Output = Result<ApiResponse<SendResultDto>, ApiError>>,
{
    let Some(txn_id) = &fields.txn_id else {
        return send(None).await;
    };
    let key = (room_id.to_owned(), txn_id.clone());
    let own: Vec<Uuid> = fields.upload_ids().collect();
    loop {
        let begin = MEDIA_TXNS.begin(account_id, key.clone(), fields.fingerprint, own.clone());
        let (event_id, theirs) = match begin {
            Begin::Owner(guard) => return send(guard).await,
            Begin::Mismatch => {
                return Err(ApiError::conflict(
                    "txn_id was already used for a different send in this room",
                ))
            }
            Begin::Sent(event_id, theirs) => (event_id, theirs),
            Begin::Wait(mut rx, theirs) => {
                match tokio::time::timeout(JOIN_WAIT, rx.wait_for(Option::is_some)).await {
                    Ok(Ok(event_id)) => (event_id.clone().unwrap_or_default(), theirs),
                    // The first send failed and was forgotten: try again.
                    Ok(Err(_)) => continue,
                    Err(_) => {
                        return Err(ApiError::gateway_timeout(
                            "the earlier send with this txn_id is still running",
                        ))
                    }
                }
            }
        };
        tracing::info!(%account_id, %room_id, %event_id, "send-media retry joined an earlier send");
        // Uploads the earlier send named are its own to claim and settle;
        // deleting one could fail that send before it claims it.
        for id in own.into_iter().filter(|id| !theirs.contains(id)) {
            match uploads.delete_upload(account_id, id).await {
                Ok(()) | Err(crate::uploads::StageUploadError::NotFound(_)) => {}
                Err(error) => {
                    tracing::warn!(%account_id, upload_id = %id, error_kind = error.diagnostic_kind(), "could not delete a joined retry's staged upload; expiry will");
                }
            }
        }
        return Ok(ApiResponse::new(SendResultDto { event_id }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `begin` with fingerprint 0 and no uploads.
    fn begin(account: Uuid, key: TxnKey) -> Begin {
        MEDIA_TXNS.begin(account, key, 0, Vec::new())
    }

    fn fields() -> MediaFields {
        MediaFields {
            upload_id: Uuid::new_v4(),
            ..MediaFields::default()
        }
    }

    #[test]
    fn validate_checks_info_shape_and_txn_id() {
        let info = |width, height, duration_ms| MediaFields {
            info: Some(SendMediaInfo {
                width,
                height,
                duration_ms,
            }),
            ..fields()
        };
        assert!(info(Some(1), Some(1), Some(1)).validate().is_ok());
        assert!(info(Some(MAX_MATRIX_INT), Some(1), None).validate().is_ok());
        assert!(info(Some(1), None, None).validate().is_err(), "unpaired");
        assert!(info(Some(0), Some(1), None).validate().is_err(), "zero");
        assert!(info(None, None, Some(MAX_MATRIX_INT + 1))
            .validate()
            .is_err());
        let txn = |id: &str| MediaFields {
            txn_id: Some(id.into()),
            ..fields()
        };
        assert!(txn("0b7c-Az._~").validate().is_ok());
        assert!(txn(&"a".repeat(64)).validate().is_ok());
        assert!(txn(&"a".repeat(65)).validate().is_err());
        assert!(txn("").validate().is_err());
        assert!(txn("a/b").validate().is_err());
        assert!(txn("é").validate().is_err());
        let mut same = fields();
        same.thumbnail_upload_id = Some(same.upload_id);
        assert!(
            same.validate().is_err(),
            "the thumbnail is not the original"
        );
    }

    #[test]
    fn each_kind_takes_only_the_fields_it_can_carry() {
        let with = |width: bool, duration: bool, thumbnail: bool| MediaFields {
            info: Some(SendMediaInfo {
                width: width.then_some(2),
                height: width.then_some(2),
                duration_ms: duration.then_some(2),
            }),
            thumbnail_upload_id: thumbnail.then(Uuid::new_v4),
            ..fields()
        };
        use MediaSendKind::*;
        for (kind, width, duration, thumbnail) in [
            (Image, true, false, true),
            (Video, true, true, true),
            (Audio, false, true, false),
            (File, false, false, true),
        ] {
            assert!(check_kind(kind, &with(width, duration, thumbnail)).is_ok());
            if !width {
                assert!(
                    check_kind(kind, &with(true, false, false)).is_err(),
                    "{kind:?}"
                );
            }
            if !duration {
                assert!(
                    check_kind(kind, &with(false, true, false)).is_err(),
                    "{kind:?}"
                );
            }
            if !thumbnail {
                assert!(
                    check_kind(kind, &with(false, false, true)).is_err(),
                    "{kind:?}"
                );
            }
        }
    }

    #[test]
    fn a_txn_is_joined_while_in_flight_remembered_on_success_and_forgotten_on_failure() {
        let account = Uuid::new_v4();
        let key = |txn: &str| ("!room:x".to_owned(), txn.to_owned());
        let Begin::Owner(Some(guard)) = begin(account, key("a")) else {
            panic!("the first send owns the id");
        };
        let Begin::Wait(rx, _) = begin(account, key("a")) else {
            panic!("a retry waits");
        };
        assert!(
            matches!(
                begin(account, ("!other:x".into(), "a".into())),
                Begin::Owner(Some(_))
            ),
            "the same id in another room is another send"
        );
        guard.settle::<()>(&Ok("$e".into()));
        assert_eq!(rx.borrow().as_deref(), Some("$e"));
        assert!(matches!(begin(account, key("a")), Begin::Sent(e, _) if e == "$e"));

        let Begin::Owner(Some(guard)) = begin(account, key("b")) else {
            panic!();
        };
        let Begin::Wait(rx, _) = begin(account, key("b")) else {
            panic!();
        };
        guard.settle(&Err(()));
        assert!(rx.has_changed().is_err(), "the waiter sees the failure");
        assert!(matches!(begin(account, key("b")), Begin::Owner(Some(_))));

        let Begin::Owner(Some(guard)) = begin(account, key("c")) else {
            panic!();
        };
        drop(guard);
        assert!(
            matches!(begin(account, key("c")), Begin::Owner(Some(_))),
            "a task that never settled does not pin the id"
        );
    }

    #[test]
    fn an_account_remembers_at_most_256_ids() {
        let account = Uuid::new_v4();
        let key = |i: usize| ("!room:x".to_owned(), format!("t{i}"));
        for i in 0..TXNS_PER_ACCOUNT {
            let Begin::Owner(Some(guard)) = begin(account, key(i)) else {
                panic!();
            };
            guard.settle::<()>(&Ok(format!("$e{i}")));
        }
        // Full of live ids: the new send goes unremembered rather than evict
        // one still inside its 10 minutes.
        assert!(matches!(begin(account, key(9999)), Begin::Owner(None)));
        assert!(matches!(begin(account, key(0)), Begin::Sent(..)));
        assert!(matches!(begin(account, key(255)), Begin::Sent(..)));
    }

    /// A panic while the lock is held must not make every later send, and
    /// [`TxnGuard`]'s `Drop`, panic too.
    #[test]
    fn a_poisoned_lock_is_still_usable() {
        let txns: &'static MediaTxns = Box::leak(Box::default());
        let poison = std::thread::spawn(move || {
            let _held = txns.state.lock().unwrap();
            panic!("poison the lock on purpose");
        });
        assert!(poison.join().is_err());
        assert!(txns.state.is_poisoned());
        let key = ("!room:x".to_owned(), "poison".to_owned());
        let Begin::Owner(Some(guard)) = txns.begin(Uuid::new_v4(), key, 0, Vec::new()) else {
            panic!("begin works after the poisoning");
        };
        drop(guard);
    }

    /// A hit with another fingerprint is another message reusing the id; a
    /// hit reports the uploads the earlier send named, so a retry leaves them
    /// alone; and every account's expired entries go, not only the caller's.
    #[test]
    fn a_hit_checks_the_fingerprint_reports_the_owners_uploads_and_expiry_spans_accounts() {
        let account = Uuid::new_v4();
        let key = ("!room:x".to_owned(), "f".to_owned());
        let mine = vec![Uuid::new_v4(), Uuid::new_v4()];
        let Begin::Owner(Some(guard)) = MEDIA_TXNS.begin(account, key.clone(), 7, mine.clone())
        else {
            panic!();
        };
        assert!(matches!(
            MEDIA_TXNS.begin(account, key.clone(), 8, Vec::new()),
            Begin::Mismatch
        ));
        assert!(
            matches!(MEDIA_TXNS.begin(account, key.clone(), 7, Vec::new()), Begin::Wait(_, ref ids) if *ids == mine)
        );
        guard.settle::<()>(&Ok("$f".into()));
        assert!(
            matches!(MEDIA_TXNS.begin(account, key.clone(), 7, Vec::new()), Begin::Sent(ref e, ref ids) if e == "$f" && *ids == mine)
        );
        assert!(matches!(
            MEDIA_TXNS.begin(account, key.clone(), 8, Vec::new()),
            Begin::Mismatch
        ));

        // Age the success past the window, then touch another account.
        {
            let mut state = MEDIA_TXNS.lock();
            let entry = state
                .accounts
                .get_mut(&account)
                .unwrap()
                .get_mut(&key)
                .unwrap();
            if let TxnState::Sent { at, .. } = &mut entry.state {
                *at -= TXN_REMEMBERED_FOR;
            }
            // As if the last sweep were a minute ago.
            state.swept_at = None;
        }
        let other = Uuid::new_v4();
        let Begin::Owner(Some(guard)) = begin(other, key.clone()) else {
            panic!();
        };
        guard.settle::<()>(&Err(()));
        assert!(
            !MEDIA_TXNS.lock().accounts.contains_key(&account),
            "an idle account's expired entries were dropped"
        );
    }
}

//! What every `send-media` shares, with or without link previews (ADR 0142
//! section 4): validating `info` and `txn_id`, claiming the original and its
//! thumbnail inside the task that owns the send, and joining a retry to the
//! send its `txn_id` names.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axon_core::{MediaAttachment, MediaSendKind};
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

/// The fields ADR 0142 adds to `send-media`, shared by both send paths.
#[derive(Debug, Clone, Default)]
pub(super) struct MediaFields {
    pub upload_id: Uuid,
    pub info: Option<SendMediaInfo>,
    pub thumbnail_upload_id: Option<Uuid>,
    pub txn_id: Option<String>,
}

impl From<&SendMediaRequest> for MediaFields {
    fn from(r: &SendMediaRequest) -> Self {
        Self {
            upload_id: r.upload_id,
            info: r.info.clone(),
            thumbnail_upload_id: r.thumbnail_upload_id,
            txn_id: r.txn_id.clone(),
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

/// Consume the claimed uploads after a success, or release them for a retry.
pub(super) async fn settle_claims(
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

enum TxnEntry {
    /// The owner's task sends the event id here when it succeeds, and drops
    /// the sender without one when it fails.
    InFlight(watch::Receiver<Option<String>>),
    Sent {
        event_id: String,
        at: Instant,
    },
}

/// Recent `txn_id`s per account (ADR 0142 section 4), in memory only: after
/// a restart a retry sends again, and the homeserver's transaction ids are
/// what keep it from posting twice.
#[derive(Default)]
struct MediaTxns {
    accounts: Mutex<HashMap<Uuid, HashMap<TxnKey, TxnEntry>>>,
}

static MEDIA_TXNS: LazyLock<MediaTxns> = LazyLock::new(MediaTxns::default);

enum Begin {
    /// No send holds this id: this request sends, and settles the id through
    /// the guard. `None` when the account's map is full of sends in flight.
    Owner(Option<TxnGuard>),
    Wait(watch::Receiver<Option<String>>),
    Sent(String),
}

impl MediaTxns {
    fn begin(&'static self, account_id: Uuid, key: TxnKey) -> Begin {
        let mut accounts = self.accounts.lock().unwrap();
        let txns = accounts.entry(account_id).or_default();
        let now = Instant::now();
        txns.retain(|_, entry| match entry {
            TxnEntry::InFlight(_) => true,
            TxnEntry::Sent { at, .. } => now.duration_since(*at) < TXN_REMEMBERED_FOR,
        });
        match txns.get(&key) {
            Some(TxnEntry::Sent { event_id, .. }) => return Begin::Sent(event_id.clone()),
            Some(TxnEntry::InFlight(rx)) => return Begin::Wait(rx.clone()),
            None => {}
        }
        if txns.len() >= TXNS_PER_ACCOUNT {
            let oldest = txns
                .iter()
                .filter_map(|(key, entry)| match entry {
                    TxnEntry::Sent { at, .. } => Some((*at, key.clone())),
                    TxnEntry::InFlight(_) => None,
                })
                .min();
            match oldest {
                Some((_, key)) => {
                    txns.remove(&key);
                }
                None => return Begin::Owner(None),
            }
        }
        let (tx, rx) = watch::channel(None);
        txns.insert(key.clone(), TxnEntry::InFlight(rx));
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
        let mut accounts = self.txns.accounts.lock().unwrap();
        let Some(txns) = accounts.get_mut(&self.account_id) else {
            return;
        };
        match result {
            Ok(event_id) => {
                txns.insert(
                    self.key.clone(),
                    TxnEntry::Sent {
                        event_id: event_id.clone(),
                        at: Instant::now(),
                    },
                );
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
        let mut accounts = self.txns.accounts.lock().unwrap();
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
/// flight waits for that send, and one whose id succeeded recently gets its
/// event id back; either way it sends nothing, and the staged uploads it
/// named are deleted. A failed send is not remembered, so a retry after it,
/// or one that was waiting on it, sends anew.
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
    loop {
        let event_id = match MEDIA_TXNS.begin(account_id, key.clone()) {
            Begin::Owner(guard) => return send(guard).await,
            Begin::Sent(event_id) => event_id,
            Begin::Wait(mut rx) => {
                // No bound of its own: each step of the first send is bounded.
                match rx.wait_for(Option::is_some).await {
                    Ok(event_id) => event_id.clone().unwrap_or_default(),
                    // The first send failed and was forgotten: try again.
                    Err(_) => continue,
                }
            }
        };
        tracing::info!(%account_id, %room_id, %event_id, "send-media retry joined an earlier send");
        for id in fields.upload_ids() {
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
        let Begin::Owner(Some(guard)) = MEDIA_TXNS.begin(account, key("a")) else {
            panic!("the first send owns the id");
        };
        let Begin::Wait(rx) = MEDIA_TXNS.begin(account, key("a")) else {
            panic!("a retry waits");
        };
        assert!(
            matches!(
                MEDIA_TXNS.begin(account, ("!other:x".into(), "a".into())),
                Begin::Owner(Some(_))
            ),
            "the same id in another room is another send"
        );
        guard.settle::<()>(&Ok("$e".into()));
        assert_eq!(rx.borrow().as_deref(), Some("$e"));
        assert!(matches!(MEDIA_TXNS.begin(account, key("a")), Begin::Sent(e) if e == "$e"));

        let Begin::Owner(Some(guard)) = MEDIA_TXNS.begin(account, key("b")) else {
            panic!();
        };
        let Begin::Wait(rx) = MEDIA_TXNS.begin(account, key("b")) else {
            panic!();
        };
        guard.settle(&Err(()));
        assert!(rx.has_changed().is_err(), "the waiter sees the failure");
        assert!(matches!(
            MEDIA_TXNS.begin(account, key("b")),
            Begin::Owner(Some(_))
        ));

        let Begin::Owner(Some(guard)) = MEDIA_TXNS.begin(account, key("c")) else {
            panic!();
        };
        drop(guard);
        assert!(
            matches!(MEDIA_TXNS.begin(account, key("c")), Begin::Owner(Some(_))),
            "a task that never settled does not pin the id"
        );
    }

    #[test]
    fn an_account_remembers_at_most_256_ids() {
        let account = Uuid::new_v4();
        let key = |i: usize| ("!room:x".to_owned(), format!("t{i}"));
        for i in 0..TXNS_PER_ACCOUNT {
            let Begin::Owner(Some(guard)) = MEDIA_TXNS.begin(account, key(i)) else {
                panic!();
            };
            guard.settle::<()>(&Ok(format!("$e{i}")));
        }
        let Begin::Owner(Some(_new)) = MEDIA_TXNS.begin(account, key(9999)) else {
            panic!("a full map evicts the oldest success");
        };
        assert!(matches!(
            MEDIA_TXNS.begin(account, key(0)),
            Begin::Owner(Some(_))
        ));
        assert!(matches!(MEDIA_TXNS.begin(account, key(5)), Begin::Sent(_)));
    }
}

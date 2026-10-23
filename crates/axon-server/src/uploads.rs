//! Composition-root staged-upload service (M15a, ADR 0059).
//!
//! This adapter satisfies `axon-api`'s upload port by streaming request bytes to
//! a durable staging directory and recording the metadata row in `axon-store`.
//!
//! M15c (GH #286) adds the pruning/reconciliation leg: [`reconcile_boot`] runs
//! once at boot, before the HTTP listener binds (resetting stale `sending`
//! rows, pruning orphan files, and sweeping already-expired staged rows), and
//! [`spawn_expiry_sweeper`] then repeats that same expiry sweep periodically
//! for the rest of the process's life.
//!
//! [`reconcile_boot`]: FilesystemStagedUploads::reconcile_boot

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axon_api::{
    ClaimedUpload, StageUploadError, StageUploadRequest, StagedUpload, StagedUploadService,
    UploadStream,
};
use axon_core::MediaConfig;
use axon_store::{AccountState, MediaUploadKind, NewMediaUpload, Store};
use chrono::{TimeDelta, Utc};
use futures_util::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Semaphore;
use uuid::Uuid;

fn validate_preview_image(
    upload: ClaimedUpload,
) -> Result<axon_core::link_preview::PreviewImage, StageUploadError> {
    use axon_core::link_preview::{PreviewImage, MAX_IMAGE_BYTES, MAX_IMAGE_PIXELS};
    let invalid = || {
        StageUploadError::Invalid("preview must be a valid PNG, JPEG or WebP image of at most 4,000,000 pixels with matching MIME type".into())
    };
    if upload.kind != axon_api::MediaUploadKindDto::Image
        || upload.bytes.len() as u64 > MAX_IMAGE_BYTES
    {
        return Err(invalid());
    }
    let reader = image::ImageReader::new(std::io::Cursor::new(&upload.bytes))
        .with_guessed_format()
        .map_err(|_| invalid())?;
    let format = reader.format().ok_or_else(invalid)?;
    let mimetype = match format {
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::WebP => "image/webp",
        _ => return Err(invalid()),
    };
    if upload.content_type.as_deref().is_some_and(|t| {
        let declared = t.split(';').next().unwrap_or_default().trim();
        !(declared.eq_ignore_ascii_case(mimetype)
            || mimetype == "image/jpeg" && declared.eq_ignore_ascii_case("image/jpg"))
    }) {
        return Err(invalid());
    }
    let (width, height) = reader.into_dimensions().map_err(|_| invalid())?;
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS {
        return Err(invalid());
    }
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(&upload.bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(width);
    limits.max_image_height = Some(height);
    limits.max_alloc = Some(32 * 1024 * 1024);
    reader.limits(limits);
    reader.decode().map_err(|_| invalid())?;
    Ok(PreviewImage {
        bytes: upload.bytes,
        mimetype: mimetype.into(),
        width,
        height,
    })
}

/// Filesystem-backed staged-upload implementation.
pub struct FilesystemStagedUploads {
    store: Store,
    root: PathBuf,
    max_upload_bytes: u64,
    upload_timeout: Duration,
    ttl: Duration,
    concurrent_uploads: Arc<Semaphore>,
    /// `config.max_concurrent_uploads`, kept alongside the semaphore purely for
    /// logging context — `Semaphore` doesn't expose its original capacity.
    concurrent_uploads_total: usize,
}

impl FilesystemStagedUploads {
    /// A cap is checked against both DB metadata and bytes actually read.
    /// The owning API task is not cancelled on HTTP disconnect; failures here
    /// release only the claim this invocation acquired.
    async fn claim_bounded(
        &self,
        account_id: Uuid,
        upload_id: Uuid,
        cap: u64,
    ) -> Result<ClaimedUpload, StageUploadError> {
        let row = self
            .store
            .claim_staged_media_upload(account_id, upload_id)
            .await
            .map_err(internal)?
            .ok_or_else(|| StageUploadError::NotFound("upload not found".into()))?;
        let result = async {
            if row.size_bytes < 0 || row.size_bytes as u64 > cap {
                return Err(StageUploadError::TooLarge { cap });
            }
            let file = tokio::fs::File::open(&row.path).await.map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    StageUploadError::NotFound("upload file not found".into())
                } else {
                    internal(e)
                }
            })?;
            let mut bytes = Vec::new();
            file.take(cap.saturating_add(1))
                .read_to_end(&mut bytes)
                .await
                .map_err(internal)?;
            if bytes.len() as u64 > cap {
                return Err(StageUploadError::TooLarge { cap });
            }
            if bytes.len() as i64 != row.size_bytes {
                return Err(StageUploadError::Invalid(
                    "staged upload size mismatch".into(),
                ));
            }
            Ok(ClaimedUpload {
                upload_id,
                kind: Self::to_api_kind(row.kind),
                filename: row.filename,
                content_type: row.content_type,
                size_bytes: bytes.len() as u64,
                bytes,
            })
        }
        .await;
        if result.is_err()
            && self
                .store
                .release_sending_media_upload(account_id, upload_id)
                .await
                .is_err()
        {
            tracing::warn!(%account_id, %upload_id, "failed to release unreadable or over-limit staged upload");
        }
        result
    }
    pub fn new(store: Store, config: &MediaConfig) -> anyhow::Result<Self> {
        anyhow::ensure!(
            config.max_concurrent_uploads > 0,
            "media.max_concurrent_uploads must be greater than zero"
        );
        anyhow::ensure!(
            config.staged_upload_ttl_secs > 0,
            "media.staged_upload_ttl_secs must be greater than zero"
        );
        Ok(Self {
            store,
            root: config.uploads_dir.clone(),
            max_upload_bytes: config.max_upload_bytes,
            upload_timeout: Duration::from_secs(config.upload_request_timeout_secs),
            ttl: Duration::from_secs(config.staged_upload_ttl_secs),
            concurrent_uploads: Arc::new(Semaphore::new(config.max_concurrent_uploads)),
            concurrent_uploads_total: config.max_concurrent_uploads,
        })
    }

    async fn stage_upload_inner(
        &self,
        request: StageUploadRequest,
        mut body: UploadStream,
    ) -> Result<StagedUpload, StageUploadError> {
        let account = self
            .store
            .get_account(request.account_id)
            .await
            .map_err(internal)?;
        let Some(account) = account else {
            return Err(StageUploadError::NotFound("account not found".to_owned()));
        };
        if account.state != AccountState::Active {
            return Err(StageUploadError::Forbidden(format!(
                "account {} is not active",
                request.account_id
            )));
        }

        let upload_id = Uuid::new_v4();
        let account_dir = self.root.join(request.account_id.to_string());
        tokio::fs::create_dir_all(&account_dir)
            .await
            .map_err(internal)?;
        let tmp_path = account_dir.join(format!(".{upload_id}.tmp"));
        let final_path = account_dir.join(upload_id.to_string());
        let mut cleanup = CleanupFileGuard::new(tmp_path.clone());
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .await
            .map_err(internal)?;

        let mut size = 0_u64;
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|err| StageUploadError::Invalid(err.to_string()))?;
            size = size
                .checked_add(chunk.len() as u64)
                .ok_or(StageUploadError::TooLarge {
                    cap: self.max_upload_bytes,
                })?;
            if size > self.max_upload_bytes {
                return Err(StageUploadError::TooLarge {
                    cap: self.max_upload_bytes,
                });
            }
            file.write_all(&chunk).await.map_err(internal)?;
        }
        file.sync_all().await.map_err(internal)?;
        drop(file);

        tokio::fs::rename(&tmp_path, &final_path)
            .await
            .map_err(internal)?;
        cleanup.move_to(final_path.clone());
        let path = final_path_to_string(&final_path)?;
        let expires_at = Utc::now()
            + TimeDelta::from_std(self.ttl)
                .map_err(|err| StageUploadError::Internal(err.to_string()))?;
        let row = self
            .store
            .insert_media_upload(&NewMediaUpload {
                upload_id,
                account_id: request.account_id,
                kind: match request.kind {
                    axon_api::MediaUploadKindDto::Image => MediaUploadKind::Image,
                    axon_api::MediaUploadKindDto::File => MediaUploadKind::File,
                },
                filename: &request.filename,
                content_type: request.content_type.as_deref(),
                size_bytes: i64::try_from(size).map_err(|_| StageUploadError::TooLarge {
                    cap: self.max_upload_bytes,
                })?,
                path: &path,
                expires_at,
            })
            .await
            .map_err(internal)?;
        cleanup.disarm();

        Ok(StagedUpload {
            upload_id: row.upload_id,
            kind: request.kind,
            filename: row.filename,
            content_type: row.content_type,
            size_bytes: row.size_bytes as u64,
            expires_at: row.expires_at.to_rfc3339(),
        })
    }

    fn to_api_kind(kind: MediaUploadKind) -> axon_api::MediaUploadKindDto {
        match kind {
            MediaUploadKind::Image => axon_api::MediaUploadKindDto::Image,
            MediaUploadKind::File => axon_api::MediaUploadKindDto::File,
        }
    }

    /// Boot-time reconcile for the M15c staged-upload leaks (ADR 0059, GH
    /// #286). Three independent sweeps, each resilient by construction — a
    /// failure is logged and never blocks boot:
    ///
    /// - Reset every row a crash left in `sending` back to `staged` (see
    ///   [`Store::reconcile_sending_media_uploads`]).
    /// - Remove staged-upload files under the uploads root with no matching
    ///   row in any state (see [`Self::prune_orphan_files`]).
    /// - Delete already-expired staged rows and unlink their files (see
    ///   [`Self::sweep_expired`]) — the same work
    ///   [`spawn_expiry_sweeper`] repeats periodically, but a boot-time crash
    ///   loop tighter than `EXPIRY_SWEEP_INTERVAL` would otherwise never
    ///   reach that first periodic tick.
    ///
    /// Must run before the HTTP listener binds, like every other boot
    /// reconcile in Axon: the reset and orphan-file sweeps assume no
    /// concurrent upload traffic, so a `.tmp` partial write (only ever
    /// transient mid-request) is always safe to remove.
    ///
    /// The reset-to-`staged` sweep runs before the expiry sweep, so a row
    /// that was mid-send at crash time *and* already past its original
    /// `expires_at` (claiming doesn't extend it) is reset and then
    /// immediately deleted with its file in the same `reconcile_boot` call,
    /// never surfacing as `staged` in between. Intentional at the default 24h
    /// TTL — a crash-wedged send is almost always well past due for a fresh
    /// upload attempt by the time axon restarts.
    pub async fn reconcile_boot(&self) {
        match self.store.reconcile_sending_media_uploads().await {
            Ok(0) => {}
            Ok(count) => tracing::info!(
                count,
                "staged-upload reconcile: reset stale 'sending' rows to 'staged'"
            ),
            Err(err) => tracing::error!(
                error = %err,
                "staged-upload reconcile: resetting 'sending' rows failed; skipping"
            ),
        }
        self.prune_orphan_files().await;
        self.sweep_expired().await;
    }

    /// Remove files under the uploads root with no matching `media_uploads`
    /// row in any state — the mirror of `axon-sync`'s
    /// `prune_orphan_store_dirs`/`prune_orphan_media_dirs`, keyed off row
    /// existence the same way. A `stage_upload` whose outer timeout fires
    /// after the rename is *not* what this catches — `CleanupFileGuard`
    /// still unlinks the final file when the cancelled future drops, since
    /// `disarm()` only runs after the row insert succeeds. What this catches
    /// is what the guard can't: a `.{uuid}.tmp` (or already-renamed,
    /// row-less) file left behind by a hard crash, where `Drop` never runs
    /// (`kill -9`), or by a failed unlink. Never returns an error — a GC
    /// failure must not block boot.
    async fn prune_orphan_files(&self) {
        let known: HashSet<(Uuid, Uuid)> = match self.store.list_media_upload_ids().await {
            Ok(ids) => ids.into_iter().collect(),
            Err(err) => {
                tracing::error!(
                    error = %err,
                    "staged-upload orphan scan: listing rows failed; skipping"
                );
                return;
            }
        };

        let mut account_dirs = match tokio::fs::read_dir(&self.root).await {
            Ok(entries) => entries,
            // No uploads dir yet (no upload has ever been staged) — nothing to prune.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
            Err(err) => {
                tracing::error!(
                    error = %err,
                    dir = %self.root.display(),
                    "staged-upload orphan scan: reading uploads dir failed; skipping"
                );
                return;
            }
        };

        loop {
            let account_entry = match account_dirs.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(err) => {
                    tracing::error!(
                        error = %err,
                        "staged-upload orphan scan: reading uploads dir entry failed; stopping"
                    );
                    break;
                }
            };

            let name = account_entry.file_name();
            // Non-UTF-8 or non-uuid names can't be one of our account dirs — leave them.
            let Some(name) = name.to_str() else { continue };
            let Ok(account_id) = Uuid::parse_str(name) else {
                continue;
            };
            match account_entry.file_type().await {
                Ok(ft) if ft.is_dir() => {}
                Ok(_) => continue,
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        entry = %account_entry.path().display(),
                        "staged-upload orphan scan: stat failed; skipping"
                    );
                    continue;
                }
            }

            prune_orphan_files_in_account_dir(&account_entry.path(), account_id, &known).await;
        }
    }
}

/// Remove row-less files inside one account's upload directory, then the
/// directory itself if that leaves it empty. Split out of
/// [`FilesystemStagedUploads::prune_orphan_files`] to keep each loop level
/// flat; see that method's docs for what "row-less" covers.
async fn prune_orphan_files_in_account_dir(
    dir: &Path,
    account_id: Uuid,
    known: &HashSet<(Uuid, Uuid)>,
) {
    let mut files = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(err) => {
            tracing::warn!(
                error = %err,
                dir = %dir.display(),
                "staged-upload orphan scan: reading account dir failed; skipping"
            );
            return;
        }
    };
    loop {
        let file_entry = match files.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    dir = %dir.display(),
                    "staged-upload orphan scan: reading account dir entry failed; stopping"
                );
                break;
            }
        };

        let file_name = file_entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        // A finalized upload file is `<uuid>`; a partial write in progress is
        // `.{uuid}.tmp` (see `stage_upload_inner`).
        let id_str = file_name
            .strip_prefix('.')
            .and_then(|rest| rest.strip_suffix(".tmp"))
            .unwrap_or(file_name);
        let Ok(upload_id) = Uuid::parse_str(id_str) else {
            continue;
        };
        if known.contains(&(account_id, upload_id)) {
            continue;
        }

        let path = file_entry.path();
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {
                tracing::info!(path = %path.display(), "staged-upload orphan scan: pruned row-less file")
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => tracing::warn!(
                error = %err,
                path = %path.display(),
                "staged-upload orphan scan: failed to prune file"
            ),
        }
    }

    // Reclaim the account dir itself once every file under it is gone —
    // otherwise a long-deleted account (whose row, and so every upload row
    // under it, cascaded away) leaves an empty `<account_id>/` behind
    // forever. `stage_upload_inner` recreates it on demand, so this is safe
    // even for a still-active account that simply has no staged uploads
    // right now. `DirectoryNotEmpty` is the expected/common case (a live
    // upload, or a non-uuid stray file, is still in there) — not an error.
    match tokio::fs::remove_dir(dir).await {
        Ok(()) => {
            tracing::info!(dir = %dir.display(), "staged-upload orphan scan: pruned empty account dir")
        }
        Err(err) if err.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => tracing::warn!(
            error = %err,
            dir = %dir.display(),
            "staged-upload orphan scan: failed to prune empty account dir"
        ),
    }
}

#[async_trait]
impl StagedUploadService for FilesystemStagedUploads {
    async fn claim_preview_upload(
        &self,
        account_id: Uuid,
        upload_id: Uuid,
    ) -> Result<axon_core::link_preview::PreviewImage, StageUploadError> {
        let permit = tokio::time::timeout(
            self.upload_timeout,
            self.concurrent_uploads.clone().acquire_owned(),
        )
        .await
        .map_err(|_| StageUploadError::Timeout("preview decoder capacity timed out".into()))?
        .map_err(|_| StageUploadError::Internal("preview decoder capacity closed".into()))?;
        let upload = self
            .claim_bounded(
                account_id,
                upload_id,
                axon_core::link_preview::MAX_IMAGE_BYTES,
            )
            .await?;
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            validate_preview_image(upload)
        })
        .await
        .map_err(|_| StageUploadError::Internal("preview decoder task failed".into()))
        .and_then(|value| value);
        if result.is_err()
            && self
                .store
                .release_sending_media_upload(account_id, upload_id)
                .await
                .is_err()
        {
            tracing::warn!(%account_id, %upload_id, "failed to release invalid preview image");
        }
        result
    }
    async fn stage_upload(
        &self,
        request: StageUploadRequest,
        body: UploadStream,
    ) -> Result<StagedUpload, StageUploadError> {
        let account_id = request.account_id;
        let queued_at = Instant::now();
        let available_permits = self.concurrent_uploads.available_permits();
        let _permit = self
            .concurrent_uploads
            .clone()
            .acquire_owned()
            .await
            .map_err(|err| StageUploadError::Internal(err.to_string()))?;
        let wait = queued_at.elapsed();
        // Anything more than trivial wait means every permit was in use — worth
        // knowing even on success, since it's the leading signal that uploads are
        // backing up (a slow/stuck upload elsewhere, or genuine load).
        if wait > Duration::from_millis(50) {
            tracing::warn!(
                account_id = %account_id,
                wait_ms = wait.as_millis(),
                available_permits_before_wait = available_permits,
                max_concurrent_uploads = self.concurrent_uploads_total,
                "stage_upload waited for a concurrent-upload permit"
            );
        }
        tracing::debug!(account_id = %account_id, filename = %request.filename, "stage_upload: permit acquired, starting write");
        let started_at = Instant::now();
        let result = tokio::time::timeout(self.upload_timeout, self.stage_upload_inner(request, body))
            .await
            .map_err(|_| {
                tracing::warn!(
                    account_id = %account_id,
                    elapsed_ms = started_at.elapsed().as_millis(),
                    timeout_secs = self.upload_timeout.as_secs(),
                    "stage_upload timed out — request never completed within upload_request_timeout_secs"
                );
                StageUploadError::Timeout(format!(
                    "upload timed out after {}s",
                    self.upload_timeout.as_secs()
                ))
            })?;
        match &result {
            Ok(staged) => tracing::info!(
                account_id = %account_id,
                upload_id = %staged.upload_id,
                size_bytes = staged.size_bytes,
                elapsed_ms = started_at.elapsed().as_millis(),
                "stage_upload completed"
            ),
            Err(err) => tracing::warn!(
                account_id = %account_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                error = ?err,
                "stage_upload failed"
            ),
        }
        result
    }

    async fn delete_upload(
        &self,
        account_id: Uuid,
        upload_id: Uuid,
    ) -> Result<(), StageUploadError> {
        let Some(row) = self
            .store
            .delete_staged_media_upload(account_id, upload_id)
            .await
            .map_err(internal)?
        else {
            return Err(StageUploadError::NotFound("upload not found".to_owned()));
        };
        remove_file_if_exists(Path::new(&row.path)).await;
        Ok(())
    }

    async fn claim_upload(
        &self,
        account_id: Uuid,
        upload_id: Uuid,
    ) -> Result<ClaimedUpload, StageUploadError> {
        self.claim_bounded(account_id, upload_id, self.max_upload_bytes)
            .await
    }

    async fn complete_upload(
        &self,
        account_id: Uuid,
        upload_id: Uuid,
    ) -> Result<(), StageUploadError> {
        let Some(row) = self
            .store
            .complete_sending_media_upload(account_id, upload_id)
            .await
            .map_err(internal)?
        else {
            return Err(StageUploadError::NotFound("upload not found".to_owned()));
        };
        remove_file_if_exists(Path::new(&row.path)).await;
        Ok(())
    }

    async fn release_upload(
        &self,
        account_id: Uuid,
        upload_id: Uuid,
    ) -> Result<(), StageUploadError> {
        let Some(_row) = self
            .store
            .release_sending_media_upload(account_id, upload_id)
            .await
            .map_err(internal)?
        else {
            return Err(StageUploadError::NotFound("upload not found".to_owned()));
        };
        Ok(())
    }
}

impl FilesystemStagedUploads {
    /// Delete every staged row whose TTL has passed and unlink its file
    /// (M15c, ADR 0059, GH #286): an abandoned staged upload — crash, lost
    /// network, abandoned compose — is never claimed, so nothing else ever
    /// removes it. Called periodically by [`spawn_expiry_sweeper`]; safe to
    /// run at any time (unlike [`Self::reconcile_boot`]) because the DELETE
    /// only ever matches `staged` rows, never `sending`, so an in-flight send
    /// can't be deleted out from under `complete_upload`/`release_upload`.
    pub async fn sweep_expired(&self) {
        let rows = match self.store.delete_expired_media_uploads().await {
            Ok(rows) => rows,
            Err(err) => {
                tracing::error!(
                    error = %err,
                    "staged-upload expiry sweep: query failed; skipping"
                );
                return;
            }
        };
        if rows.is_empty() {
            return;
        }
        tracing::info!(
            count = rows.len(),
            "staged-upload expiry sweep: pruning expired staged uploads"
        );
        for row in rows {
            remove_file_if_exists(Path::new(&row.path)).await;
        }
    }
}

/// Upper bound on how often [`spawn_expiry_sweeper`] reclaims expired staged
/// uploads (the actual interval is also capped by the configured TTL, down to
/// [`MIN_EXPIRY_SWEEP_INTERVAL`] — see its use in `spawn_expiry_sweeper`).
/// Comfortably fine-grained against the default 24h TTL
/// (`media.staged_upload_ttl_secs`) without being chatty.
const EXPIRY_SWEEP_INTERVAL: Duration = Duration::from_secs(300);

/// Floor on the TTL-capped sweep interval (see [`spawn_expiry_sweeper`]).
/// `MediaConfig::staged_upload_ttl_secs` is only validated `> 0` — without
/// this floor, a misconfigured TTL of e.g. 1s would run a `DELETE` against
/// Postgres every second for the life of the process.
const MIN_EXPIRY_SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// Spawn the background task that periodically sweeps `uploads`'s expired
/// staged rows (see [`FilesystemStagedUploads::sweep_expired`]). Not part of
/// graceful shutdown — like the oauth rate-limit sweeper and the fd
/// diagnostics logger, this is pure maintenance with no state to drain, so
/// it's fine for it to simply stop when the process exits.
pub fn spawn_expiry_sweeper(uploads: Arc<FilesystemStagedUploads>) {
    // A short `media.staged_upload_ttl_secs` should reclaim on roughly its own
    // cadence rather than waiting up to a full `EXPIRY_SWEEP_INTERVAL`; a long
    // (or default 24h) one is unaffected, since claiming is already gated on
    // `expires_at` regardless of when the sweep next runs. Floored at
    // `MIN_EXPIRY_SWEEP_INTERVAL` so a very short TTL can't turn this into a
    // tight polling loop against Postgres.
    let interval = EXPIRY_SWEEP_INTERVAL
        .min(uploads.ttl)
        .max(MIN_EXPIRY_SWEEP_INTERVAL);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        // Unlike the in-memory `oauth::rate_limit::spawn_sweeper` this precedent
        // is modeled on, each tick here awaits DB + filesystem I/O, so a slow
        // sweep should skip backlogged ticks rather than firing them back-to-back.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tick.tick().await; // first tick fires immediately; skip it, we just booted
        loop {
            tick.tick().await;
            uploads.sweep_expired().await;
        }
    });
}

fn final_path_to_string(path: &Path) -> Result<String, StageUploadError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| StageUploadError::Internal("upload path is not valid UTF-8".to_owned()))
}

fn internal(err: impl std::fmt::Display) -> StageUploadError {
    StageUploadError::Internal(err.to_string())
}

struct CleanupFileGuard {
    path: PathBuf,
    armed: bool,
}

impl CleanupFileGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn move_to(&mut self, path: PathBuf) {
        self.path = path;
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CleanupFileGuard {
    fn drop(&mut self) {
        if self.armed {
            schedule_remove_file(self.path.clone());
        }
    }
}

fn schedule_remove_file(path: PathBuf) {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            let _cleanup = handle.spawn_blocking(move || remove_file_if_exists_sync(&path));
        }
        Err(_) => {
            let _cleanup = std::thread::spawn(move || remove_file_if_exists_sync(&path));
        }
    };
}

fn remove_file_if_exists_sync(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            tracing::warn!(path = %path.display(), error = %err, "staged upload cleanup failed")
        }
    }
}

async fn remove_file_if_exists(path: &Path) {
    match tokio::fs::remove_file(path).await {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            tracing::warn!(path = %path.display(), error = %err, "staged upload cleanup failed")
        }
    }
}

#[cfg(test)]
mod tests {
    fn preview_png() -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2, 2)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    fn preview_upload(bytes: Vec<u8>) -> axon_api::ClaimedUpload {
        axon_api::ClaimedUpload {
            upload_id: uuid::Uuid::new_v4(),
            kind: axon_api::MediaUploadKindDto::Image,
            filename: "preview.png".into(),
            content_type: Some("image/png".into()),
            size_bytes: bytes.len() as u64,
            bytes,
        }
    }

    #[test]
    fn preview_decoder_normalizes_case_parameters_and_jpeg_aliases() {
        for (format, canonical, declarations) in [
            (
                image::ImageFormat::Png,
                "image/png",
                vec!["IMAGE/PNG", "image/PNG; charset=binary"],
            ),
            (
                image::ImageFormat::Jpeg,
                "image/jpeg",
                vec!["image/JPEG", "image/jpg", "IMAGE/JPG; x=test"],
            ),
            (image::ImageFormat::WebP, "image/webp", vec!["IMAGE/WEBP"]),
        ] {
            let mut out = std::io::Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(2, 2)
                .write_to(&mut out, format)
                .unwrap();
            for declared in declarations {
                let mut upload = preview_upload(out.get_ref().clone());
                upload.content_type = Some(declared.into());
                assert_eq!(
                    super::validate_preview_image(upload).unwrap().mimetype,
                    canonical
                );
            }
        }
    }

    #[test]
    fn preview_decoder_checks_type_integrity_and_pixel_and_byte_budgets() {
        let image = super::validate_preview_image(preview_upload(preview_png())).unwrap();
        assert_eq!((image.width, image.height), (2, 2));
        assert_eq!(image.mimetype, "image/png");
        let mut bad = preview_upload(preview_png());
        bad.content_type = Some("image/jpeg".into());
        assert!(super::validate_preview_image(bad).is_err());
        assert!(
            super::validate_preview_image(preview_upload(b"<svg onload='x'>".to_vec())).is_err()
        );
        assert!(
            super::validate_preview_image(preview_upload(vec![0; 2 * 1024 * 1024 + 1])).is_err()
        );
        let mut large = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_luma8(2001, 2000)
            .write_to(&mut large, image::ImageFormat::Png)
            .unwrap();
        assert!(super::validate_preview_image(preview_upload(large.into_inner())).is_err());
        let mut truncated = preview_png();
        truncated.truncate(40);
        assert!(super::validate_preview_image(preview_upload(truncated)).is_err());
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn preview_claim_checks_account_and_releases_rejected_images() {
        use axon_api::StagedUploadService;
        let store = connect_store().await;
        let owner = test_account(&store).await;
        let other = test_account(&store).await;
        let dir = tempfile::tempdir().unwrap();
        let service = uploads(store.clone(), dir.path().into());
        let request = axon_api::StageUploadRequest {
            account_id: owner,
            kind: axon_api::MediaUploadKindDto::Image,
            filename: "preview.png".into(),
            content_type: Some("image/png".into()),
        };
        let upload = service
            .stage_upload(
                request,
                Box::pin(futures_util::stream::once(async {
                    Ok(preview_png().into())
                })),
            )
            .await
            .unwrap();
        assert!(service
            .claim_preview_upload(other, upload.upload_id)
            .await
            .is_err());
        let image = service
            .claim_preview_upload(owner, upload.upload_id)
            .await
            .unwrap();
        assert_eq!((image.width, image.height), (2, 2));
        service
            .release_upload(owner, upload.upload_id)
            .await
            .unwrap();
        // Actual bytes are capped even when stale/tampered DB metadata says small.
        let row = store
            .get_media_upload(owner, upload.upload_id)
            .await
            .unwrap()
            .unwrap();
        tokio::fs::write(&row.path, vec![0; 2 * 1024 * 1024 + 1])
            .await
            .unwrap();
        assert!(service
            .claim_preview_upload(owner, upload.upload_id)
            .await
            .is_err());
        assert_eq!(
            store
                .get_media_upload(owner, upload.upload_id)
                .await
                .unwrap()
                .unwrap()
                .state,
            axon_store::MediaUploadState::Staged
        );
    }
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use uuid::Uuid;

    use super::CleanupFileGuard;

    #[tokio::test]
    async fn cleanup_guard_removes_armed_file_on_drop() {
        let path = std::env::temp_dir().join(format!("axon-upload-guard-{}", Uuid::new_v4()));
        std::fs::File::create(&path)
            .expect("create temp")
            .write_all(b"partial")
            .expect("write temp");

        {
            let _guard = CleanupFileGuard::new(path.clone());
        }

        wait_for_removal(&path).await;
        assert!(!path.exists(), "armed guard removes partial staged file");
    }

    #[test]
    fn cleanup_guard_disarm_keeps_file() {
        let path = std::env::temp_dir().join(format!("axon-upload-guard-{}", Uuid::new_v4()));
        std::fs::File::create(&path)
            .expect("create temp")
            .write_all(b"complete")
            .expect("write temp");

        {
            let mut guard = CleanupFileGuard::new(path.clone());
            guard.disarm();
        }

        assert!(path.exists(), "disarmed guard leaves committed file");
        std::fs::remove_file(&path).expect("cleanup temp");
    }

    async fn wait_for_removal(path: &Path) {
        for _ in 0..20 {
            if !path.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn connect_store() -> axon_store::Store {
        let url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for integration tests");
        axon_store::Store::connect(&url, 5)
            .await
            .expect("connect + migrate")
    }

    async fn test_account(store: &axon_store::Store) -> Uuid {
        let user = format!("@upload-reconcile-{}:localhost", Uuid::new_v4());
        store
            .upsert_account(&user, "https://hs.example.org")
            .await
            .expect("account")
            .account_id
    }

    fn uploads(store: axon_store::Store, root: PathBuf) -> super::FilesystemStagedUploads {
        super::FilesystemStagedUploads::new(
            store,
            &axon_core::MediaConfig {
                uploads_dir: root,
                ..axon_core::MediaConfig::default()
            },
        )
        .expect("configure staged uploads")
    }

    /// The M15c boot reconcile (GH #286) resets a `sending` row left wedged by
    /// a crash back to `staged`, so it becomes claimable/deletable again
    /// instead of being permanently stuck.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn reconcile_boot_resets_wedged_sending_rows() {
        let store = connect_store().await;
        let account_id = test_account(&store).await;
        let root = std::env::temp_dir().join(format!("axon-uploads-reconcile-{}", Uuid::new_v4()));
        let uploads = uploads(store.clone(), root.clone());

        let account_dir = root.join(account_id.to_string());
        tokio::fs::create_dir_all(&account_dir).await.unwrap();
        let upload_id = Uuid::new_v4();
        let path = account_dir.join(upload_id.to_string());
        tokio::fs::write(&path, b"file bytes").await.unwrap();
        store
            .insert_media_upload(&axon_store::NewMediaUpload {
                upload_id,
                account_id,
                kind: axon_store::MediaUploadKind::File,
                filename: "file.bin",
                content_type: None,
                size_bytes: 10,
                path: path.to_str().unwrap(),
                expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
            })
            .await
            .expect("insert upload");
        store
            .claim_staged_media_upload(account_id, upload_id)
            .await
            .expect("claim upload")
            .expect("claimed row");

        uploads.reconcile_boot().await;

        let row = store
            .get_media_upload(account_id, upload_id)
            .await
            .expect("get upload")
            .expect("row still present");
        assert_eq!(row.state, axon_store::MediaUploadState::Staged);
        assert!(path.exists(), "the claimed row's file is untouched");

        store.delete_account_row(account_id).await.expect("cleanup");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The M15c orphan-file scan (GH #286) removes a staged-upload file with
    /// no matching row in any state — including a `.{uuid}.tmp` partial write
    /// left behind by a hard crash mid-`stage_upload` — while leaving a file
    /// whose row still exists (even mid-`sending`) untouched.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn reconcile_boot_prunes_orphan_files_only() {
        let store = connect_store().await;
        let account_id = test_account(&store).await;
        let root = std::env::temp_dir().join(format!("axon-uploads-orphan-{}", Uuid::new_v4()));
        let uploads = uploads(store.clone(), root.clone());

        let account_dir = root.join(account_id.to_string());
        tokio::fs::create_dir_all(&account_dir).await.unwrap();

        // A live row's file — must survive.
        let live_id = Uuid::new_v4();
        let live_path = account_dir.join(live_id.to_string());
        tokio::fs::write(&live_path, b"live").await.unwrap();
        store
            .insert_media_upload(&axon_store::NewMediaUpload {
                upload_id: live_id,
                account_id,
                kind: axon_store::MediaUploadKind::File,
                filename: "live.bin",
                content_type: None,
                size_bytes: 4,
                path: live_path.to_str().unwrap(),
                expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
            })
            .await
            .expect("insert live upload");

        // A finalized file with no row at all (e.g. a hard crash after the
        // rename but before the row insert landed) — pruned.
        let orphan_id = Uuid::new_v4();
        let orphan_path = account_dir.join(orphan_id.to_string());
        tokio::fs::write(&orphan_path, b"orphan").await.unwrap();

        // A partial write left by a hard crash before the row was ever inserted — pruned.
        let tmp_path = account_dir.join(format!(".{}.tmp", Uuid::new_v4()));
        tokio::fs::write(&tmp_path, b"partial").await.unwrap();

        // A non-uuid stray file — untouched.
        let stray_path = account_dir.join("not-a-uuid.txt");
        tokio::fs::write(&stray_path, b"keep me").await.unwrap();

        uploads.reconcile_boot().await;

        assert!(live_path.exists(), "live row's file kept");
        assert!(!orphan_path.exists(), "row-less file pruned");
        assert!(!tmp_path.exists(), "orphaned partial write pruned");
        assert!(stray_path.exists(), "non-uuid file untouched");

        store.delete_account_row(account_id).await.expect("cleanup");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The orphan-file scan also reclaims an account dir once pruning empties
    /// it out (e.g. a long-deleted account whose row, and so every upload row
    /// under it, cascaded away) — otherwise it would sit there forever, since
    /// nothing else ever removes it.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn reconcile_boot_prunes_emptied_account_dir() {
        let store = connect_store().await;
        // Not inserted as a real account row — stands in for a long-deleted
        // account whose row (and cascaded upload rows) are already gone.
        let account_id = Uuid::new_v4();
        let root = std::env::temp_dir().join(format!("axon-uploads-emptydir-{}", Uuid::new_v4()));
        let uploads = uploads(store.clone(), root.clone());

        let account_dir = root.join(account_id.to_string());
        tokio::fs::create_dir_all(&account_dir).await.unwrap();
        let orphan_path = account_dir.join(Uuid::new_v4().to_string());
        tokio::fs::write(&orphan_path, b"orphan").await.unwrap();

        uploads.reconcile_boot().await;

        assert!(
            !account_dir.exists(),
            "emptied account dir reclaimed, not left behind"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The M15c periodic sweep (GH #286) deletes an expired `staged` row and
    /// unlinks its file, but leaves an expired-but-`sending` row (an
    /// in-flight send whose original TTL happened to lapse) fully alone.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn sweep_expired_deletes_expired_staged_rows_only() {
        let store = connect_store().await;
        let account_id = test_account(&store).await;
        let root = std::env::temp_dir().join(format!("axon-uploads-sweep-{}", Uuid::new_v4()));
        let uploads = uploads(store.clone(), root.clone());

        let account_dir = root.join(account_id.to_string());
        tokio::fs::create_dir_all(&account_dir).await.unwrap();

        let expired_id = Uuid::new_v4();
        let expired_path = account_dir.join(expired_id.to_string());
        tokio::fs::write(&expired_path, b"expired").await.unwrap();
        store
            .insert_media_upload(&axon_store::NewMediaUpload {
                upload_id: expired_id,
                account_id,
                kind: axon_store::MediaUploadKind::File,
                filename: "expired.bin",
                content_type: None,
                size_bytes: 7,
                path: expired_path.to_str().unwrap(),
                expires_at: chrono::Utc::now() - chrono::Duration::minutes(1),
            })
            .await
            .expect("insert expired upload");

        // An in-flight send whose original TTL has since lapsed (like a slow
        // real-world send outliving its `expires_at`). Set directly via a raw
        // UPDATE rather than `claim_staged_media_upload` + sleep — claiming
        // requires `expires_at > now()`, so racing the row's own expiry
        // against a sleep would make this test flaky under load; the store
        // test for the same behavior (`delete_expired_media_uploads_prunes_staged_but_spares_sending`)
        // uses the same raw-UPDATE approach.
        let sending_id = Uuid::new_v4();
        let sending_path = account_dir.join(sending_id.to_string());
        tokio::fs::write(&sending_path, b"sending").await.unwrap();
        store
            .insert_media_upload(&axon_store::NewMediaUpload {
                upload_id: sending_id,
                account_id,
                kind: axon_store::MediaUploadKind::File,
                filename: "sending.bin",
                content_type: None,
                size_bytes: 7,
                path: sending_path.to_str().unwrap(),
                expires_at: chrono::Utc::now() - chrono::Duration::minutes(1),
            })
            .await
            .expect("insert sending upload");
        sqlx_core::query::query(
            "UPDATE media_uploads SET state = 'sending' WHERE account_id = $1 AND upload_id = $2",
        )
        .bind(account_id)
        .bind(sending_id)
        .execute(store.pool())
        .await
        .expect("mark sending");

        uploads.sweep_expired().await;

        assert!(
            store
                .get_media_upload(account_id, expired_id)
                .await
                .expect("get expired")
                .is_none(),
            "expired staged row deleted"
        );
        assert!(!expired_path.exists(), "expired staged row's file unlinked");
        assert!(
            store
                .get_media_upload(account_id, sending_id)
                .await
                .expect("get sending")
                .is_some(),
            "expired-but-sending row survives"
        );
        assert!(sending_path.exists(), "sending row's file untouched");

        store.delete_account_row(account_id).await.expect("cleanup");
        let _ = std::fs::remove_dir_all(&root);
    }
}

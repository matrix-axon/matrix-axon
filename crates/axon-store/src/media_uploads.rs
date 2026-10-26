//! Metadata for staged client-originated media uploads (M15a, ADR 0059).
//!
//! The bytes live on disk under the configured upload staging directory; this
//! table is the durable index that lets Axon recover pending local mutations
//! after restart. Filesystem cleanup is owned by the composition-root staging
//! service, while the store keeps the account-scoped metadata transactional.
//!
//! M15c (GH #286) adds the pruning/reconciliation leg:
//! [`Store::reconcile_sending_media_uploads`] and [`Store::list_media_upload_ids`]
//! back the composition-root's boot reconcile, and
//! [`Store::delete_expired_media_uploads`] backs its periodic expiry sweep.

use chrono::{DateTime, Utc};
use sqlx_core::row::Row;
use sqlx_postgres::{PgRow, Postgres};
use uuid::Uuid;

use crate::{Store, StoreError};

/// Supported outbound media message shape for a staged upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaUploadKind {
    Image,
    File,
}

impl MediaUploadKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MediaUploadKind::Image => "image",
            MediaUploadKind::File => "file",
        }
    }

    fn from_db(s: &str) -> Result<Self, sqlx_core::Error> {
        match s {
            "image" => Ok(MediaUploadKind::Image),
            "file" => Ok(MediaUploadKind::File),
            other => Err(sqlx_core::Error::ColumnDecode {
                index: "kind".to_owned(),
                source: format!("unknown media upload kind {other:?}").into(),
            }),
        }
    }
}

/// Durable state of a staged upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaUploadState {
    Staged,
    Sending,
}

impl MediaUploadState {
    pub fn as_str(self) -> &'static str {
        match self {
            MediaUploadState::Staged => "staged",
            MediaUploadState::Sending => "sending",
        }
    }

    fn from_db(s: &str) -> Result<Self, sqlx_core::Error> {
        match s {
            "staged" => Ok(MediaUploadState::Staged),
            "sending" => Ok(MediaUploadState::Sending),
            other => Err(sqlx_core::Error::ColumnDecode {
                index: "state".to_owned(),
                source: format!("unknown media upload state {other:?}").into(),
            }),
        }
    }
}

/// New staged-upload metadata. The referenced file must already be durable on
/// disk before this row is inserted.
pub struct NewMediaUpload<'a> {
    pub upload_id: Uuid,
    pub account_id: Uuid,
    pub kind: MediaUploadKind,
    pub filename: &'a str,
    pub content_type: Option<&'a str>,
    pub size_bytes: i64,
    pub path: &'a str,
    pub expires_at: DateTime<Utc>,
}

/// One staged-upload metadata row.
#[derive(Debug, Clone)]
pub struct MediaUpload {
    pub upload_id: Uuid,
    pub account_id: Uuid,
    pub kind: MediaUploadKind,
    pub filename: String,
    pub content_type: Option<String>,
    pub size_bytes: i64,
    pub path: String,
    pub state: MediaUploadState,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl sqlx_core::from_row::FromRow<'_, PgRow> for MediaUpload {
    fn from_row(row: &PgRow) -> Result<Self, sqlx_core::Error> {
        let kind: String = row.try_get("kind")?;
        let state: String = row.try_get("state")?;
        Ok(MediaUpload {
            upload_id: row.try_get("upload_id")?,
            account_id: row.try_get("account_id")?,
            kind: MediaUploadKind::from_db(&kind)?,
            filename: row.try_get("filename")?,
            content_type: row.try_get("content_type")?,
            size_bytes: row.try_get("size_bytes")?,
            path: row.try_get("path")?,
            state: MediaUploadState::from_db(&state)?,
            expires_at: row.try_get("expires_at")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

const MEDIA_UPLOAD_COLUMNS: &str = "upload_id, account_id, kind, filename, content_type, \
    size_bytes, path, state, expires_at, created_at, updated_at";

impl Store {
    /// Insert a newly staged upload. The filesystem service writes and renames
    /// the file first; this row is the durable index that makes it discoverable.
    pub async fn insert_media_upload(
        &self,
        upload: &NewMediaUpload<'_>,
    ) -> Result<MediaUpload, StoreError> {
        let mut connection = self.pool.acquire().await?;
        insert_upload(&mut connection, upload, false).await
    }

    /// A transaction-scoped account lock makes quota check + insertion atomic
    /// across every server task/process, including restart. Expired rows count
    /// until the sweeper removes their disk obligations.
    pub async fn insert_generated_preview_upload(
        &self,
        upload: &NewMediaUpload<'_>,
        max_count: i64,
        max_bytes: i64,
    ) -> Result<Option<MediaUpload>, StoreError> {
        let mut tx = self.pool.begin().await?;
        sqlx_core::query::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 822))")
            .bind(upload.account_id.to_string())
            .execute(&mut *tx)
            .await?;
        let (count, bytes): (i64,i64) = sqlx_core::query_as::query_as("SELECT count(*), COALESCE(sum(size_bytes), 0)::bigint FROM media_uploads WHERE account_id = $1 AND generated_preview")
            .bind(upload.account_id).fetch_one(&mut *tx).await?;
        if count >= max_count || upload.size_bytes > max_bytes.saturating_sub(bytes) {
            tx.rollback().await?;
            return Ok(None);
        }
        let row = insert_upload(&mut tx, upload, true).await?;
        tx.commit().await?;
        Ok(Some(row))
    }

    /// Fetch an account-scoped upload by id.
    pub async fn get_media_upload(
        &self,
        account_id: Uuid,
        upload_id: Uuid,
    ) -> Result<Option<MediaUpload>, StoreError> {
        let sql = format!(
            "SELECT {MEDIA_UPLOAD_COLUMNS} FROM media_uploads \
             WHERE account_id = $1 AND upload_id = $2"
        );
        let row = sqlx_core::query_as::query_as::<Postgres, MediaUpload>(&sql)
            .bind(account_id)
            .bind(upload_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    /// Delete a staged upload row, returning its metadata so the caller can
    /// unlink the file. `sending` rows are deliberately not deleted here; M15b
    /// will own that transition and consume-on-success behavior.
    pub async fn delete_staged_media_upload(
        &self,
        account_id: Uuid,
        upload_id: Uuid,
    ) -> Result<Option<MediaUpload>, StoreError> {
        let sql = format!(
            "DELETE FROM media_uploads \
             WHERE account_id = $1 AND upload_id = $2 AND state = 'staged' \
             RETURNING {MEDIA_UPLOAD_COLUMNS}"
        );
        let row = sqlx_core::query_as::query_as::<Postgres, MediaUpload>(&sql)
            .bind(account_id)
            .bind(upload_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    /// Claim a staged upload for sending. The state transition is atomic, so two
    /// clients cannot send the same upload concurrently. Expired uploads are not
    /// claimable; [`delete_expired_media_uploads`](Self::delete_expired_media_uploads)
    /// is what eventually prunes them (M15c).
    pub async fn claim_staged_media_upload(
        &self,
        account_id: Uuid,
        upload_id: Uuid,
    ) -> Result<Option<MediaUpload>, StoreError> {
        let sql = format!(
            "UPDATE media_uploads \
             SET state = 'sending' \
             WHERE account_id = $1 \
               AND upload_id = $2 \
               AND state = 'staged' \
               AND expires_at > now() \
             RETURNING {MEDIA_UPLOAD_COLUMNS}"
        );
        let row = sqlx_core::query_as::query_as::<Postgres, MediaUpload>(&sql)
            .bind(account_id)
            .bind(upload_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    /// Delete a claimed upload after the Matrix event has been sent, returning
    /// its path for filesystem cleanup.
    pub async fn complete_sending_media_upload(
        &self,
        account_id: Uuid,
        upload_id: Uuid,
    ) -> Result<Option<MediaUpload>, StoreError> {
        let sql = format!(
            "DELETE FROM media_uploads \
             WHERE account_id = $1 AND upload_id = $2 AND state = 'sending' \
             RETURNING {MEDIA_UPLOAD_COLUMNS}"
        );
        let row = sqlx_core::query_as::query_as::<Postgres, MediaUpload>(&sql)
            .bind(account_id)
            .bind(upload_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    /// Release a claimed upload after a send failure so the caller can retry
    /// from a clean staged state.
    pub async fn release_sending_media_upload(
        &self,
        account_id: Uuid,
        upload_id: Uuid,
    ) -> Result<Option<MediaUpload>, StoreError> {
        let sql = format!(
            "UPDATE media_uploads \
             SET state = 'staged' \
             WHERE account_id = $1 AND upload_id = $2 AND state = 'sending' \
             RETURNING {MEDIA_UPLOAD_COLUMNS}"
        );
        let row = sqlx_core::query_as::query_as::<Postgres, MediaUpload>(&sql)
            .bind(account_id)
            .bind(upload_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    /// Reset every row a crash left in `sending` back to `staged` (M15c, ADR
    /// 0059, GH #286). `claim_upload` flips a row to `sending` before reading
    /// its file and driving the Matrix send; if axon dies before
    /// `complete_upload`/`release_upload` runs, no API path ever deletes or
    /// resets a `sending` row, so it would otherwise wedge forever. Meant to
    /// be called once at boot, before any client traffic, over every account
    /// at once — a wedged row from a stale process has no per-account caller
    /// left to release it. Returns the number of rows reset.
    ///
    /// Unconditionally global, with no process/instance scoping: this
    /// assumes a single axon process is ever live against a given database
    /// at once (true today). If that ever changes, booting one instance
    /// would reset another's genuinely in-flight `sending` rows and permit a
    /// duplicate send of the same upload — revisit with e.g. a
    /// `pg_try_advisory_lock` around the boot reconcile before running
    /// multiple instances against one database.
    pub async fn reconcile_sending_media_uploads(&self) -> Result<u64, StoreError> {
        let result = sqlx_core::query::query(
            "UPDATE media_uploads SET state = 'staged' WHERE state = 'sending'",
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Delete staged rows whose `expires_at` has passed, returning their
    /// metadata so the caller can unlink the backing files (M15c, ADR 0059,
    /// GH #286): an abandoned staged upload (crash, lost network, abandoned
    /// compose) is never claimed, so nothing else ever removes it. Restricted
    /// to `state = 'staged'` — `expires_at` only ever gated *claiming*, so a
    /// `sending` row past its original `expires_at` is still an in-flight
    /// send and must not be deleted out from under `complete_upload`/
    /// `release_upload`.
    pub async fn delete_expired_media_uploads(&self) -> Result<Vec<MediaUpload>, StoreError> {
        let sql = format!(
            "DELETE FROM media_uploads \
             WHERE state = 'staged' AND expires_at <= now() \
             RETURNING {MEDIA_UPLOAD_COLUMNS}"
        );
        let rows = sqlx_core::query_as::query_as::<Postgres, MediaUpload>(&sql)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    /// Every `(account_id, upload_id)` pair with a live row, in any state.
    /// Feeds the boot orphan-file scan (M15c, ADR 0059, GH #286) — the same
    /// row-existence-keyed pattern as
    /// [`list_all_account_ids`](crate::Store::list_all_account_ids) feeding
    /// `axon-sync`'s `prune_orphan_store_dirs`/`prune_orphan_media_dirs`: a
    /// staged-upload file on disk with no id in this list has no row in any
    /// state and is safe to remove.
    pub async fn list_media_upload_ids(&self) -> Result<Vec<(Uuid, Uuid)>, StoreError> {
        let ids = sqlx_core::query::query("SELECT account_id, upload_id FROM media_uploads")
            .try_map(|row: PgRow| {
                let account_id: Uuid = row.try_get("account_id")?;
                let upload_id: Uuid = row.try_get("upload_id")?;
                Ok((account_id, upload_id))
            })
            .fetch_all(&self.pool)
            .await?;
        Ok(ids)
    }
}

async fn insert_upload(
    connection: &mut sqlx_postgres::PgConnection,
    upload: &NewMediaUpload<'_>,
    generated: bool,
) -> Result<MediaUpload, StoreError> {
    let sql = format!(
            "INSERT INTO media_uploads \
               (upload_id, account_id, kind, filename, content_type, size_bytes, path, expires_at, generated_preview) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             RETURNING {MEDIA_UPLOAD_COLUMNS}"
        );
    let row = sqlx_core::query_as::query_as::<Postgres, MediaUpload>(&sql)
        .bind(upload.upload_id)
        .bind(upload.account_id)
        .bind(upload.kind.as_str())
        .bind(upload.filename)
        .bind(upload.content_type)
        .bind(upload.size_bytes)
        .bind(upload.path)
        .bind(upload.expires_at)
        .bind(generated)
        .fetch_one(connection)
        .await?;
    Ok(row)
}

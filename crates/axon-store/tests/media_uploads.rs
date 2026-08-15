//! Integration tests for staged media-upload metadata (M15a, ADR 0059).
//!
//! Like the other store tests these need a running Postgres and are `#[ignore]`d
//! by default. Run them with:
//!
//! ```sh
//! docker compose up -d postgres
//! DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon cargo test -p axon-store --test media_uploads -- --ignored
//! ```

mod common;

use axon_store::{MediaUploadKind, MediaUploadState, NewMediaUpload};
use chrono::{Duration, Utc};
use common::test_account;
use sqlx_core::row::Row;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires Postgres"]
async fn insert_get_and_delete_staged_upload_metadata() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "media-upload").await;
    let upload_id = Uuid::new_v4();
    let expires_at = Utc::now() + Duration::hours(1);

    let inserted = store
        .insert_media_upload(&NewMediaUpload {
            upload_id,
            account_id,
            kind: MediaUploadKind::Image,
            filename: "photo.png",
            content_type: Some("image/png"),
            size_bytes: 3,
            path: "/tmp/photo",
            expires_at,
        })
        .await
        .expect("insert upload");

    assert_eq!(inserted.upload_id, upload_id);
    assert_eq!(inserted.account_id, account_id);
    assert_eq!(inserted.kind, MediaUploadKind::Image);
    assert_eq!(inserted.filename, "photo.png");
    assert_eq!(inserted.content_type.as_deref(), Some("image/png"));
    assert_eq!(inserted.size_bytes, 3);
    assert_eq!(inserted.path, "/tmp/photo");
    assert_eq!(inserted.state, MediaUploadState::Staged);

    let fetched = store
        .get_media_upload(account_id, upload_id)
        .await
        .expect("get upload")
        .expect("upload present");
    assert_eq!(fetched.upload_id, upload_id);

    let deleted = store
        .delete_staged_media_upload(account_id, upload_id)
        .await
        .expect("delete upload")
        .expect("deleted row");
    assert_eq!(deleted.path, "/tmp/photo");
    assert!(store
        .get_media_upload(account_id, upload_id)
        .await
        .expect("get after delete")
        .is_none());

    common::cleanup_account(&pool, account_id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn delete_staged_upload_does_not_delete_sending_rows() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "media-upload-sending").await;
    let upload_id = Uuid::new_v4();

    store
        .insert_media_upload(&NewMediaUpload {
            upload_id,
            account_id,
            kind: MediaUploadKind::File,
            filename: "file.bin",
            content_type: None,
            size_bytes: 10,
            path: "/tmp/file",
            expires_at: Utc::now() + Duration::hours(1),
        })
        .await
        .expect("insert upload");
    sqlx_core::query::query(
        "UPDATE media_uploads SET state = 'sending' WHERE account_id = $1 AND upload_id = $2",
    )
    .bind(account_id)
    .bind(upload_id)
    .execute(&pool)
    .await
    .expect("mark sending");

    assert!(
        store
            .delete_staged_media_upload(account_id, upload_id)
            .await
            .expect("delete sending")
            .is_none(),
        "sending rows are not deleted by the staged-only helper"
    );
    let row = sqlx_core::query::query(
        "SELECT state FROM media_uploads WHERE account_id = $1 AND upload_id = $2",
    )
    .bind(account_id)
    .bind(upload_id)
    .fetch_one(&pool)
    .await
    .expect("row remains");
    assert_eq!(row.get::<String, _>("state"), "sending");

    common::cleanup_account(&pool, account_id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn claim_release_and_complete_media_upload() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "media-upload-claim").await;
    let upload_id = Uuid::new_v4();

    store
        .insert_media_upload(&NewMediaUpload {
            upload_id,
            account_id,
            kind: MediaUploadKind::File,
            filename: "file.bin",
            content_type: Some("application/octet-stream"),
            size_bytes: 10,
            path: "/tmp/file",
            expires_at: Utc::now() + Duration::hours(1),
        })
        .await
        .expect("insert upload");

    let claimed = store
        .claim_staged_media_upload(account_id, upload_id)
        .await
        .expect("claim upload")
        .expect("claimed row");
    assert_eq!(claimed.state, MediaUploadState::Sending);
    assert!(
        store
            .claim_staged_media_upload(account_id, upload_id)
            .await
            .expect("second claim")
            .is_none(),
        "a sending row cannot be claimed again"
    );

    let released = store
        .release_sending_media_upload(account_id, upload_id)
        .await
        .expect("release upload")
        .expect("released row");
    assert_eq!(released.state, MediaUploadState::Staged);

    let claimed = store
        .claim_staged_media_upload(account_id, upload_id)
        .await
        .expect("claim after release")
        .expect("claimed row");
    assert_eq!(claimed.state, MediaUploadState::Sending);
    let completed = store
        .complete_sending_media_upload(account_id, upload_id)
        .await
        .expect("complete upload")
        .expect("completed row");
    assert_eq!(completed.path, "/tmp/file");
    assert!(
        store
            .get_media_upload(account_id, upload_id)
            .await
            .expect("get after complete")
            .is_none(),
        "completion consumes the row"
    );

    common::cleanup_account(&pool, account_id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn expired_media_upload_cannot_be_claimed() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "media-upload-expired").await;
    let upload_id = Uuid::new_v4();

    store
        .insert_media_upload(&NewMediaUpload {
            upload_id,
            account_id,
            kind: MediaUploadKind::Image,
            filename: "old.png",
            content_type: Some("image/png"),
            size_bytes: 10,
            path: "/tmp/old",
            expires_at: Utc::now() - Duration::minutes(1),
        })
        .await
        .expect("insert upload");

    assert!(
        store
            .claim_staged_media_upload(account_id, upload_id)
            .await
            .expect("claim expired")
            .is_none(),
        "expired rows are not claimable"
    );
    let row = store
        .get_media_upload(account_id, upload_id)
        .await
        .expect("get expired")
        .expect("row still present for M15c pruning");
    assert_eq!(row.state, MediaUploadState::Staged);

    common::cleanup_account(&pool, account_id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn account_delete_cascades_media_upload_rows() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "media-upload-cascade").await;
    let upload_id = Uuid::new_v4();

    store
        .insert_media_upload(&NewMediaUpload {
            upload_id,
            account_id,
            kind: MediaUploadKind::File,
            filename: "file.bin",
            content_type: None,
            size_bytes: 10,
            path: "/tmp/file",
            expires_at: Utc::now() + Duration::hours(1),
        })
        .await
        .expect("insert upload");
    common::cleanup_account(&pool, account_id).await;

    let count: i64 = sqlx_core::query_scalar::query_scalar(
        "SELECT COUNT(*) FROM media_uploads WHERE upload_id = $1",
    )
    .bind(upload_id)
    .fetch_one(&pool)
    .await
    .expect("count uploads");
    assert_eq!(count, 0);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn reconcile_sending_media_uploads_resets_stale_sending_rows() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "media-upload-reconcile").await;
    let sending_id = Uuid::new_v4();
    let staged_id = Uuid::new_v4();

    for (upload_id, filename) in [(sending_id, "sending.bin"), (staged_id, "staged.bin")] {
        store
            .insert_media_upload(&NewMediaUpload {
                upload_id,
                account_id,
                kind: MediaUploadKind::File,
                filename,
                content_type: None,
                size_bytes: 10,
                path: "/tmp/file",
                expires_at: Utc::now() + Duration::hours(1),
            })
            .await
            .expect("insert upload");
    }
    store
        .claim_staged_media_upload(account_id, sending_id)
        .await
        .expect("claim upload")
        .expect("claimed row");
    let staged_before = store
        .get_media_upload(account_id, staged_id)
        .await
        .expect("get staged upload before reconcile")
        .expect("staged row present");

    // The reconcile is process-wide, so parallel tests may contribute other
    // `sending` rows to its affected-row count. Assert this account's state
    // transitions below instead of depending on a database-global total.
    store
        .reconcile_sending_media_uploads()
        .await
        .expect("reconcile sending uploads");

    let reconciled = store
        .get_media_upload(account_id, sending_id)
        .await
        .expect("get reconciled upload")
        .expect("row still present");
    assert_eq!(reconciled.state, MediaUploadState::Staged);
    let untouched = store
        .get_media_upload(account_id, staged_id)
        .await
        .expect("get untouched upload")
        .expect("row still present");
    assert_eq!(untouched.state, MediaUploadState::Staged);
    assert_eq!(
        untouched.updated_at, staged_before.updated_at,
        "an already-staged row must not be touched by the sending-only reconcile"
    );

    common::cleanup_account(&pool, account_id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn delete_expired_media_uploads_prunes_staged_but_spares_sending() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "media-upload-sweep").await;
    let expired_staged_id = Uuid::new_v4();
    let expired_sending_id = Uuid::new_v4();
    let live_id = Uuid::new_v4();

    store
        .insert_media_upload(&NewMediaUpload {
            upload_id: expired_staged_id,
            account_id,
            kind: MediaUploadKind::Image,
            filename: "expired-staged.png",
            content_type: None,
            size_bytes: 10,
            path: "/tmp/expired-staged",
            expires_at: Utc::now() - Duration::minutes(1),
        })
        .await
        .expect("insert expired staged upload");
    store
        .insert_media_upload(&NewMediaUpload {
            upload_id: expired_sending_id,
            account_id,
            kind: MediaUploadKind::Image,
            filename: "expired-sending.png",
            content_type: None,
            size_bytes: 10,
            path: "/tmp/expired-sending",
            expires_at: Utc::now() - Duration::minutes(1),
        })
        .await
        .expect("insert expired sending upload");
    sqlx_core::query::query(
        "UPDATE media_uploads SET state = 'sending' WHERE account_id = $1 AND upload_id = $2",
    )
    .bind(account_id)
    .bind(expired_sending_id)
    .execute(&pool)
    .await
    .expect("mark sending");
    store
        .insert_media_upload(&NewMediaUpload {
            upload_id: live_id,
            account_id,
            kind: MediaUploadKind::Image,
            filename: "live.png",
            content_type: None,
            size_bytes: 10,
            path: "/tmp/live",
            expires_at: Utc::now() + Duration::hours(1),
        })
        .await
        .expect("insert live upload");

    let deleted = store
        .delete_expired_media_uploads()
        .await
        .expect("delete expired uploads");
    assert_eq!(deleted.len(), 1, "only the expired staged row is deleted");
    assert_eq!(deleted[0].upload_id, expired_staged_id);
    assert_eq!(deleted[0].path, "/tmp/expired-staged");

    assert!(
        store
            .get_media_upload(account_id, expired_staged_id)
            .await
            .expect("get expired staged")
            .is_none(),
        "expired staged row is gone"
    );
    assert!(
        store
            .get_media_upload(account_id, expired_sending_id)
            .await
            .expect("get expired sending")
            .is_some(),
        "expired-but-sending row survives the sweep"
    );
    assert!(
        store
            .get_media_upload(account_id, live_id)
            .await
            .expect("get live upload")
            .is_some(),
        "unexpired row survives the sweep"
    );

    common::cleanup_account(&pool, account_id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn list_media_upload_ids_includes_every_state() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "media-upload-ids").await;
    let staged_id = Uuid::new_v4();
    let sending_id = Uuid::new_v4();

    for upload_id in [staged_id, sending_id] {
        store
            .insert_media_upload(&NewMediaUpload {
                upload_id,
                account_id,
                kind: MediaUploadKind::File,
                filename: "file.bin",
                content_type: None,
                size_bytes: 10,
                path: "/tmp/file",
                expires_at: Utc::now() + Duration::hours(1),
            })
            .await
            .expect("insert upload");
    }
    store
        .claim_staged_media_upload(account_id, sending_id)
        .await
        .expect("claim upload")
        .expect("claimed row");

    let ids = store
        .list_media_upload_ids()
        .await
        .expect("list media upload ids");
    assert!(ids.contains(&(account_id, staged_id)), "staged row listed");
    assert!(
        ids.contains(&(account_id, sending_id)),
        "sending row listed"
    );

    common::cleanup_account(&pool, account_id).await;
}

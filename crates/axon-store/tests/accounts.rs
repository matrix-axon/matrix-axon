//! Integration tests for the account store methods.
//!
//! These require a running Postgres and are `#[ignore]`d by default so the
//! normal `cargo test` stays database-free. Run them with:
//!
//! ```sh
//! docker compose up -d postgres
//! DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon cargo test -p axon-store -- --ignored
//! ```

mod common;

use axon_store::{
    AccountAuthKind, AccountState, MatrixOAuthRegistration, StoreError, StoredAccountSession,
};
use sqlx_core::row::Row;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires Postgres"]
async fn upsert_is_idempotent_and_listable() {
    let store = common::migrated_store().await;
    // Unique user per run so repeated runs don't collide.
    let user = format!("@upsert-{}:localhost", Uuid::new_v4());
    let hs = "https://hs.example.org";

    let first = store.upsert_account(&user, hs).await.expect("first upsert");
    let second = store
        .upsert_account(&user, hs)
        .await
        .expect("second upsert");
    // Same logical account: stable primary key across calls.
    assert_eq!(first.account_id, second.account_id);

    let listed = store.list_accounts().await.expect("list");
    assert!(listed.iter().any(|a| a.account_id == first.account_id));

    common::cleanup_account(&common::raw_pool().await, first.account_id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn matrix_session_token_encrypts_and_round_trips() {
    let store = common::migrated_store().await;
    let user = format!("@session-{}:localhost", Uuid::new_v4());
    let account = store
        .upsert_account(&user, "https://hs.example.org")
        .await
        .expect("upsert");

    // No session yet.
    assert!(store
        .account_session(account.account_id, "key-A")
        .await
        .expect("token query")
        .is_none());

    store
        .set_account_matrix_session(account.account_id, "DEVICE42", "syt_secret", "key-A")
        .await
        .expect("set session");

    // Correct key round-trips.
    match store
        .account_session(account.account_id, "key-A")
        .await
        .expect("token query")
        .expect("session")
    {
        StoredAccountSession::Matrix { access_token } => {
            assert_eq!(access_token, "syt_secret")
        }
        StoredAccountSession::OAuth { .. } => panic!("expected Matrix session"),
    }

    // Device id was persisted.
    let listed = store.list_accounts().await.expect("list");
    let row = listed
        .iter()
        .find(|a| a.account_id == account.account_id)
        .expect("row");
    assert_eq!(row.device_id.as_deref(), Some("DEVICE42"));
    assert_eq!(row.auth_kind, AccountAuthKind::Matrix);

    // Wrong key fails to decrypt (pgcrypto raises an error).
    assert!(store
        .account_session(account.account_id, "wrong-key")
        .await
        .is_err());

    common::cleanup_account(&common::raw_pool().await, account.account_id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn oauth_session_rotates_atomically_and_can_return_to_matrix_auth() {
    let store = common::migrated_store().await;
    let user = format!("@oauth-session-{}:localhost", Uuid::new_v4());
    let account = store
        .upsert_account(&user, "https://hs.example.org")
        .await
        .expect("upsert");

    // Omitting the new columns exercises the migration defaults used by every
    // pre-ADR 0097 row.
    assert_eq!(account.auth_kind, AccountAuthKind::Matrix);

    store
        .set_account_oauth_session(
            account.account_id,
            "OAUTHDEVICE",
            "access-1",
            "refresh-1",
            "client-1",
            "key-A",
        )
        .await
        .expect("set OAuth session");
    store
        .update_account_oauth_session(
            account.account_id,
            "access-2",
            "refresh-2",
            "client-1",
            "key-A",
        )
        .await
        .expect("rotate OAuth session");

    let pool = common::raw_pool().await;
    let encrypted = sqlx_core::query::query(
        "SELECT access_token_encrypted, oauth_refresh_token_encrypted \
         FROM accounts WHERE account_id = $1",
    )
    .bind(account.account_id)
    .fetch_one(&pool)
    .await
    .expect("read ciphertext");
    let access_ciphertext: Vec<u8> = encrypted.get("access_token_encrypted");
    let refresh_ciphertext: Vec<u8> = encrypted.get("oauth_refresh_token_encrypted");
    assert!(!access_ciphertext
        .windows(b"access-2".len())
        .any(|window| window == b"access-2"));
    assert!(!refresh_ciphertext
        .windows(b"refresh-2".len())
        .any(|window| window == b"refresh-2"));

    match store
        .account_session(account.account_id, "key-A")
        .await
        .expect("read OAuth session")
        .expect("session")
    {
        StoredAccountSession::OAuth {
            access_token,
            refresh_token,
            client_id,
        } => {
            assert_eq!(access_token, "access-2");
            assert_eq!(refresh_token, "refresh-2");
            assert_eq!(client_id, "client-1");
        }
        StoredAccountSession::Matrix { .. } => panic!("expected OAuth session"),
    }

    // Race a later password login against a refresh write. Regardless of which
    // statement reaches Postgres first, the replacement is the final complete
    // session: a later refresh is rejected by `auth_kind`; an earlier refresh is
    // atomically overwritten together with both OAuth-only fields.
    let replace = store.set_account_matrix_session(
        account.account_id,
        "MATRIXDEVICE",
        "matrix-access",
        "key-A",
    );
    let stale_refresh = store.update_account_oauth_session(
        account.account_id,
        "stale-access",
        "stale-refresh",
        "client-1",
        "key-A",
    );
    let (replace_result, _refresh_result) = tokio::join!(replace, stale_refresh);
    replace_result.expect("replace with Matrix session");
    match store
        .account_session(account.account_id, "key-A")
        .await
        .expect("read replacement session")
        .expect("replacement session")
    {
        StoredAccountSession::Matrix { access_token } => {
            assert_eq!(access_token, "matrix-access")
        }
        StoredAccountSession::OAuth { .. } => panic!("stale OAuth rotation won lifecycle race"),
    }

    let stale_result = store
        .update_account_oauth_session(
            account.account_id,
            "later-stale-access",
            "later-stale-refresh",
            "client-1",
            "key-A",
        )
        .await;
    assert!(matches!(
        stale_result,
        Err(StoreError::OAuthSessionNotCurrent)
    ));

    common::cleanup_account(&common::raw_pool().await, account.account_id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn oauth_registration_is_shared_by_issuer() {
    let store = common::migrated_store().await;
    let issuer = format!("https://issuer-{}.example/", Uuid::new_v4());
    let registration = MatrixOAuthRegistration {
        issuer_url: issuer.clone(),
        homeserver_url: "https://hs.example.org/".to_owned(),
        client_id: "public-client".to_owned(),
    };
    store
        .upsert_matrix_oauth_registration(&registration)
        .await
        .expect("persist registration");

    assert_eq!(
        store
            .matrix_oauth_registration(&issuer)
            .await
            .expect("read registration"),
        Some(registration)
    );

    let pool = common::raw_pool().await;
    sqlx_core::query::query("DELETE FROM matrix_oauth_registrations WHERE issuer_url = $1")
        .bind(issuer)
        .execute(&pool)
        .await
        .expect("cleanup registration");
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn deactivation_drops_from_listing_but_retains_row() {
    let store = common::migrated_store().await;
    let user = format!("@state-{}:localhost", Uuid::new_v4());
    let account = store
        .upsert_account(&user, "https://hs.example.org")
        .await
        .expect("upsert");

    // New accounts default to active and unverified, and `list_accounts` (the
    // connect-path default) returns them.
    assert_eq!(account.state, AccountState::Active);
    assert!(!account.verified);
    assert!(store
        .list_accounts()
        .await
        .expect("list")
        .iter()
        .any(|a| a.account_id == account.account_id));

    // Deactivating drops the row from `list_accounts` — so the sync engine never
    // brings it online — while the row itself is retained (visible via
    // `get_account`, in its new state).
    store
        .set_account_state(account.account_id, AccountState::Deactivated)
        .await
        .expect("deactivate");
    assert!(!store
        .list_accounts()
        .await
        .expect("list")
        .iter()
        .any(|a| a.account_id == account.account_id));
    let retained = store
        .get_account(account.account_id)
        .await
        .expect("get")
        .expect("row retained");
    assert_eq!(retained.state, AccountState::Deactivated);

    // Reactivating returns it to the listing.
    store
        .set_account_state(account.account_id, AccountState::Active)
        .await
        .expect("reactivate");
    assert!(store
        .list_accounts()
        .await
        .expect("list")
        .iter()
        .any(|a| a.account_id == account.account_id));

    common::cleanup_account(&common::raw_pool().await, account.account_id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn set_account_verified_round_trips_orthogonal_to_state() {
    let store = common::migrated_store().await;
    let user = format!("@verified-{}:localhost", Uuid::new_v4());
    let account = store
        .upsert_account(&user, "https://hs.example.org")
        .await
        .expect("upsert");

    // A fresh row defaults to unverified (ADR 0026: the derivation writes it).
    assert!(!account.verified);

    // Flipping `verified` is a derived-value write, independent of lifecycle
    // `state` — the row stays `active`.
    store
        .set_account_verified(account.account_id, true)
        .await
        .expect("set verified");
    let after = store
        .get_account(account.account_id)
        .await
        .expect("get")
        .expect("row");
    assert!(after.verified);
    assert_eq!(after.state, AccountState::Active);

    // It is re-derivable both ways (not write-once): a later derivation can clear
    // it again, e.g. when cross-signing is reset.
    store
        .set_account_verified(account.account_id, false)
        .await
        .expect("clear verified");
    let cleared = store
        .get_account(account.account_id)
        .await
        .expect("get")
        .expect("row");
    assert!(!cleared.verified);

    common::cleanup_account(&common::raw_pool().await, account.account_id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn set_backup_enable_intent_round_trips_orthogonal_to_state() {
    let store = common::migrated_store().await;
    let user = format!("@backup-intent-{}:localhost", Uuid::new_v4());
    let account = store
        .upsert_account(&user, "https://hs.example.org")
        .await
        .expect("upsert");

    assert!(
        !account.backup_enable_intent,
        "fresh row defaults intent off (ADR 0098)"
    );

    store
        .set_backup_enable_intent(account.account_id, true)
        .await
        .expect("set intent");
    let after = store
        .get_account(account.account_id)
        .await
        .expect("get")
        .expect("row");
    assert!(after.backup_enable_intent);
    assert_eq!(after.state, AccountState::Active);

    store
        .set_backup_enable_intent(account.account_id, false)
        .await
        .expect("clear intent");
    let cleared = store
        .get_account(account.account_id)
        .await
        .expect("get")
        .expect("row");
    assert!(!cleared.backup_enable_intent);

    common::cleanup_account(&common::raw_pool().await, account.account_id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn delete_removes_row_and_is_idempotent() {
    let store = common::migrated_store().await;
    let user = format!("@delete-{}:localhost", Uuid::new_v4());
    let account = store
        .upsert_account(&user, "https://hs.example.org")
        .await
        .expect("upsert");

    store
        .delete_account_row(account.account_id)
        .await
        .expect("delete");
    assert!(store
        .get_account(account.account_id)
        .await
        .expect("get")
        .is_none());

    // Idempotent: deleting an already-gone row affects zero rows, not an error —
    // the property the boot reconcile and a client retry both lean on.
    store
        .delete_account_row(account.account_id)
        .await
        .expect("second delete is a no-op");
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn deleting_rows_listed_only_by_the_reconcile_accessor() {
    let store = common::migrated_store().await;
    let user = format!("@deleting-{}:localhost", Uuid::new_v4());
    let account = store
        .upsert_account(&user, "https://hs.example.org")
        .await
        .expect("upsert");
    store
        .set_account_state(account.account_id, AccountState::Deleting)
        .await
        .expect("mark deleting");

    // A `deleting` row is invisible to both the connect-path and client-facing
    // listings, but the reconcile accessor finds it (and a by-id read still does).
    let id = account.account_id;
    assert!(!store
        .list_accounts()
        .await
        .expect("list")
        .iter()
        .any(|a| a.account_id == id));
    assert!(!store
        .list_client_visible_accounts()
        .await
        .expect("list")
        .iter()
        .any(|a| a.account_id == id));
    assert!(store
        .list_deleting_accounts()
        .await
        .expect("list deleting")
        .iter()
        .any(|a| a.account_id == id));
    // It is a real row in any state, so the orphan-GC's existence check sees it.
    assert!(store
        .list_all_account_ids()
        .await
        .expect("all ids")
        .contains(&id));

    common::cleanup_account(&common::raw_pool().await, id).await;
}

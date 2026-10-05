//! The lockout guard and the sign-in time step-up depends on (ADR 0109).
//!
//! Run against a disposable Postgres database, serially: the guard counts
//! every credential on the instance, so these tests clear the credential
//! tables to control what it sees.
mod common;

use axon_store::{IdentityRemoval, Store};
use chrono::{Duration, Utc};
use common::{migrated_store, raw_pool};
use sqlx_postgres::PgPool;
use uuid::Uuid;

async fn clear_credentials(pool: &PgPool) {
    for sql in [
        "DELETE FROM oauth_authorization_requests",
        "DELETE FROM oauth_bind_requests",
        "DELETE FROM oauth_refresh_tokens",
        "DELETE FROM tokens",
        "DELETE FROM oauth_identities",
    ] {
        sqlx_core::query::query(sql).execute(pool).await.unwrap();
    }
}

async fn bind(store: &Store, provider: &str) -> Uuid {
    store
        .bind_identity(provider, &Uuid::new_v4().to_string(), None)
        .await
        .unwrap()
        .id
}

fn providers(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn the_last_credential_is_not_removed_and_a_refusal_changes_nothing() {
    let store = migrated_store().await;
    clear_credentials(&raw_pool().await).await;
    let apple = bind(&store, "apple").await;
    // An hour of access left is not a way back in: it is never a survivor.
    let session = store
        .issue_oauth_token(
            "session",
            Utc::now() + Duration::hours(1),
            "apple",
            apple,
            "web",
            Some(Utc::now()),
        )
        .await
        .unwrap();

    // The identity must not count itself: the guard looks at what is left.
    assert_eq!(
        store
            .delete_identity_unless_last_credential(apple, &providers(&["apple"]))
            .await
            .unwrap(),
        IdentityRemoval::LastCredential
    );
    assert!(store.find_identity_by_id(apple).await.unwrap().is_some());
    assert!(
        store.verify_token(&session.token).await.unwrap().is_some(),
        "a refused unbind must not have revoked the identity's sessions"
    );

    assert_eq!(
        store
            .delete_identity_unless_last_credential(Uuid::new_v4(), &providers(&["apple"]))
            .await
            .unwrap(),
        IdentityRemoval::NotFound
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_non_expiring_token_or_a_usable_identity_is_a_survivor() {
    let store = migrated_store().await;
    let pool = raw_pool().await;

    // A non-expiring token survives the unbind.
    clear_credentials(&pool).await;
    let apple = bind(&store, "apple").await;
    let cli = store.issue_token("laptop").await.unwrap();
    assert_eq!(
        store
            .delete_identity_unless_last_credential(apple, &[])
            .await
            .unwrap(),
        IdentityRemoval::Removed
    );

    // A revoked one does not.
    let apple = bind(&store, "apple").await;
    store.revoke_token(cli.id).await.unwrap();
    assert_eq!(
        store
            .delete_identity_unless_last_credential(apple, &providers(&["apple"]))
            .await
            .unwrap(),
        IdentityRemoval::LastCredential
    );

    // Another identity survives only if its provider can still sign in.
    let google = bind(&store, "google").await;
    assert_eq!(
        store
            .delete_identity_unless_last_credential(apple, &providers(&["apple"]))
            .await
            .unwrap(),
        IdentityRemoval::LastCredential,
        "an identity whose provider is switched off cannot produce a session"
    );
    assert_eq!(
        store
            .delete_identity_unless_last_credential(apple, &providers(&["apple", "google"]))
            .await
            .unwrap(),
        IdentityRemoval::Removed
    );
    assert!(store.find_identity_by_id(google).await.unwrap().is_some());
}

/// Two requests each removing one of the last two credentials. Under
/// `READ COMMITTED` and no shared lock, each counts the other as the survivor
/// and both commit. Repeated, because one lucky interleaving proves nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Postgres"]
async fn concurrent_removals_cannot_each_count_the_other_as_the_survivor() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let usable = providers(&["apple", "google"]);
    for round in 0..25 {
        clear_credentials(&pool).await;
        let apple = bind(&store, "apple").await;
        let google = bind(&store, "google").await;
        let (a, b) = tokio::join!(
            tokio::spawn({
                let (store, usable) = (store.clone(), usable.clone());
                async move {
                    store
                        .delete_identity_unless_last_credential(apple, &usable)
                        .await
                }
            }),
            tokio::spawn({
                let (store, usable) = (store.clone(), usable.clone());
                async move {
                    store
                        .delete_identity_unless_last_credential(google, &usable)
                        .await
                }
            }),
        );
        let mut outcomes = [a.unwrap().unwrap(), b.unwrap().unwrap()];
        outcomes.sort_by_key(|outcome| *outcome == IdentityRemoval::LastCredential);
        assert_eq!(
            outcomes,
            [IdentityRemoval::Removed, IdentityRemoval::LastCredential],
            "round {round}: exactly one removal may go through"
        );
        assert_eq!(store.list_identities().await.unwrap().len(), 1);
    }
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn the_sign_in_time_survives_refresh_rotation_unchanged() {
    let store = migrated_store().await;
    let identity = bind(&store, "google").await;
    let signed_in = Utc::now() - Duration::days(3);
    let first = format!("hash-{}", Uuid::new_v4());
    store
        .issue_refresh_token(
            &first,
            identity,
            "web",
            Utc::now() + Duration::days(30),
            Some(signed_in),
        )
        .await
        .unwrap();

    // Rotate twice: a refresh must never move the time forward.
    let mut presented = first;
    for _ in 0..2 {
        let next = format!("hash-{}", Uuid::new_v4());
        let rotated = store
            .redeem_refresh_token(
                &presented,
                Uuid::new_v4(),
                &next,
                Utc::now() + Duration::days(30),
            )
            .await
            .unwrap()
            .unwrap();
        // Postgres keeps microseconds; compare at that precision.
        assert_eq!(
            rotated.authenticated_at.map(|at| at.timestamp_micros()),
            Some(signed_in.timestamp_micros())
        );
        presented = next;
    }

    let access = store
        .issue_oauth_token(
            "refreshed",
            Utc::now() + Duration::hours(1),
            "google",
            identity,
            "web",
            Some(signed_in),
        )
        .await
        .unwrap();
    let verified = store.verify_token(&access.token).await.unwrap().unwrap();
    assert_eq!(verified.id, access.id);
    assert_eq!(verified.oauth_identity_id, Some(identity));
    assert!(verified.expires_at.is_some());
    assert_eq!(
        verified.authenticated_at.map(|at| at.timestamp_micros()),
        Some(signed_in.timestamp_micros())
    );
    store.delete_identity(identity).await.unwrap();
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_chain_with_no_recorded_sign_in_stays_without_one() {
    let store = migrated_store().await;
    let identity = bind(&store, "google").await;
    let first = format!("hash-{}", Uuid::new_v4());
    store
        .issue_refresh_token(
            &first,
            identity,
            "web",
            Utc::now() + Duration::days(30),
            None,
        )
        .await
        .unwrap();
    let rotated = store
        .redeem_refresh_token(
            &first,
            Uuid::new_v4(),
            &format!("hash-{}", Uuid::new_v4()),
            Utc::now() + Duration::days(30),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rotated.authenticated_at, None);

    // A token minted on the command line has no sign-in and never expires.
    let cli = store.issue_token("cli").await.unwrap();
    let verified = store.verify_token(&cli.token).await.unwrap().unwrap();
    assert_eq!(verified.expires_at, None);
    assert_eq!(verified.authenticated_at, None);
    assert_eq!(verified.oauth_identity_id, None);
    store.revoke_token(cli.id).await.unwrap();
    store.delete_identity(identity).await.unwrap();
}
